//! Instance lifecycle state machine and launch failure handling.

use std::process::Command;
use std::sync::Mutex;
use std::time::Instant;

use crate::db::{HcomDb, InstanceRow};
use crate::shared::time::{now_epoch_f64, now_epoch_i64};
use crate::shared::{ST_ACTIVE, ST_BLOCKED, ST_INACTIVE, ST_LAUNCHING, ST_LISTENING};

/// Parameters for `set_status` beyond the core name/status/context triplet.
#[derive(Debug, Default)]
pub struct StatusUpdate<'a> {
    pub detail: &'a str,
    pub msg_ts: &'a str,
    /// Tool-reported name of the tool call active when this write happened
    /// (e.g. "Bash", "Edit"). Empty when not applicable/available.
    pub tool_name: &'a str,
    /// Tool-reported id of the tool call active when this write happened
    /// (Claude's `tool_use_id`). Empty when not applicable/available.
    pub tool_use_id: &'a str,
}

/// Max time between instance creation and session binding before launch is considered failed.
pub const LAUNCH_PLACEHOLDER_TIMEOUT: i64 = 30;

/// Heartbeat timeout with active TCP listener (PTY, hooks with notify).
/// 35s = 30s hook polling interval + 5s buffer.
pub const HEARTBEAT_THRESHOLD_TCP: i64 = 35;

/// Heartbeat timeout without TCP listener (adhoc instances).
pub const HEARTBEAT_THRESHOLD_NO_TCP: i64 = 10;

/// Heartbeat age when last_stop is missing (marker for unreliable data).
pub const UNKNOWN_HEARTBEAT_AGE: i64 = 999999;

/// Max time without status update before marking inactive (5 min).
pub const STATUS_ACTIVITY_TIMEOUT: i64 = 300;

/// How long placeholder instances can exist before cleanup (2 min).
pub const CLEANUP_PLACEHOLDER_THRESHOLD: i64 = 120;

/// Grace period after sleep/wake before resuming stale cleanup (60s).
pub const WAKE_GRACE_PERIOD: f64 = 60.0;

/// Remote device stale threshold (90s without push).
const REMOTE_DEVICE_STALE_THRESHOLD: f64 = 90.0;

/// Window for showing recently stopped instances (10 minutes).
pub const RECENTLY_STOPPED_WINDOW: f64 = 600.0;

/// Return type for `get_instance_status()` with structured status metadata.
#[derive(Debug, Clone)]
pub struct ComputedStatus {
    pub status: String,
    pub age_string: String,
    pub description: String,
    pub age_seconds: i64,
    /// Simple context key (e.g., "stale", "killed", "timeout").
    pub context: String,
}

pub use crate::shared::time::format_age;

/// Whether this row represents a bound Claude session that can only receive
/// messages at hook boundaries (for example, Claude Desktop).
///
/// The session binding is the durable ownership signal. Hard stops delete it,
/// and soft stops clear it, so a non-null `instances.session_id` alone is not
/// sufficient. A process binding means the PTY delivery path owns the session.
pub(crate) fn is_hook_only_claude_session(data: &InstanceRow, db: &HcomDb) -> bool {
    if data.tool != "claude"
        || data
            .origin_device_id
            .as_deref()
            .is_some_and(|device_id| !device_id.is_empty())
        || db.has_process_binding_for_instance(&data.name)
    {
        return false;
    }

    data.session_id.as_deref().is_some_and(|session_id| {
        matches!(
            db.get_session_binding(session_id),
            Ok(Some(owner)) if owner == data.name
        )
    })
}

// Tracks wall-clock vs monotonic-clock drift to detect system sleep.
// On macOS, Instant (mach_absolute_time) does not advance during sleep,
// but SystemTime (gettimeofday) does. Large drift means the system just woke.
struct WakeState {
    last_mono: Option<Instant>,
    last_wall: f64,
    grace_until_mono: Option<Instant>,
}

static WAKE_STATE: Mutex<WakeState> = Mutex::new(WakeState {
    last_mono: None,
    last_wall: 0.0,
    grace_until_mono: None,
});

/// Detect sleep/wake via wall-vs-monotonic drift and report whether grace is active.
pub fn is_in_wake_grace() -> bool {
    is_in_wake_grace_with_persistence(None)
}

/// Wake-grace detection with optional DB persistence for short-lived processes.
pub fn is_in_wake_grace_with_persistence(db: Option<&crate::db::HcomDb>) -> bool {
    let now_mono = Instant::now();
    let now_wall = now_epoch_f64();

    let mut state = match WAKE_STATE.lock() {
        Ok(s) => s,
        Err(_) => return false,
    };

    if state.last_mono.is_none()
        && let Some(db) = db
        && let Ok(Some(persisted_wall)) = db.kv_get("_wake_last_wall")
        && let Ok(last_wall) = persisted_wall.parse::<f64>()
    {
        let wall_elapsed = now_wall - last_wall;
        if wall_elapsed > 30.0 && wall_elapsed < 3600.0 {
            crate::log::log_info(
                "cleanup",
                "sleep_wake_detected",
                &format!(
                    "drift={:.0}s (cross-process), grace={:.0}s",
                    wall_elapsed, WAKE_GRACE_PERIOD
                ),
            );
            state.grace_until_mono =
                Some(now_mono + std::time::Duration::from_secs_f64(WAKE_GRACE_PERIOD));
        }
        if let Ok(Some(grace_until)) = db.kv_get("_wake_grace_until")
            && let Ok(grace_wall) = grace_until.parse::<f64>()
            && now_wall < grace_wall
        {
            let remaining = grace_wall - now_wall;
            state.grace_until_mono = Some(now_mono + std::time::Duration::from_secs_f64(remaining));
        }
    }

    if let Some(last_mono) = state.last_mono {
        let mono_elapsed = now_mono.duration_since(last_mono).as_secs_f64();
        let wall_elapsed = now_wall - state.last_wall;
        let drift = wall_elapsed - mono_elapsed;

        if drift > 30.0 {
            crate::log::log_info(
                "cleanup",
                "sleep_wake_detected",
                &format!("drift={:.0}s, grace={:.0}s", drift, WAKE_GRACE_PERIOD),
            );
            let grace_deadline = now_mono + std::time::Duration::from_secs_f64(WAKE_GRACE_PERIOD);
            state.grace_until_mono = Some(grace_deadline);

            if let Some(db) = db {
                let grace_wall = now_wall + WAKE_GRACE_PERIOD;
                let _ = db.kv_set("_wake_grace_until", Some(&grace_wall.to_string()));
            }
        }
    }

    state.last_mono = Some(now_mono);
    state.last_wall = now_wall;

    if let Some(db) = db {
        let _ = db.kv_set("_wake_last_wall", Some(&now_wall.to_string()));
    }

    match state.grace_until_mono {
        Some(deadline) => now_mono < deadline,
        None => false,
    }
}

