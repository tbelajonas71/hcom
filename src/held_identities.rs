//! Held identities: seats that are away, not gone.
//!
//! A seat loses its `instances` row for reasons that say nothing about whether
//! it still exists: the stale reaper after an hour of quiet, a desktop session
//! ending, a PTY exiting, or, for a seat on another device, a relay sync gap of
//! more than 90 seconds. Sends to such a seat were refused as "non-existent or
//! stopped", and a desktop chat that carried on was re-created with its cursor
//! at the newest event, so anything sent while it was away was skipped.
//!
//! For `HOLD_SECS` after an automatic stop a seat stays addressable. A message
//! to it is logged like any other and reaches it when it binds again, because
//! re-creation restores the cursor saved when it stopped. A stop someone asked
//! for (`hcom stop`, `hcom kill`) is not held, and neither are subagents.

use serde_json::Value;

use crate::db::HcomDb;

/// How long an automatically stopped seat stays addressable.
pub(crate) const HOLD_SECS: f64 = 7.0 * 86400.0;

/// `by` values of stops the system made on its own.
const AUTOMATIC_STOPPERS: &[&str] = &["system", "session", "pty"];

const REMOTE_KEY_PREFIX: &str = "relay_offline_";

/// Stop reason recorded when someone retires a held seat.
const RETIRED_REASON: &str = "retired";

fn remote_key(device_id: &str) -> String {
    format!("{REMOTE_KEY_PREFIX}{device_id}")
}

fn parse_iso_epoch(ts: &str) -> Option<f64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|dt| dt.timestamp_millis() as f64 / 1000.0)
}