/// Compute the current status from stored fields and heartbeat.
pub fn get_instance_status(data: &InstanceRow, db: &HcomDb) -> ComputedStatus {
    let status = &data.status;
    let status_time = data.status_time;
    let status_context = &data.status_context;
    let wake_grace = is_in_wake_grace();
    let now = now_epoch_i64();

    if crate::instances::is_launching_placeholder(data) {
        let created_at = data.created_at as i64;
        let age = if created_at > 0 { now - created_at } else { 0 };
        if age < LAUNCH_PLACEHOLDER_TIMEOUT {
            return ComputedStatus {
                status: ST_LAUNCHING.to_string(),
                age_string: if age > 0 {
                    format_age(age)
                } else {
                    String::new()
                },
                description: "launching".to_string(),
                age_seconds: age,
                context: "new".to_string(),
            };
        }

        let detail = match get_or_finalize_launch_failure_detail(db, data) {
            Some(detail) => detail,
            None => {
                // The finalizer is a current-generation CAS. A miss commonly
                // means the hook bound or advanced this row after the caller's
                // list snapshot. Render the current row instead of reporting a
                // launch failure that is true only of the stale snapshot.
                if let Ok(Some(current)) = db.get_instance_full(&data.name)
                    && (current.created_at != data.created_at
                        || !crate::instances::is_launching_placeholder(&current))
                {
                    return get_instance_status(&current, db);
                }
                extract_launch_failure_detail(data).unwrap_or_else(|| {
                    "launch probably failed - check logs or hcom list -v".to_string()
                })
            }
        };
        return ComputedStatus {
            status: ST_INACTIVE.to_string(),
            age_string: format_age(age),
            description: detail,
            age_seconds: age,
            context: "launch_failed".to_string(),
        };
    }

    let mut current_status = status.to_string();
    let mut current_context = status_context.to_string();
    let mut age = if status_time > 0 {
        now - status_time
    } else {
        0
    };
    if status_time == 0 {
        let created_at = data.created_at as i64;
        if created_at > 0 {
            age = now - created_at;
        }
    }

    if current_status == ST_LISTENING {
        let last_stop = data.last_stop;
        let is_remote = data.origin_device_id.is_some();

        if is_remote {
            age = 0;
        } else {
            let heartbeat_age = if last_stop > 0 {
                now - last_stop
            } else if status_time > 0 {
                now - status_time
            } else {
                UNKNOWN_HEARTBEAT_AGE
            };

            let has_tcp = data.tcp_mode != 0 || db.has_notify_endpoint(&data.name);
            let threshold = if has_tcp {
                HEARTBEAT_THRESHOLD_TCP
            } else {
                HEARTBEAT_THRESHOLD_NO_TCP
            };

            if heartbeat_age > threshold {
                if wake_grace {
                    age = 0;
                } else {
                    current_status = ST_INACTIVE.to_string();
                    current_context = "stale:listening".to_string();
                    age = heartbeat_age;
                }
            } else {
                age = 0;
            }
        }
    } else if current_status != ST_INACTIVE {
        let status_age = if status_time > 0 {
            now - status_time
        } else {
            let created_at = data.created_at as i64;
            if created_at > 0 { now - created_at } else { 0 }
        };

        if status_age > STATUS_ACTIVITY_TIMEOUT && data.origin_device_id.is_none() {
            let last_stop = data.last_stop;
            if last_stop > 0 && (now - last_stop) < HEARTBEAT_THRESHOLD_TCP {
                // Fresh heartbeat means the process is alive even if the status is old.
            } else if wake_grace {
                // Grace: heartbeat should refresh after wake.
            } else {
                let prev = current_status.clone();
                current_status = ST_INACTIVE.to_string();
                current_context = format!("stale:{prev}");
                age = status_age;
            }
        }
    }

    let description = get_status_description(&current_status, &current_context);
    let description = if data.tool == "adhoc" && current_status == ST_INACTIVE {
        if let Some(rest) = description.strip_prefix("inactive: ") {
            rest.to_string()
        } else if description == "inactive" {
            String::new()
        } else {
            description
        }
    } else {
        description
    };

    let simple_context = if current_context.contains(':') {
        let (prefix, suffix) = current_context.split_once(':').unwrap();
        if prefix == "exit" {
            suffix.to_string()
        } else {
            prefix.to_string()
        }
    } else {
        current_context.clone()
    };

    ComputedStatus {
        status: current_status,
        age_string: format_age(age),
        description,
        age_seconds: age,
        context: simple_context,
    }
}

pub(crate) fn get_or_finalize_launch_failure_detail(
    db: &HcomDb,
    data: &InstanceRow,
) -> Option<String> {
    finalize_launch_failure_detail(db, data, None)
}

pub(crate) fn get_launch_blocker_detail(data: &InstanceRow) -> Option<String> {
    extract_launch_failure_detail(data)
}

pub(crate) fn finalize_launch_failure_detail(
    db: &HcomDb,
    data: &InstanceRow,
    fallback_detail: Option<&str>,
) -> Option<String> {
    if data.status_context == "launch_failed" && !data.status_detail.is_empty() {
        return Some(data.status_detail.clone());
    }

    if !crate::instances::is_launching_placeholder(data) {
        return if data.status_context == "launch_failed" {
            extract_launch_failure_detail(data)
                .or_else(|| fallback_detail.map(ToString::to_string))
                .or_else(|| (!data.status_detail.is_empty()).then(|| data.status_detail.clone()))
        } else {
            None
        };
    }

    if fallback_detail.is_none() {
        let created_at = data.created_at as i64;
        let age = if created_at > 0 {
            now_epoch_i64() - created_at
        } else {
            0
        };
        if age < LAUNCH_PLACEHOLDER_TIMEOUT {
            return None;
        }
    }

    let created_at = data.created_at as i64;
    let age = if created_at > 0 {
        (now_epoch_i64() - created_at).max(0)
    } else {
        0
    };
    // Name what the pid actually is. For a background launch this is the
    // wrapper shell hcom spawned, not the tool: the tool is its grandchild, and
    // a wrapper that is alive says nothing about whether the tool ever started.
    // The old wording ("process alive Ns, never bound") read as "the tool is
    // running but won't bind" and sent a Windows launch-chain stall investigation
    // after the tool instead of the chain.
    let process_state = data.pid.and_then(|pid| {
        let alive = crate::sys::process::is_alive(pid as u32);
        let what = if data.background != 0 {
            "launcher process"
        } else {
            "process"
        };
        alive.then(|| format!("{what} (pid {pid}) alive {age}s, never bound"))
    });
    let mut detail = fallback_detail
        .map(ToString::to_string)
        .or(process_state)
        .unwrap_or_else(|| format!("exited before binding (observed after {age}s)"));
    if !detail.contains("PTY output:")
        && let Some(evidence) = extract_launch_failure_detail(data)
        && !detail.contains(&evidence)
    {
        detail.push('\n');
        detail.push_str(&evidence);
    }

    // A status/list process can hold this snapshot while a hook binds the
    // session (or while the name is deleted and recreated). Re-check the
    // current generation under the write lock, and commit the state change and
    // its event together so a stale observer cannot overwrite the binding.
    let status_time = now_epoch_i64();
    let timestamp = crate::db::chrono_now_iso();
    let expected_created_at = data.created_at;
    let committed = db.with_immediate_transaction(|txn| {
        let updated = txn.execute(
            "UPDATE instances
             SET status = ?1, status_time = ?2,
                 status_context = 'launch_failed', status_detail = ?3
             WHERE name = ?4 AND created_at = ?5
               AND (session_id IS NULL OR session_id = '')
               AND status_context = 'new'
               AND status IN (?1, 'pending')",
            rusqlite::params![
                ST_INACTIVE,
                status_time,
                &detail,
                &data.name,
                expected_created_at
            ],
        )?;
        if updated == 0 {
            return Ok(None);
        }

        let position: i64 = txn
            .query_row(
                "SELECT last_event_id FROM instances
                 WHERE name = ?1 AND created_at = ?2",
                rusqlite::params![&data.name, expected_created_at],
                |row| row.get::<_, Option<i64>>(0),
            )?
            .unwrap_or(0);
        let mut event_data = serde_json::json!({
            "status": ST_INACTIVE,
            "context": "launch_failed",
            "position": position,
            "detail": detail.clone(),
        });
        if detail.is_empty() {
            event_data.as_object_mut().map(|obj| obj.remove("detail"));
        }
        let event_json = serde_json::to_string(&event_data)?;
        txn.execute(
            "INSERT INTO events (timestamp, type, instance, data)
             VALUES (?1, 'status', ?2, ?3)",
            rusqlite::params![&timestamp, &data.name, event_json],
        )?;
        Ok(Some((txn.last_insert_rowid(), event_data)))
    });

    match committed {
        Ok(Some((event_id, event_data))) => {
            crate::db::subscriptions::process_logged_event(
                db,
                event_id,
                "status",
                &data.name,
                &event_data,
            );
            Some(detail)
        }
        Ok(None) => None,
        Err(error) => {
            crate::log::log_warn(
                "lifecycle",
                "launch_failure_finalize_failed",
                &format!("{}: {error}", data.name),
            );
            None
        }
    }
}

fn extract_launch_failure_detail(data: &InstanceRow) -> Option<String> {
    if !data.background_log_file.is_empty()
        && let Some(tail) = read_launch_log_tail(&data.background_log_file)
    {
        return Some(format!("PTY output:\n{tail}"));
    }

    let info = crate::terminal::resolve_terminal_info(
        data.terminal_preset_effective.as_deref(),
        data.launch_context.as_deref(),
    );

    match info.preset_name.as_str() {
        "tmux" | "tmux-split" => capture_tmux_launch_failure(&info.pane_id, &data.tool),
        _ => None,
    }
}

fn read_launch_log_tail(path: &str) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut lines: Vec<&str> = content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.is_empty() {
        return None;
    }
    if lines.len() > 8 {
        lines = lines.split_off(lines.len() - 8);
    }
    let mut tail = lines.join("\n");
    if tail.chars().count() > 1000 {
        tail = tail.chars().rev().take(1000).collect::<String>();
        tail = tail.chars().rev().collect();
        tail.insert_str(0, "...");
    }
    Some(tail)
}

fn capture_tmux_launch_failure(pane_id: &str, tool: &str) -> Option<String> {
    if pane_id.is_empty() {
        return None;
    }

    let output = Command::new("tmux")
        .args(["capture-pane", "-p", "-t", pane_id])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    parse_tmux_launch_failure_output(&String::from_utf8_lossy(&output.stdout), tool)
}

fn add_tmux_server_remediation(detail: &str) -> String {
    if !detail.contains("Operation not permitted") {
        return detail.to_string();
    }
    format!(
        "{detail} Fully reset tmux first (`tmux kill-server`), then start a fresh tmux server with approval/escalation (for example: `tmux new-session -d -s hcom-external`), then retry."
    )
}

fn parse_tmux_launch_failure_output(captured: &str, _tool: &str) -> Option<String> {
    let mut warning: Option<String> = None;

    for line in captured.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("Error:") {
            return Some(add_tmux_server_remediation(trimmed));
        }
        if warning.is_none() && trimmed.starts_with("WARNING:") {
            warning = Some(add_tmux_server_remediation(trimmed));
        }
    }

    warning
}

/// Build a human-readable status description from status and context tokens.
pub fn get_status_description(status: &str, context: &str) -> String {
    match status {
        ST_ACTIVE => {
            if let Some(sender) = context.strip_prefix("deliver:") {
                format!("active: msg from {sender}")
            } else if let Some(tool) = context.strip_prefix("tool:") {
                format!("active: {tool}")
            } else if let Some(tool) = context.strip_prefix("approved:") {
                format!("active: approved {tool}")
            } else if let Some(tool) = context.strip_prefix("denied:") {
                format!("active: denied {tool}")
            } else if context == "resuming" {
                "resuming...".to_string()
            } else if context.is_empty() {
                "active".to_string()
            } else {
                format!("active: {context}")
            }
        }
        ST_LISTENING => {
            if context == "tui:not-ready" {
                "listening: blocked".to_string()
            } else if context == "tui:not-idle" {
                "listening: waiting for idle".to_string()
            } else if context == "tui:user-active" {
                "listening: user typing".to_string()
            } else if context == "tui:output-unstable" {
                "listening: output streaming".to_string()
            } else if context == "tui:prompt-has-text" {
                "listening: uncommitted text".to_string()
            } else if let Some(reason) = context.strip_prefix("tui:") {
                format!("listening: {}", reason.replace('-', " "))
            } else if context == "suspended" {
                "listening: suspended".to_string()
            } else {
                "listening".to_string()
            }
        }
        ST_BLOCKED => {
            if context == "pty:approval" || context == "approval" {
                "blocked: approval pending".to_string()
            } else if context.is_empty() {
                "blocked: permission needed".to_string()
            } else {
                format!("blocked: {context}")
            }
        }
        ST_INACTIVE => {
            if context.starts_with("stale:") {
                "inactive: stale".to_string()
            } else if let Some(reason) = context.strip_prefix("exit:") {
                format!("inactive: {reason}")
            } else if context == "subagent:dormant" {
                "inactive: dormant subagent".to_string()
            } else if context == "unknown" {
                "inactive: unknown".to_string()
            } else if context.is_empty() {
                "inactive".to_string()
            } else {
                format!("inactive: {context}")
            }
        }
        _ => "unknown".to_string(),
    }
}