/// The automatic stop that holds `name`, if its latest life event is one and
/// it is recent enough: `(stopped_at_epoch, snapshot)`.
fn holding_stop(db: &HcomDb, name: &str, now: f64) -> Option<(f64, Value)> {
    let (ts, data): (String, String) = db
        .conn()
        .query_row(
            "SELECT timestamp, data FROM events
             WHERE type = 'life' AND instance = ?1
             ORDER BY id DESC LIMIT 1",
            rusqlite::params![name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok()?;
    let data: Value = serde_json::from_str(&data).ok()?;
    if data.get("action").and_then(|v| v.as_str()) != Some("stopped") {
        return None;
    }
    let by = data.get("by").and_then(|v| v.as_str()).unwrap_or("");
    let reason = data.get("reason").and_then(|v| v.as_str()).unwrap_or("");
    if !AUTOMATIC_STOPPERS.contains(&by) || reason == RETIRED_REASON {
        return None;
    }
    let snapshot = data.get("snapshot").cloned().unwrap_or(Value::Null);
    if snapshot
        .get("parent_name")
        .and_then(|v| v.as_str())
        .is_some_and(|p| !p.is_empty())
    {
        return None;
    }
    let stopped_at = parse_iso_epoch(&ts)?;
    if now - stopped_at > HOLD_SECS {
        return None;
    }
    Some((stopped_at, snapshot))
}

fn has_row(db: &HcomDb, name: &str) -> bool {
    db.conn()
        .query_row(
            "SELECT 1 FROM instances WHERE name = ?1 COLLATE NOCASE",
            rusqlite::params![name],
            |_| Ok(()),
        )
        .is_ok()
}

/// Local seats that are held: no row now, latest life event an automatic stop
/// within the hold window. Returned with the seconds since they stopped.
pub(crate) fn local_held(db: &HcomDb, now: f64) -> Vec<(String, f64)> {
    let names: Vec<String> = db
        .conn()
        .prepare(
            "SELECT DISTINCT instance FROM events
             WHERE type = 'life' AND instance NOT LIKE '%:%' AND instance NOT LIKE '\\_%' ESCAPE '\\'
               AND json_extract(data, '$.action') = 'stopped'
               AND NOT EXISTS (SELECT 1 FROM instances i WHERE i.name = events.instance)",
        )
        .ok()
        .map(|mut stmt| {
            stmt.query_map([], |row| row.get::<_, String>(0))
                .ok()
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    names
        .into_iter()
        .filter_map(|name| {
            holding_stop(db, &name, now).map(|(stopped_at, _)| (name, now - stopped_at))
        })
        .collect()
}

/// Cursor a re-created seat should resume from: the one saved when it was
/// automatically stopped, if that stop still holds it.
pub(crate) fn held_cursor(db: &HcomDb, name: &str, now: f64) -> Option<i64> {
    let (_, snapshot) = holding_stop(db, name, now)?;
    snapshot
        .get("last_event_id")
        .and_then(|v| v.as_i64())
        .filter(|id| *id > 0)
}

/// Record remote seats (namespaced `name:SHORT`) whose rows are about to be
/// removed, so they stay addressable while their device is away.
pub(crate) fn remember_remote(db: &HcomDb, device_id: &str, names: &[String], now: f64) {
    if names.is_empty() {
        return;
    }
    let key = remote_key(device_id);
    let mut map: serde_json::Map<String, Value> = db
        .kv_get(&key)
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    for name in names {
        map.insert(name.clone(), Value::from(now));
    }
    map.retain(|_, seen| seen.as_f64().is_some_and(|t| now - t <= HOLD_SECS));
    let _ = db.kv_set(&key, Some(&Value::Object(map).to_string()));
}

/// Remote seats that are held: recorded when their rows went, within the hold
/// window, and not back as a live row. With seconds since they were last seen.
pub(crate) fn remote_held(db: &HcomDb, now: f64) -> Vec<(String, f64)> {
    let mut out = Vec::new();
    for (_, raw) in db.kv_prefix(REMOTE_KEY_PREFIX).unwrap_or_default() {
        let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        for (name, seen) in map {
            let Some(seen) = seen.as_f64() else { continue };
            if now - seen <= HOLD_SECS && !has_row(db, &name) {
                out.push((name, now - seen));
            }
        }
    }
    out
}

/// End the hold on `name` because someone said the seat is gone for good.
/// Returns false when `name` is not held.
pub(crate) fn retire(db: &HcomDb, name: &str, by: &str, now: f64) -> bool {
    if holding_stop(db, name, now).is_some() {
        return db
            .log_life_event(name, "stopped", by, RETIRED_REASON, None)
            .is_ok();
    }
    let mut retired = false;
    for (key, raw) in db.kv_prefix(REMOTE_KEY_PREFIX).unwrap_or_default() {
        let Ok(Value::Object(mut map)) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        let before = map.len();
        map.retain(|held, _| !held.eq_ignore_ascii_case(name));
        if map.len() != before {
            retired = true;
            let _ = db.kv_set(&key, Some(&Value::Object(map).to_string()));
        }
    }
    retired
}

/// Every held seat, local and remote, with seconds since it went away.
pub(crate) fn all_held(db: &HcomDb, now: f64) -> Vec<(String, f64)> {
    let mut held = local_held(db, now);
    held.extend(remote_held(db, now));
    held.sort_by(|a, b| a.0.cmp(&b.0));
    held.dedup_by(|a, b| a.0 == b.0);
    held
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_helpers::isolated_test_env;
    use serde_json::json;
    use serial_test::serial;

    fn stop(db: &HcomDb, name: &str, by: &str, last_event_id: i64, parent: Option<&str>) {
        db.log_life_event(
            name,
            "stopped",
            by,
            "test",
            Some(json!({"last_event_id": last_event_id, "parent_name": parent})),
        )
        .unwrap();
    }

    fn now() -> f64 {
        crate::shared::time::now_epoch_f64()
    }

    #[test]
    #[serial]
    fn automatic_stops_are_held_and_deliberate_ones_are_not() {
        let (_dir, _hcom_dir, _home, _guard) = isolated_test_env();
        let db = HcomDb::open().unwrap();
        stop(&db, "reaped", "system", 40, None);
        stop(&db, "closed", "session", 41, None);
        stop(&db, "killed", "HCC-LendPC", 42, None);
        stop(&db, "helper", "system", 43, Some("reaped"));

        let names: Vec<String> = local_held(&db, now()).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.contains(&"reaped".to_string()));
        assert!(names.contains(&"closed".to_string()));
        assert_eq!(held_cursor(&db, "reaped", now()), Some(40));
        assert_eq!(held_cursor(&db, "killed", now()), None);
    }

    #[test]
    #[serial]
    fn a_hold_expires_and_a_live_row_ends_it() {
        let (_dir, _hcom_dir, _home, _guard) = isolated_test_env();
        let db = HcomDb::open().unwrap();
        stop(&db, "luna", "system", 40, None);
        assert_eq!(local_held(&db, now() + HOLD_SECS + 60.0), vec![]);

        db.conn()
            .execute(
                "INSERT INTO instances (name, created_at, last_event_id) VALUES ('luna', 1.0, 0)",
                [],
            )
            .unwrap();
        assert_eq!(local_held(&db, now()), vec![]);
    }

    #[test]
    #[serial]
    fn a_later_life_event_replaces_the_hold() {
        let (_dir, _hcom_dir, _home, _guard) = isolated_test_env();
        let db = HcomDb::open().unwrap();
        stop(&db, "luna", "system", 40, None);
        stop(&db, "luna", "HCC-LendPC", 50, None);
        assert_eq!(local_held(&db, now()), vec![]);
        assert_eq!(held_cursor(&db, "luna", now()), None);
    }

    #[test]
    #[serial]
    fn retiring_ends_a_local_or_remote_hold() {
        let (_dir, _hcom_dir, _home, _guard) = isolated_test_env();
        let db = HcomDb::open().unwrap();
        stop(&db, "luna", "system", 40, None);
        remember_remote(&db, "dev-1", &["nova:ABCD".to_string()], now());

        assert!(retire(&db, "luna", "HCC-LendPC", now()));
        assert!(retire(&db, "nova:ABCD", "HCC-LendPC", now()));
        assert!(!retire(&db, "nobody", "HCC-LendPC", now()));
        assert!(all_held(&db, now()).is_empty());
        assert_eq!(held_cursor(&db, "luna", now()), None);
    }

    #[test]
    #[serial]
    fn a_retirement_by_the_system_does_not_restart_the_hold() {
        let (_dir, _hcom_dir, _home, _guard) = isolated_test_env();
        let db = HcomDb::open().unwrap();
        stop(&db, "luna", "system", 40, None);
        assert!(retire(&db, "luna", "system", now()));
        assert!(local_held(&db, now()).is_empty());
    }

    #[test]
    #[serial]
    fn remote_seats_are_remembered_until_they_return_or_expire() {
        let (_dir, _hcom_dir, _home, _guard) = isolated_test_env();
        let db = HcomDb::open().unwrap();
        let t = 1_000_000.0;
        remember_remote(&db, "dev-1", &["luna:ABCD".to_string()], t);
        remember_remote(&db, "dev-1", &["nova:ABCD".to_string()], t + 10.0);

        let mut held = remote_held(&db, t + 20.0);
        held.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            held,
            vec![
                ("luna:ABCD".to_string(), 20.0),
                ("nova:ABCD".to_string(), 10.0)
            ]
        );
        assert!(remote_held(&db, t + HOLD_SECS + 11.0).is_empty());

        db.conn()
            .execute(
                "INSERT INTO instances (name, origin_device_id, created_at, last_event_id)
                 VALUES ('luna:ABCD', 'dev-1', 1.0, 0)",
                [],
            )
            .unwrap();
        let names: Vec<String> = remote_held(&db, t + 20.0)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names, vec!["nova:ABCD".to_string()]);
    }
}