/// Set instance status with timestamp and log the status-change event.
#[track_caller]
pub fn set_status(
    db: &HcomDb,
    instance_name: &str,
    status: &str,
    context: &str,
    upd: StatusUpdate<'_>,
) {
    let StatusUpdate {
        detail,
        msg_ts,
        tool_name,
        tool_use_id,
    } = upd;
    let writer = std::panic::Location::caller();

    let current_data = match db.get_instance_full(instance_name) {
        Ok(data) => data,
        Err(e) => {
            eprintln!("[hcom] warn: set_status DB read failed for {instance_name}: {e}");
            None
        }
    };
    let now = now_epoch_i64();
    let mut updates = serde_json::Map::new();
    updates.insert("status".into(), serde_json::json!(status));
    updates.insert("status_time".into(), serde_json::json!(now));
    updates.insert("status_context".into(), serde_json::json!(context));
    updates.insert("status_detail".into(), serde_json::json!(detail));

    if status == ST_LISTENING {
        updates.insert("last_stop".into(), serde_json::json!(now));
    }

    let old_status = current_data.as_ref().map(|d| d.status.as_str());
    let status_changed = old_status != Some(status);
    let status_event_changed = current_data.as_ref().is_none_or(|d| {
        d.status != status || d.status_context != context || d.status_detail != detail
    });

    crate::instances::update_instance_position(db, instance_name, &updates);

    if status_changed {
        crate::notify::wake(db, instance_name, crate::notify::WakeKind::DELIVERY_LOOPS);
    }

    // The pi-family plugins (pi, and its fork omp) structurally double-write tool
    // status: the extension's tool_call handler calls reportStatus (omp/pi-status)
    // AND the Rust beforetool hook calls update_tool_status, both with the same
    // tool:<name>+detail. Suppress the redundant unchanged event for this family so
    // it doesn't emit duplicate status events (~30% of events for omp otherwise).
    let is_pi_family = matches!(
        current_data.as_ref().map(|d| d.tool.as_str()),
        Some("pi") | Some("omp")
    );
    if is_pi_family && !status_event_changed && msg_ts.is_empty() {
        return;
    }

    let position = current_data.as_ref().map(|d| d.last_event_id).unwrap_or(0);
    let mut data = serde_json::json!({
        "status": status,
        "context": context,
        "position": position,
    });
    if !detail.is_empty() {
        data["detail"] = serde_json::json!(detail);
    }
    if !msg_ts.is_empty() {
        data["msg_ts"] = serde_json::json!(msg_ts);
    }
    // old_* differs from the prior status event when set_gate_status() touched
    // the row without logging (tui:* gate context churns silently).
    data["old_status"] = serde_json::json!(old_status);
    data["old_context"] =
        serde_json::json!(current_data.as_ref().map(|d| d.status_context.as_str()));
    data["old_detail"] = serde_json::json!(current_data.as_ref().map(|d| d.status_detail.as_str()));
    data["new_status"] = serde_json::json!(status);
    data["new_context"] = serde_json::json!(context);
    data["new_detail"] = serde_json::json!(detail);
    data["writer"] = serde_json::json!(format!("{}:{}", writer.file(), writer.line()));
    if let Some(session_id) = current_data.as_ref().and_then(|d| d.session_id.as_deref()) {
        data["session"] = serde_json::json!(session_id);
    }
    if let Some(agent_id) = current_data.as_ref().and_then(|d| d.agent_id.as_deref()) {
        data["agent_id"] = serde_json::json!(agent_id);
    }
    if !tool_name.is_empty() {
        data["tool_name"] = serde_json::json!(tool_name);
    }
    if !tool_use_id.is_empty() {
        data["tool_use_id"] = serde_json::json!(tool_use_id);
    }
    let _ = db.log_event("status", instance_name, &data);
}

/// Delete placeholder instances that have been launching too long.
pub fn cleanup_stale_placeholders(db: &HcomDb) -> i32 {
    let mut deleted = 0;
    let now = now_epoch_f64();

    if let Ok(instances) = db.iter_instances_full() {
        for data in &instances {
            if !crate::instances::is_launching_placeholder(data) {
                continue;
            }
            let created_at = data.created_at;
            if created_at > 0.0
                && (now - created_at) > CLEANUP_PLACEHOLDER_THRESHOLD as f64
                && crate::hooks::common::stop_placeholder_instance(
                    db,
                    &data.name,
                    data.created_at,
                    "system",
                    "stale_cleanup",
                ) == crate::hooks::common::StopOutcome::Stopped
            {
                deleted += 1;
            }
        }
    }
    deleted
}

/// Delete instances that have been inactive too long.
/// Three tiers: exit contexts (1 min), stale (1 hr), other inactive (12 hr).
pub fn cleanup_stale_instances(
    db: &HcomDb,
    max_stale_seconds: i64,
    max_inactive_seconds: i64,
) -> i32 {
    if is_in_wake_grace() {
        return 0;
    }

    cleanup_stale_remote_instances(db);

    let mut deleted = 0;

    if let Ok(instances) = db.iter_instances_full() {
        for data in &instances {
            let computed = get_instance_status(data, db);

            if computed.status != ST_INACTIVE {
                continue;
            }

            // Silence between app hook boundaries is not a session stop.
            // It can be stored as inactive/exit:timeout OR computed as stale
            // from an old active/listening/blocked row. Retain the exact
            // hook-only binding and pending queue for both cases. Do not
            // refresh timestamps or report the session alive: computed status
            // stays stale. Explicit stops/end and non-hook ownership continue
            // through the existing cleanup paths.
            if is_hook_only_claude_session(data, db)
                && (computed.context == "stale"
                    || (data.status == ST_INACTIVE && data.status_context == "exit:timeout"))
            {
                continue;
            }

            let context = &computed.context;
            let age = computed.age_seconds;

            if matches!(
                context.as_str(),
                "killed" | "closed" | "timeout" | "interrupted" | "session_switch"
            ) && age > 60
            {
                crate::hooks::common::stop_instance(db, &data.name, "system", "exit_cleanup");
                deleted += 1;
                return deleted;
            }

            if context == "stale" && max_stale_seconds > 0 && age > max_stale_seconds {
                crate::hooks::common::stop_instance(db, &data.name, "system", "stale_cleanup");
                deleted += 1;
                return deleted;
            }

            if max_inactive_seconds > 0 && age > max_inactive_seconds {
                crate::hooks::common::stop_instance(db, &data.name, "system", "inactive_cleanup");
                deleted += 1;
                return deleted;
            }
        }
    }

    deleted
}

fn cleanup_stale_remote_instances(db: &HcomDb) {
    let now = now_epoch_f64();
    let sync_map: std::collections::HashMap<String, String> = db
        .kv_prefix("relay_sync_time_")
        .unwrap_or_default()
        .into_iter()
        .collect();

    if let Ok(instances) = db.iter_instances_full() {
        let device_ids: std::collections::HashSet<String> = instances
            .iter()
            .filter_map(|d| d.origin_device_id.clone())
            .collect();

        for device_id in device_ids {
            let sync_val = sync_map.get(&format!("relay_sync_time_{device_id}"));
            let sync_time: f64 = sync_val.and_then(|s| s.parse().ok()).unwrap_or(0.0);
            if sync_time > 0.0 && (now - sync_time) <= REMOTE_DEVICE_STALE_THRESHOLD {
                continue;
            }
            if let Err(e) = db.conn().execute(
                "DELETE FROM instances WHERE origin_device_id = ?",
                rusqlite::params![device_id],
            ) {
                crate::log::log_warn("cleanup", "remote_stale_cleanup_fail", &e.to_string());
            } else {
                crate::log::log_info(
                    "cleanup",
                    "remote_device_stale",
                    crate::relay::device_id_prefix(&device_id),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn setup_test_db() -> (HcomDb, PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let temp_dir = std::env::temp_dir();
        let test_id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let db_path = temp_dir.join(format!(
            "test_instance_lifecycle_{}_{}.db",
            std::process::id(),
            test_id
        ));

        let db = HcomDb::open_at(&db_path).unwrap();
        (db, db_path)
    }

    fn cleanup(path: PathBuf) {
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }

    fn default_instance() -> InstanceRow {
        InstanceRow {
            name: String::new(),
            session_id: None,
            parent_session_id: None,
            parent_name: None,
            agent_id: None,
            tag: None,
            last_event_id: 0,
            last_stop: 0,
            status: ST_INACTIVE.into(),
            status_time: 0,
            last_seen: 0,
            status_context: String::new(),
            status_detail: String::new(),
            directory: String::new(),
            created_at: 0.0,
            transcript_path: String::new(),
            tool: "claude".into(),
            background: 0,
            background_log_file: String::new(),
            tcp_mode: 0,
            wait_timeout: None,
            subagent_timeout: None,
            hints: None,
            origin_device_id: None,
            pid: None,
            launch_args: None,
            terminal_preset_requested: None,
            terminal_preset_effective: None,
            launch_context: None,
            name_announced: 0,
            idle_since: None,
        }
    }

    fn launch_failed_status_event_count(db: &HcomDb, name: &str) -> i64 {
        db.conn()
            .query_row(
                "SELECT COUNT(*) FROM events
                 WHERE type = 'status' AND instance = ?1
                   AND json_extract(data, '$.context') = 'launch_failed'",
                rusqlite::params![name],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn test_status_launching_new() {
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let data = InstanceRow {
            name: "test".into(),
            status: ST_INACTIVE.into(),
            status_context: "new".into(),
            created_at: now as f64,
            ..default_instance()
        };

        let result = get_instance_status(&data, &db);
        assert_eq!(result.status, ST_LAUNCHING);
        assert_eq!(result.context, "new");
        cleanup(path);
    }

    #[test]
    fn test_status_launch_failed() {
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let data = InstanceRow {
            name: "test".into(),
            status: ST_INACTIVE.into(),
            status_context: "new".into(),
            created_at: (now - LAUNCH_PLACEHOLDER_TIMEOUT - 1) as f64,
            ..default_instance()
        };

        let result = get_instance_status(&data, &db);
        assert_eq!(result.status, ST_INACTIVE);
        assert_eq!(result.context, "launch_failed");
        cleanup(path);
    }

    fn assert_pi_family_skips_duplicate(tool: &str) {
        let (db, path) = setup_test_db();
        let mut row = serde_json::Map::new();
        row.insert("name".into(), serde_json::json!("luna"));
        row.insert("tool".into(), serde_json::json!(tool));
        row.insert("status".into(), serde_json::json!(ST_ACTIVE));
        row.insert("status_context".into(), serde_json::json!("tool:bash"));
        row.insert("status_detail".into(), serde_json::json!("echo hi"));
        row.insert("status_time".into(), serde_json::json!(1));
        row.insert("last_stop".into(), serde_json::json!(0));
        row.insert("created_at".into(), serde_json::json!(1.0));
        db.save_instance_named("luna", &row).unwrap();

        // Two identical unchanged writes (reportStatus + beforetool) → one event.
        set_status(
            &db,
            "luna",
            ST_ACTIVE,
            "tool:bash",
            StatusUpdate {
                detail: "ls -la",
                ..Default::default()
            },
        );
        set_status(
            &db,
            "luna",
            ST_ACTIVE,
            "tool:bash",
            StatusUpdate {
                detail: "ls -la",
                ..Default::default()
            },
        );
        let event_count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE type = 'status' AND instance = 'luna'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            event_count, 1,
            "pi-family tool {tool} should dedup identical status events"
        );
        cleanup(path);
    }

    #[test]
    fn test_set_status_dedup_covers_pi_family() {
        assert_pi_family_skips_duplicate("pi");
        assert_pi_family_skips_duplicate("omp");
    }

    #[test]
    fn test_set_status_skips_duplicate_status_events_but_refreshes_heartbeat() {
        let (db, path) = setup_test_db();
        let mut row = serde_json::Map::new();
        row.insert("name".into(), serde_json::json!("luna"));
        row.insert("tool".into(), serde_json::json!("pi"));
        row.insert("status".into(), serde_json::json!(ST_ACTIVE));
        row.insert("status_context".into(), serde_json::json!("prompt"));
        row.insert("status_detail".into(), serde_json::json!(""));
        row.insert("status_time".into(), serde_json::json!(1));
        row.insert("last_stop".into(), serde_json::json!(0));
        row.insert("created_at".into(), serde_json::json!(1.0));
        db.save_instance_named("luna", &row).unwrap();

        set_status(&db, "luna", ST_LISTENING, "", Default::default());
        let event_count_after_change: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE type = 'status' AND instance = 'luna'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(event_count_after_change, 1);
        let first_last_stop: i64 = db.get_instance_full("luna").unwrap().unwrap().last_stop;

        set_status(&db, "luna", ST_LISTENING, "", Default::default());
        let event_count_after_duplicate: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE type = 'status' AND instance = 'luna'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(event_count_after_duplicate, 1);
        let refreshed_last_stop: i64 = db.get_instance_full("luna").unwrap().unwrap().last_stop;
        assert!(refreshed_last_stop >= first_last_stop);

        cleanup(path);
    }

    #[test]
    fn test_set_status_logs_duplicate_status_events_for_non_pi_tools() {
        let (db, path) = setup_test_db();
        let mut row = serde_json::Map::new();
        row.insert("name".into(), serde_json::json!("luna"));
        row.insert("tool".into(), serde_json::json!("claude"));
        row.insert("status".into(), serde_json::json!(ST_LISTENING));
        row.insert("status_context".into(), serde_json::json!(""));
        row.insert("status_detail".into(), serde_json::json!(""));
        row.insert("status_time".into(), serde_json::json!(1));
        row.insert("last_stop".into(), serde_json::json!(0));
        row.insert("created_at".into(), serde_json::json!(1.0));
        db.save_instance_named("luna", &row).unwrap();

        set_status(&db, "luna", ST_LISTENING, "", Default::default());
        set_status(&db, "luna", ST_LISTENING, "", Default::default());

        let event_count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE type = 'status' AND instance = 'luna'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(event_count, 2);

        cleanup(path);
    }

    #[test]
    fn test_finalize_launch_failure_detail_uses_fallback() {
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let mut row = serde_json::Map::new();
        row.insert("name".into(), serde_json::json!("test"));
        row.insert("status".into(), serde_json::json!(ST_INACTIVE));
        row.insert("status_context".into(), serde_json::json!("new"));
        row.insert(
            "created_at".into(),
            serde_json::json!((now - LAUNCH_PLACEHOLDER_TIMEOUT - 1) as f64),
        );
        row.insert("status_time".into(), serde_json::json!(0));
        row.insert("tool".into(), serde_json::json!("codex"));
        db.save_instance_named("test", &row).unwrap();

        let data = InstanceRow {
            name: "test".into(),
            status: ST_INACTIVE.into(),
            status_context: "new".into(),
            created_at: (now - LAUNCH_PLACEHOLDER_TIMEOUT - 1) as f64,
            ..default_instance()
        };

        let detail = finalize_launch_failure_detail(
            &db,
            &data,
            Some("process exited before startup completed (exit code 1)"),
        );
        assert_eq!(
            detail.as_deref(),
            Some("process exited before startup completed (exit code 1)")
        );

        let stored = db.get_instance_full("test").unwrap().unwrap();
        assert_eq!(stored.status_context, "launch_failed");
        assert_eq!(
            stored.status_detail,
            "process exited before startup completed (exit code 1)"
        );
        assert_eq!(launch_failed_status_event_count(&db, "test"), 1);

        assert_eq!(
            finalize_launch_failure_detail(
                &db,
                &data,
                Some("process exited before startup completed (exit code 1)"),
            ),
            None,
            "a stale placeholder snapshot must not finalize the row twice"
        );
        assert_eq!(launch_failed_status_event_count(&db, "test"), 1);
        cleanup(path);
    }

    #[test]
    fn test_bound_new_instance_never_becomes_launch_failed() {
        let (db, path) = setup_test_db();
        let created_at = (now_epoch_i64() - LAUNCH_PLACEHOLDER_TIMEOUT - 1) as f64;

        let mut row = serde_json::Map::new();
        row.insert("name".into(), serde_json::json!("test"));
        row.insert("session_id".into(), serde_json::json!("thread-test"));
        row.insert("status".into(), serde_json::json!(ST_INACTIVE));
        row.insert("status_context".into(), serde_json::json!("new"));
        row.insert("created_at".into(), serde_json::json!(created_at));
        row.insert("status_time".into(), serde_json::json!(0));
        row.insert("tool".into(), serde_json::json!("codex"));
        db.save_instance_named("test", &row).unwrap();
        db.set_session_binding("thread-test", "test").unwrap();

        let data = db.get_instance_full("test").unwrap().unwrap();
        let computed = get_instance_status(&data, &db);
        assert_eq!(computed.status, ST_INACTIVE);
        assert_eq!(computed.context, "new");
        assert_ne!(computed.context, "launch_failed");

        let stored = db.get_instance_full("test").unwrap().unwrap();
        assert_eq!(stored.session_id.as_deref(), Some("thread-test"));
        assert_eq!(stored.status_context, "new");
        assert_eq!(
            db.get_session_binding("thread-test").unwrap().as_deref(),
            Some("test")
        );
        assert_eq!(launch_failed_status_event_count(&db, "test"), 0);
        cleanup(path);
    }

    #[test]
    fn test_stale_placeholder_snapshot_cannot_overwrite_session_binding() {
        let (db, path) = setup_test_db();
        let created_at = (now_epoch_i64() - LAUNCH_PLACEHOLDER_TIMEOUT - 1) as f64;

        let mut row = serde_json::Map::new();
        row.insert("name".into(), serde_json::json!("test"));
        row.insert("status".into(), serde_json::json!(ST_INACTIVE));
        row.insert("status_context".into(), serde_json::json!("new"));
        row.insert("created_at".into(), serde_json::json!(created_at));
        row.insert("status_time".into(), serde_json::json!(0));
        row.insert("tool".into(), serde_json::json!("codex"));
        db.save_instance_named("test", &row).unwrap();
        let stale = db.get_instance_full("test").unwrap().unwrap();

        let mut updates = serde_json::Map::new();
        updates.insert("session_id".into(), serde_json::json!("thread-test"));
        db.update_instance_fields("test", &updates).unwrap();
        db.set_session_binding("thread-test", "test").unwrap();

        assert_eq!(finalize_launch_failure_detail(&db, &stale, None), None);
        let computed = get_instance_status(&stale, &db);
        assert_eq!(computed.status, ST_INACTIVE);
        assert_eq!(computed.context, "new");
        let stored = db.get_instance_full("test").unwrap().unwrap();
        assert_eq!(stored.session_id.as_deref(), Some("thread-test"));
        assert_eq!(stored.status_context, "new");
        assert_eq!(
            db.get_session_binding("thread-test").unwrap().as_deref(),
            Some("test")
        );
        assert_eq!(launch_failed_status_event_count(&db, "test"), 0);
        cleanup(path);
    }

    #[test]
    fn test_stale_placeholder_snapshot_cannot_overwrite_new_generation() {
        let (db, path) = setup_test_db();
        let old_created_at = (now_epoch_i64() - LAUNCH_PLACEHOLDER_TIMEOUT - 2) as f64;
        let new_created_at = old_created_at + 1.0;

        let mut row = serde_json::Map::new();
        row.insert("name".into(), serde_json::json!("test"));
        row.insert("status".into(), serde_json::json!(ST_INACTIVE));
        row.insert("status_context".into(), serde_json::json!("new"));
        row.insert("created_at".into(), serde_json::json!(old_created_at));
        row.insert("status_time".into(), serde_json::json!(0));
        row.insert("tool".into(), serde_json::json!("codex"));
        db.save_instance_named("test", &row).unwrap();
        let stale = db.get_instance_full("test").unwrap().unwrap();

        db.delete_instance("test").unwrap();
        row.insert("created_at".into(), serde_json::json!(new_created_at));
        db.save_instance_named("test", &row).unwrap();

        assert_eq!(finalize_launch_failure_detail(&db, &stale, None), None);
        let stored = db.get_instance_full("test").unwrap().unwrap();
        assert_eq!(stored.created_at, new_created_at);
        assert_eq!(stored.status_context, "new");
        assert_eq!(launch_failed_status_event_count(&db, "test"), 0);
        cleanup(path);
    }

    #[test]
    fn test_finalize_launch_failure_detail_leaves_fresh_placeholder_launching() {
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let mut row = serde_json::Map::new();
        row.insert("name".into(), serde_json::json!("test"));
        row.insert("status".into(), serde_json::json!(ST_INACTIVE));
        row.insert("status_context".into(), serde_json::json!("new"));
        row.insert("created_at".into(), serde_json::json!(now as f64));
        row.insert("status_time".into(), serde_json::json!(0));
        row.insert("tool".into(), serde_json::json!("codex"));
        db.save_instance_named("test", &row).unwrap();

        let data = InstanceRow {
            name: "test".into(),
            status: ST_INACTIVE.into(),
            status_context: "new".into(),
            created_at: now as f64,
            ..default_instance()
        };

        let detail = finalize_launch_failure_detail(&db, &data, None);
        assert_eq!(detail, None);

        let stored = db.get_instance_full("test").unwrap().unwrap();
        assert_eq!(stored.status_context, "new");
        cleanup(path);
    }

    #[test]
    fn test_parse_tmux_launch_failure_output_prefers_error() {
        let captured = "\
Starting Codex...
WARNING: proceeding, even though we could not update PATH: Operation not permitted (os error 1)
Error: Operation not permitted (os error 1)
";

        let result = parse_tmux_launch_failure_output(captured, "codex");
        assert_eq!(
            result.as_deref(),
            Some(
                "Error: Operation not permitted (os error 1) Fully reset tmux first (`tmux kill-server`), then start a fresh tmux server with approval/escalation (for example: `tmux new-session -d -s hcom-external`), then retry."
            )
        );
    }

    #[test]
    fn test_parse_tmux_launch_failure_output_falls_back_to_warning() {
        let captured = "\
Starting Codex...
WARNING: proceeding, even though we could not update PATH: Operation not permitted (os error 1)
";

        let result = parse_tmux_launch_failure_output(captured, "codex");
        assert_eq!(
            result.as_deref(),
            Some(
                "WARNING: proceeding, even though we could not update PATH: Operation not permitted (os error 1) Fully reset tmux first (`tmux kill-server`), then start a fresh tmux server with approval/escalation (for example: `tmux new-session -d -s hcom-external`), then retry."
            )
        );
    }

    #[test]
    fn test_status_listening_fresh_heartbeat() {
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let data = InstanceRow {
            name: "test".into(),
            status: ST_LISTENING.into(),
            status_time: now - 5,
            last_stop: now - 2,
            tcp_mode: 1,
            ..default_instance()
        };

        let result = get_instance_status(&data, &db);
        assert_eq!(result.status, ST_LISTENING);
        assert_eq!(result.age_string, "now");
        cleanup(path);
    }

    #[test]
    fn test_status_listening_stale_heartbeat() {
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let data = InstanceRow {
            name: "test".into(),
            status: ST_LISTENING.into(),
            status_time: now - 100,
            last_stop: now - 100,
            tcp_mode: 1,
            ..default_instance()
        };

        let result = get_instance_status(&data, &db);
        assert_eq!(result.status, ST_INACTIVE);
        assert!(
            result.context.starts_with("stale"),
            "context should be stale, got: {}",
            result.context
        );
        cleanup(path);
    }

    #[test]
    fn test_status_active_stale_activity() {
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let data = InstanceRow {
            name: "test".into(),
            status: ST_ACTIVE.into(),
            status_context: "tool:Bash".into(),
            status_time: now - STATUS_ACTIVITY_TIMEOUT - 10,
            last_stop: 0,
            ..default_instance()
        };

        let result = get_instance_status(&data, &db);
        assert_eq!(result.status, ST_INACTIVE);
        assert!(result.context.starts_with("stale"));
        cleanup(path);
    }

    #[test]
    fn test_status_remote_instance_trusted() {
        let (db, path) = setup_test_db();
        let now = now_epoch_i64();

        let data = InstanceRow {
            name: "test".into(),
            status: ST_LISTENING.into(),
            status_time: now - 100,
            last_stop: 0,
            origin_device_id: Some("device-abc".into()),
            ..default_instance()
        };

        let result = get_instance_status(&data, &db);
        assert_eq!(result.status, ST_LISTENING);
        cleanup(path);
    }

    #[test]
    fn test_status_descriptions() {
        assert_eq!(
            get_status_description(ST_ACTIVE, "tool:Bash"),
            "active: Bash"
        );
        assert_eq!(
            get_status_description(ST_ACTIVE, "deliver:luna"),
            "active: msg from luna"
        );
        assert_eq!(get_status_description(ST_ACTIVE, ""), "active");
        assert_eq!(get_status_description(ST_LISTENING, ""), "listening");
        assert_eq!(
            get_status_description(ST_LISTENING, "tui:not-ready"),
            "listening: blocked"
        );
        assert_eq!(
            get_status_description(ST_BLOCKED, ""),
            "blocked: permission needed"
        );
        assert_eq!(
            get_status_description(ST_INACTIVE, "stale:listening"),
            "inactive: stale"
        );
        assert_eq!(
            get_status_description(ST_INACTIVE, "exit:timeout"),
            "inactive: timeout"
        );
    }

    #[test]
    fn test_cleanup_stale_placeholders_deletes_old() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();

        let old_time = now_epoch_f64() - 200.0;
        let mut data = serde_json::Map::new();
        data.insert("name".into(), serde_json::json!("stale"));
        data.insert("status".into(), serde_json::json!("pending"));
        data.insert("status_context".into(), serde_json::json!("new"));
        data.insert("created_at".into(), serde_json::json!(old_time));
        db.save_instance_named("stale", &data).unwrap();

        let deleted = cleanup_stale_placeholders(&db);
        assert_eq!(deleted, 1);
        assert!(db.get_instance_full("stale").unwrap().is_none());
        let placeholder: i64 = db
            .conn()
            .query_row(
                "SELECT COALESCE(json_extract(data, '$.placeholder'), 0)
                 FROM events
                 WHERE type = 'life' AND instance = 'stale'
                 ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(placeholder, 1);

        cleanup(path);
    }

    #[test]
    fn test_cleanup_stale_placeholders_keeps_fresh() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();

        let now = now_epoch_f64();
        let mut data = serde_json::Map::new();
        data.insert("name".into(), serde_json::json!("fresh"));
        data.insert("status".into(), serde_json::json!("pending"));
        data.insert("status_context".into(), serde_json::json!("new"));
        data.insert("created_at".into(), serde_json::json!(now));
        db.save_instance_named("fresh", &data).unwrap();

        let deleted = cleanup_stale_placeholders(&db);
        assert_eq!(deleted, 0);
        assert!(db.get_instance_full("fresh").unwrap().is_some());

        cleanup(path);
    }

    #[test]
    fn test_cleanup_stale_placeholders_skips_non_placeholder() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();

        let old_time = now_epoch_f64() - 200.0;
        let mut data = serde_json::Map::new();
        data.insert("name".into(), serde_json::json!("real"));
        data.insert("session_id".into(), serde_json::json!("sess-1"));
        data.insert("status".into(), serde_json::json!("pending"));
        data.insert("status_context".into(), serde_json::json!("new"));
        data.insert("created_at".into(), serde_json::json!(old_time));
        db.save_instance_named("real", &data).unwrap();

        let deleted = cleanup_stale_placeholders(&db);
        assert_eq!(deleted, 0);
        assert!(db.get_instance_full("real").unwrap().is_some());

        cleanup(path);
    }

    #[test]
    fn test_placeholder_cleanup_does_not_delete_instance_bound_after_scan() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();

        let old_time = now_epoch_f64() - 200.0;
        let mut data = serde_json::Map::new();
        data.insert("name".into(), serde_json::json!("raced"));
        data.insert("status".into(), serde_json::json!("pending"));
        data.insert("status_context".into(), serde_json::json!("new"));
        data.insert("created_at".into(), serde_json::json!(old_time));
        data.insert("tool".into(), serde_json::json!("codex"));
        db.save_instance_named("raced", &data).unwrap();

        // This is the row observed by cleanup_stale_placeholders before the
        // hook wins the bind race.
        let scanned = db.get_instance_full("raced").unwrap().unwrap();
        let mut updates = serde_json::Map::new();
        updates.insert("session_id".into(), serde_json::json!("thread-raced"));
        db.update_instance_fields("raced", &updates).unwrap();
        db.set_session_binding("thread-raced", "raced").unwrap();

        assert_eq!(
            crate::hooks::common::stop_placeholder_instance(
                &db,
                "raced",
                scanned.created_at,
                "system",
                "stale_cleanup",
            ),
            crate::hooks::common::StopOutcome::AlreadyStopped
        );
        let stored = db.get_instance_full("raced").unwrap().unwrap();
        assert_eq!(stored.session_id.as_deref(), Some("thread-raced"));
        assert_eq!(stored.status_context, "new");
        assert_eq!(
            db.get_session_binding("thread-raced").unwrap().as_deref(),
            Some("raced")
        );
        let stopped_events: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM events
                 WHERE type = 'life' AND instance = 'raced'
                   AND json_extract(data, '$.action') = 'stopped'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stopped_events, 0);

        cleanup(path);
    }

    #[test]
    fn test_placeholder_cleanup_does_not_signal_reused_pid() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();

        #[cfg(windows)]
        let child = std::process::Command::new("cmd")
            .args(["/C", "ping -n 31 127.0.0.1 >NUL"])
            .spawn()
            .unwrap();
        #[cfg(not(windows))]
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();

        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let child = ChildGuard(child);
        let pid = child.0.id();
        let identity = crate::sys::process::identity(pid).unwrap();

        let old_time = now_epoch_f64() - 200.0;
        let mut data = serde_json::Map::new();
        data.insert("name".into(), serde_json::json!("stale-pid"));
        data.insert("status".into(), serde_json::json!("pending"));
        data.insert("status_context".into(), serde_json::json!("new"));
        data.insert("created_at".into(), serde_json::json!(old_time));
        data.insert("background".into(), serde_json::json!(1));
        data.insert("pid".into(), serde_json::json!(pid));
        db.save_instance_named("stale-pid", &data).unwrap();

        let outcome = crate::hooks::common::stop_placeholder_instance(
            &db,
            "stale-pid",
            old_time,
            "system",
            "stale_cleanup",
        );
        let child_survived = crate::sys::process::has_identity(pid, &identity);

        assert_eq!(outcome, crate::hooks::common::StopOutcome::Stopped);
        assert!(db.get_instance_full("stale-pid").unwrap().is_none());
        assert!(
            child_survived,
            "a stale row's numeric PID is not proof of process ownership"
        );
        drop(child);
        cleanup(path);
    }

    #[test]
    fn test_placeholder_replacement_created_by_stop_event_keeps_its_capability() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();

        let old_time = now_epoch_f64() - 200.0;
        let replacement_time = now_epoch_f64();
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, status, status_context, created_at, tool)
                 VALUES ('reused', 'pending', 'new', ?1, 'codex')",
                rusqlite::params![old_time],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO claude_actor_capabilities
                 (token, session_id, tool_use_id, agent_id, instance_name,
                  created_at, expires_at, last_seen)
                 VALUES ('old-cap', 'old-session', 'old-tool', '', 'reused', 1, 9999999999, 1)",
                [],
            )
            .unwrap();
        db.conn()
            .execute_batch(&format!(
                "CREATE TRIGGER replace_placeholder_on_stop
                 AFTER INSERT ON events
                 WHEN NEW.type = 'life' AND NEW.instance = 'reused'
                   AND json_extract(NEW.data, '$.placeholder') = 1
                 BEGIN
                   INSERT INTO instances
                     (name, session_id, status, status_context, created_at, tool)
                   VALUES
                     ('reused', 'new-session', 'listening', '', {replacement_time}, 'codex');
                   INSERT INTO session_bindings
                     (session_id, instance_name, created_at)
                   VALUES ('new-session', 'reused', {replacement_time});
                   INSERT INTO claude_actor_capabilities
                     (token, session_id, tool_use_id, agent_id, instance_name,
                      created_at, expires_at, last_seen)
                   VALUES
                     ('new-cap', 'new-session', 'new-tool', '', 'reused', 2, 9999999999, 2);
                 END;"
            ))
            .unwrap();

        assert_eq!(
            crate::hooks::common::stop_placeholder_instance(
                &db,
                "reused",
                old_time,
                "system",
                "stale_cleanup",
            ),
            crate::hooks::common::StopOutcome::Stopped
        );
        let replacement = db.get_instance_full("reused").unwrap().unwrap();
        assert_eq!(replacement.session_id.as_deref(), Some("new-session"));
        assert_eq!(replacement.created_at, replacement_time);
        assert_eq!(
            db.get_session_binding("new-session").unwrap().as_deref(),
            Some("reused")
        );
        let capabilities: Vec<String> = db
            .conn()
            .prepare(
                "SELECT token FROM claude_actor_capabilities
                 WHERE instance_name = 'reused' ORDER BY token",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(capabilities, vec!["new-cap"]);

        cleanup(path);
    }

    #[test]
    fn cleanup_preserves_bound_hook_only_claude_timeout() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();
        let old = now_epoch_i64() - 120;
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, session_id, tool, status, status_context, status_time, created_at)
                 VALUES ('risa', 'sess-risa', 'claude', 'inactive', 'exit:timeout', ?, 1)",
                rusqlite::params![old],
            )
            .unwrap();
        db.set_session_binding("sess-risa", "risa").unwrap();

        assert_eq!(cleanup_stale_instances(&db, 3600, 3600), 0);
        assert!(db.get_instance_full("risa").unwrap().is_some());

        cleanup(path);
    }

    #[test]
    fn cleanup_removes_unbound_claude_timeout() {
        crate::config::Config::init();
        let (db, path) = setup_test_db();
        let old = now_epoch_i64() - 120;
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, session_id, tool, status, status_context, status_time, created_at)
                 VALUES ('risa', 'sess-risa', 'claude', 'inactive', 'exit:timeout', ?, 1)",
                rusqlite::params![old],
            )
            .unwrap();

        assert_eq!(cleanup_stale_instances(&db, 3600, 3600), 1);
        assert!(db.get_instance_full("risa").unwrap().is_none());

        cleanup(path);
    }

    #[test]
    fn cleanup_preserves_bound_hook_only_claude_stale_states() {
        crate::config::Config::init();
        for state in [ST_LISTENING, ST_ACTIVE, ST_BLOCKED] {
            let (db, path) = setup_test_db();
            let old = now_epoch_i64() - 7200;
            db.conn()
                .execute(
                    "INSERT INTO instances
                 (name, session_id, tool, status, status_context, status_time, created_at)
                 VALUES ('risa', 'sess-risa', 'claude', ?, 'tool:Bash', ?, 1)",
                    rusqlite::params![state, old],
                )
                .unwrap();
            db.set_session_binding("sess-risa", "risa").unwrap();

            let before = db.get_instance_full("risa").unwrap().unwrap();
            let computed = get_instance_status(&before, &db);
            assert_eq!(computed.status, ST_INACTIVE);
            assert_eq!(computed.context, "stale");
            assert!(computed.age_seconds >= 7200);
            assert_eq!(cleanup_stale_instances(&db, 3600, 3600), 0, "{state}");
            let after = db.get_instance_full("risa").unwrap().unwrap();
            assert_eq!(after.status_time, old, "retention must not fake freshness");
            assert_eq!(after.status, state);
            assert_eq!(
                db.get_session_binding("sess-risa").unwrap().as_deref(),
                Some("risa")
            );
            cleanup(path);
        }
    }

    #[test]
    fn cleanup_stale_preservation_requires_exact_hook_only_claude_binding() {
        crate::config::Config::init();
        for case in ["unbound", "wrong-owner", "process-bound", "different-tool"] {
            let (db, path) = setup_test_db();
            let old = now_epoch_i64() - 7200;
            let tool = if case == "different-tool" {
                "codex"
            } else {
                "claude"
            };
            db.conn()
                .execute(
                    "INSERT INTO instances
                 (name, session_id, tool, status, status_context, status_time, created_at)
                 VALUES ('risa', 'sess-risa', ?, 'active', 'tool:Bash', ?, 1)",
                    rusqlite::params![tool, old],
                )
                .unwrap();
            if case == "wrong-owner" {
                let now = now_epoch_i64();
                db.conn()
                    .execute(
                        "INSERT INTO instances
                     (name, session_id, tool, status, status_time, created_at)
                     VALUES ('other', 'sess-other', 'claude', 'active', ?1, ?1)",
                        rusqlite::params![now],
                    )
                    .unwrap();
                db.set_session_binding("sess-risa", "other").unwrap();
            } else if case != "unbound" {
                db.set_session_binding("sess-risa", "risa").unwrap();
            }
            if case == "process-bound" {
                db.set_process_binding("proc-risa", "sess-risa", "risa")
                    .unwrap();
            }
            assert_eq!(cleanup_stale_instances(&db, 3600, 3600), 1, "{case}");
            assert!(db.get_instance_full("risa").unwrap().is_none(), "{case}");
            cleanup(path);
        }
    }

    #[test]
    fn cleanup_still_removes_explicitly_stopped_hook_only_claude() {
        crate::config::Config::init();
        for context in [
            "exit:killed",
            "exit:closed",
            "exit:interrupted",
            "exit:session_switch",
        ] {
            let (db, path) = setup_test_db();
            let old = now_epoch_i64() - 120;
            db.conn()
                .execute(
                    "INSERT INTO instances
                 (name, session_id, tool, status, status_context, status_time, created_at)
                 VALUES ('risa', 'sess-risa', 'claude', 'inactive', ?, ?, 1)",
                    rusqlite::params![context, old],
                )
                .unwrap();
            db.set_session_binding("sess-risa", "risa").unwrap();
            assert_eq!(cleanup_stale_instances(&db, 3600, 3600), 1, "{context}");
            assert!(db.get_instance_full("risa").unwrap().is_none());
            cleanup(path);
        }
    }
}
