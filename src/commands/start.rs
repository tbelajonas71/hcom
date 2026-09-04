//! Start command: `hcom start [--name <agent-id>] [--as <name> [--relocate|--migrate-platform]] [--orphan <name|pid>]`
//!
//! Runs inside an already-running tool session rather than launching a new one.
//! Used for adhoc/manual setup, identity rebinding, and orphan recovery:
//! - Bare start: detect vanilla tool or create adhoc instance
//! - `--name <agent-id>`: register a subagent (a router-level global flag, not
//!   parsed by `StartArgs` — resolved in `run()` via `flags.name`)
//! - `--orphan`: recover orphaned PTY process
//! - `--as`: rebind session identity

use anyhow::{Result, bail};
use rusqlite::OptionalExtension;
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use crate::bootstrap;
use crate::claude_actor;
use crate::config::HcomConfig;
use crate::db::{HcomDb, InstanceRow};
use crate::identity;
use crate::instance_binding;
use crate::instance_lifecycle as lifecycle;
use crate::instance_names;
use crate::instances;
use crate::log::log_info;
use crate::paths;
use crate::pidtrack;
use crate::relay;
use crate::router::GlobalFlags;
use crate::shared::constants::ST_ACTIVE;
use crate::shared::context::HcomContext;

/// Parsed arguments for `hcom start`.
#[derive(clap::Parser, Debug)]
#[command(name = "start", about = "Start hcom participation")]
pub struct StartArgs {
    /// Rebind to a different instance name
    #[arg(long = "as")]
    pub as_name: Option<String>,
    /// Explicitly move a stopped top-level Codex identity to this task's directory
    #[arg(long)]
    pub relocate: bool,
    /// One-off guarded migration of the stopped NSFW Studio Claude holder to its Codex task
    #[arg(long)]
    pub migrate_platform: bool,
    /// Absolute developer-registry path used only with --migrate-platform
    #[arg(long, value_name = "PATH")]
    pub registry: Option<PathBuf>,
    /// Recover orphaned PTY process by name or PID
    #[arg(long)]
    pub orphan: Option<String>,
}

/// Run the start command.
pub fn run(argv: &[String], flags: &GlobalFlags) -> Result<i32> {
    // Filter out global flags already consumed by the router (start, --name X, --go)
    let mut filtered = vec!["start".to_string()];
    let mut skip_next = false;
    for arg in argv {
        if skip_next {
            skip_next = false;
            continue;
        }
        match arg.as_str() {
            "start" | "--go" => continue,
            "--name" => {
                skip_next = true;
                continue;
            }
            _ => filtered.push(arg.clone()),
        }
    }

    use clap::Parser;
    let start_args = match StartArgs::try_parse_from(&filtered) {
        Ok(a) => a,
        Err(e) => {
            e.print().ok();
            return Ok(if e.use_stderr() { 1 } else { 0 });
        }
    };

    let orphan_target = start_args.orphan;
    let rebind_target = start_args.as_name;
    let relocate = start_args.relocate;
    let migrate_platform = start_args.migrate_platform;
    let registry_path = start_args.registry;

    if relocate && rebind_target.is_none() {
        bail!("--relocate requires --as <name>");
    }
    if relocate && orphan_target.is_some() {
        bail!("--relocate cannot be combined with --orphan");
    }
    if migrate_platform && rebind_target.is_none() {
        bail!("--migrate-platform requires --as <name>");
    }
    if migrate_platform && orphan_target.is_some() {
        bail!("--migrate-platform cannot be combined with --orphan");
    }
    if migrate_platform && relocate {
        bail!("--migrate-platform cannot be combined with --relocate");
    }
    if migrate_platform && registry_path.is_none() {
        bail!("--migrate-platform requires --registry <absolute DEV-REGISTRY.json path>");
    }
    if !migrate_platform && registry_path.is_some() {
        bail!("--registry is supported only with --migrate-platform");
    }

    let db = HcomDb::open()?;
    let hcom_dir = paths::hcom_dir();

    let ctx = HcomContext::from_os();
    let verified_actor = claude_actor::resolve_env_actor(&db).map_err(anyhow::Error::new)?;
    if let (Some(actor), Some(name)) = (verified_actor.as_ref(), flags.name.as_deref()) {
        claude_actor::ensure_explicit_matches(&db, actor, name).map_err(anyhow::Error::new)?;
    }

    let requested_name = flags
        .name
        .as_deref()
        .map(|name| identity::resolve_display_name(&db, name).unwrap_or_else(|| name.to_string()));

    // A verified child actor can only promote/use its existing row. It cannot
    // rebind or recover another identity, and it does not need --name.
    if let Some(actor) = verified_actor.as_ref()
        && let Some(actor_row) = db.get_instance_full(&actor.name)?
        && instances::is_subagent_instance(&actor_row)
    {
        if rebind_target.is_some() {
            println!("[HCOM] Subagents cannot use --as. End your turn.");
            return Ok(1);
        }
        if orphan_target.is_some() {
            println!("[HCOM] Subagents cannot use --orphan. End your turn.");
            return Ok(1);
        }
        return start_subagent(&db, &actor_row);
    }

    // Without a capability, retain the ordinary manual fallback. A direct
    // indexed child lookup supports the documented --name <agent-id> form
    // without scanning duplicated parent JSON.
    let subagent_via_name = if verified_actor.is_none() {
        requested_name
            .as_deref()
            .and_then(|id| detect_subagent(&db, id))
    } else {
        None
    };
    let subagent_via_as = if verified_actor.is_none() {
        rebind_target
            .as_deref()
            .and_then(|id| detect_subagent(&db, id))
    } else {
        None
    };

    if subagent_via_as.is_some() || (subagent_via_name.is_some() && rebind_target.is_some()) {
        println!("[HCOM] Subagents cannot change identity. End your turn.");
        return Ok(1);
    }

    if let Some(orphan) = orphan_target {
        return start_from_orphan(&db, &hcom_dir, &orphan, &ctx);
    }

    if let Some(rebind) = rebind_target {
        let current_name = verified_actor
            .as_ref()
            .map(|actor| actor.name.as_str())
            .or(requested_name.as_deref());
        return start_rebind_with_options(
            &db,
            &rebind,
            &ctx,
            current_name,
            relocate,
            migrate_platform,
            registry_path.as_deref(),
        );
    }

    if let Some(subagent) = subagent_via_name {
        return start_subagent(&db, &subagent);
    }

    // A verified root actor stays the root even while children exist.
    let effective_name = verified_actor
        .as_ref()
        .map(|actor| actor.name.as_str())
        .or(requested_name.as_deref());
    start_bare(&db, &hcom_dir, &ctx, effective_name)
}

/// Resolve a live child row directly by agent_id (or by its exact row name).
fn detect_subagent(db: &HcomDb, check_id: &str) -> Option<InstanceRow> {
    let name = db
        .get_instance_by_agent_id(check_id)
        .ok()
        .flatten()
        .unwrap_or_else(|| check_id.to_string());
    let row = db.get_instance_full(&name).ok().flatten()?;
    row.parent_name.as_ref().filter(|name| !name.is_empty())?;
    Some(row)
}

/// Promote an existing dormant child row into active hcom participation.
fn start_subagent(db: &HcomDb, info: &InstanceRow) -> Result<i32> {
    let parent_name = info.parent_name.as_deref().unwrap_or("");
    if parent_name.is_empty() || info.agent_id.as_deref().unwrap_or("").is_empty() {
        bail!(
            "Subagent row '{}' is missing parent/agent identity",
            info.name
        );
    }

    let was_announced = info.name_announced != 0;
    lifecycle::set_status(db, &info.name, ST_ACTIVE, "tool:start", Default::default());
    instance_binding::capture_and_store_launch_context(db, &info.name);

    log_info(
        "lifecycle",
        "start.subagent",
        &format!(
            "name={} parent={} agent_id={} announced={}",
            info.name,
            parent_name,
            info.agent_id.as_deref().unwrap_or(""),
            was_announced
        ),
    );

    if was_announced {
        println!("hcom already started for {}", info.name);
        return Ok(0);
    }

    let bootstrap = bootstrap::get_subagent_bootstrap(&info.name, parent_name);
    if !bootstrap.is_empty() {
        println!("{bootstrap}");
    }
    let mut updates = serde_json::Map::new();
    updates.insert("name_announced".into(), serde_json::json!(true));
    instances::update_instance_position(db, &info.name, &updates);

    Ok(0)
}

/// Recover orphaned PTY process by PID or name.
fn start_from_orphan(
    db: &HcomDb,
    hcom_dir: &std::path::Path,
    target: &str,
    _ctx: &HcomContext,
) -> Result<i32> {
    let active_pids: HashSet<u32> = db
        .iter_instances_full()?
        .iter()
        .filter_map(|inst| inst.pid.map(|p| p as u32))
        .collect();
    let orphans = pidtrack::get_orphan_processes(hcom_dir, Some(&active_pids));

    if orphans.is_empty() {
        bail!("No orphan processes found.");
    }

    // Match by PID or name
    let orphan = if let Ok(pid) = target.parse::<u32>() {
        match orphans.iter().find(|o| o.pid == pid) {
            Some(o) => o,
            None => bail!("Orphan PID {} not found.", pid),
        }
    } else {
        let matches: Vec<_> = orphans
            .iter()
            .filter(|o| o.names.contains(&target.to_string()))
            .collect();
        match matches.len() {
            0 => bail!("Orphan '{}' not found.", target),
            1 => matches[0],
            _ => {
                let pids: Vec<String> = matches.iter().map(|m| m.pid.to_string()).collect();
                bail!(
                    "Multiple orphans match '{}' (PIDs: {}). Use --orphan <pid>.",
                    target,
                    pids.join(", ")
                );
            }
        }
    };

    let pid = orphan.pid;

    if orphan.process_id.is_empty() {
        bail!(
            "Orphan PID {} has no process_id and cannot be recovered.",
            pid
        );
    }

    let preferred_name = orphan.names.last().cloned().unwrap_or_default();
    let can_reuse = !preferred_name.is_empty()
        && identity::is_valid_base_name(&preferred_name)
        && db.get_instance_full(&preferred_name)?.is_none();
    let name = if can_reuse {
        preferred_name
    } else {
        instance_names::generate_unique_name(db)?
    };

    // Core DB registration
    let _ = pidtrack::recover_single_orphan_to_db(db, orphan, &name);

    db.log_event(
        "life",
        &name,
        &json!({
            "action": "started",
            "by": "cli",
            "reason": "orphan_recover",
            "orphan_pid": pid,
        }),
    )
    .ok();

    pidtrack::remove_pid(hcom_dir, pid);

    println!("[hcom:{}]", name);
    if can_reuse {
        println!("Recovered orphan PID {} as '{}'.", pid, name);
    } else {
        println!(
            "Recovered orphan PID {} as new identity '{}' (name conflict/unavailable).",
            pid, name
        );
    }

    log_info(
        "start",
        "orphan.recovered",
        &format!("name={} pid={} tool={}", name, pid, orphan.tool),
    );

    Ok(0)
}

#[derive(Debug, Clone)]
struct ChildLink {
    name: String,
    parent_name: Option<String>,
}

fn snapshot_child_links(db: &HcomDb, session_id: Option<&str>) -> Result<Vec<ChildLink>> {
    let Some(session_id) = session_id.filter(|value| !value.is_empty()) else {
        return Ok(Vec::new());
    };
    let mut stmt = db
        .conn()
        .prepare("SELECT name, parent_name FROM instances WHERE parent_session_id = ?")?;
    let rows = stmt.query_map(rusqlite::params![session_id], |row| {
        Ok(ChildLink {
            name: row.get(0)?,
            parent_name: row.get(1)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn restore_child_links_after_root_rebind(
    db: &HcomDb,
    links: &[ChildLink],
    session_id: &str,
    old_root: &str,
    new_root: &str,
) -> Result<()> {
    db.with_immediate_transaction(|txn| {
        for link in links {
            let parent_name = match link.parent_name.as_deref() {
                Some(parent) if parent == old_root => Some(new_root),
                other => other,
            };
            txn.execute(
                "UPDATE instances SET parent_session_id = ?, parent_name = ? WHERE name = ?",
                rusqlite::params![session_id, parent_name, &link.name],
            )?;
        }
        Ok(())
    })
}

/// Rebind session identity (`--as <name>`), preserving last_event_id and any
/// live Claude child hierarchy owned by the current root actor.
#[cfg(test)]
fn start_rebind(
    db: &HcomDb,
    rebind_target: &str,
    ctx: &HcomContext,
    explicit_name: Option<&str>,
) -> Result<i32> {
    start_rebind_with_options(db, rebind_target, ctx, explicit_name, false, false, None)
}

fn start_rebind_with_options(
    db: &HcomDb,
    rebind_target: &str,
    ctx: &HcomContext,
    explicit_name: Option<&str>,
    relocate: bool,
    migrate_platform: bool,
    registry_path: Option<&Path>,
) -> Result<i32> {
    let hcom_dir = paths::hcom_dir();

    // Resolve the target name
    let target_name = identity::resolve_display_name_or_stopped(db, rebind_target)
        .unwrap_or_else(|| rebind_target.to_string());

    // Guard: refuse to reclaim a subagent slot. Subagents share their parent's
    // session_id, so `hcom start --as <subagent_name>` from inside a subagent
    // bash would rebind session_bindings[parent_sid] to the subagent name,
    // clobbering the parent's identity. `--as` is documented for top-level
    // restartable identities (compaction/resume/clear), not for subagent
    // lifecycle — which has its own SubagentStart bootstrap path.
    if db.was_subagent_name(&target_name) {
        eprintln!(
            "Error: '{target_name}' is a subagent slot; cannot be reclaimed with --as.\n\
             Subagents register via 'hcom start --name <agent-id>' in the SubagentStart context. If your session ended, stop working and end your turn."
        );
        return Ok(1);
    }

    let explicit_current_name = explicit_name.unwrap_or("");

    // Resolve session_id from process binding or existing instance
    let mut session_id: Option<String> = None;
    if let Some(ref process_id) = ctx.process_id
        && let Ok(Some((sid, _))) = db.get_process_binding_full(process_id)
    {
        session_id = sid.filter(|s| !s.is_empty());
    }
    if session_id.is_none()
        && !explicit_current_name.is_empty()
        && let Ok(Some(current_data)) = db.get_instance_full(explicit_current_name)
    {
        session_id = current_data.session_id.filter(|s| !s.is_empty());
    }
    if session_id.is_none() && ctx.tool == crate::tool::Tool::Claude {
        // A vanilla Claude session has neither a process binding nor, before its
        // first start, a row to read the id back from. Its own session id is
        // what makes the rebind stick: without it the reclaimed name stays
        // unbound and the identity it replaces is never cleaned up.
        session_id = resolve_claude_session_id(&ctx.raw_env);
    }
    if ctx.tool == crate::tool::Tool::Codex {
        // Codex Desktop is not launched through hcom, so it has no
        // HCOM_PROCESS_ID/process binding. Its thread id is the stable
        // session identity exposed to commands run inside the task.
        if let Some(codex_session_id) = resolve_codex_session_id(ctx) {
            if let Some(existing_session_id) = session_id.as_deref()
                && existing_session_id != codex_session_id
            {
                bail!(
                    "Refusing Codex rebind: current task session '{}' conflicts with existing session '{}'",
                    codex_session_id,
                    existing_session_id
                );
            }
            session_id = Some(codex_session_id);
        }
    }
    let current_name = if !explicit_current_name.is_empty() {
        explicit_current_name.to_string()
    } else if let Some(ref sid) = session_id {
        db.get_session_binding(sid)?.unwrap_or_default()
    } else {
        String::new()
    };
    let child_links = snapshot_child_links(db, session_id.as_deref())?;

    let relocation = if relocate {
        Some(validate_codex_relocation(
            db,
            &target_name,
            ctx,
            session_id.as_deref(),
            &current_name,
        )?)
    } else {
        None
    };
    let platform_migration = if migrate_platform {
        let registry_path = registry_path.ok_or_else(|| {
            anyhow::anyhow!(
                "--migrate-platform requires --registry <absolute DEV-REGISTRY.json path>"
            )
        })?;
        Some(validate_codex_platform_migration(
            db,
            &target_name,
            ctx,
            session_id.as_deref(),
            &current_name,
            registry_path,
        )?)
    } else {
        None
    };
    let target_meta = if relocation.is_some() || platform_migration.is_some() {
        None
    } else {
        load_rebind_target_metadata(db, &target_name).ok()
    };
    if let Some(ref meta) = target_meta {
        ensure_rebind_compatible(&target_name, meta, ctx, relocation.is_some())?;
    }

    // A relocation is deliberately not a generic rebind. A generic rebind
    // deletes the target before recreating it, which lets a registration that
    // wins between validation and mutation be deleted and stolen. Compare the
    // exact stopped proof, create the row, and bind the session under one
    // BEGIN IMMEDIATE transaction. Any competing winner makes the whole
    // transaction fail without changing that winner.
    if let Some(ref proof) = relocation {
        commit_codex_relocation(db, &target_name, ctx, proof)?;
        return finish_rebind_output(db, &hcom_dir, &target_name, ctx, &current_name, true);
    }
    if let Some(ref proof) = platform_migration {
        commit_codex_platform_migration(db, &target_name, ctx, proof)?;
        return finish_rebind_output(db, &hcom_dir, &target_name, ctx, &current_name, false);
    }

    // Preserve last_event_id from target (cursor preservation)
    let mut last_event_id = target_meta.as_ref().map(|m| m.last_event_id);
    let target_data = db.get_instance_full(&target_name)?;

    // Final fallback: use current max to avoid re-delivering old messages
    if last_event_id.is_none() {
        last_event_id = Some(db.get_last_event_id());
    }

    // Skip delete for remote instances (origin_device_id)
    if let Some(ref td) = target_data
        && (td.origin_device_id.is_none() || td.origin_device_id.as_deref() == Some(""))
        && let Err(e) = db.delete_instance(&target_name)
    {
        eprintln!("[hcom] warn: delete_instance failed for {target_name}: {e}");
    }

    // Clean up target's bindings
    if let Err(e) = db.delete_process_bindings_for_instance(&target_name) {
        eprintln!("[hcom] warn: delete_process_bindings failed for {target_name}: {e}");
    }
    if let Err(e) = db.delete_session_bindings_for_instance(&target_name) {
        eprintln!("[hcom] warn: delete_session_bindings failed for {target_name}: {e}");
    }

    // Delete old identity if different from target
    if !current_name.is_empty()
        && current_name != target_name
        && let Err(e) = db.delete_instance(&current_name)
    {
        eprintln!("[hcom] warn: delete_instance failed for {current_name}: {e}");
    }

    // Create fresh instance with the target name
    let tool = ctx.tool.as_str();
    let cwd_override = ctx.cwd.to_string_lossy().to_string();
    let initialized = instance_binding::initialize_instance_in_position_file(
        db,
        &target_name,
        session_id.as_deref(),
        None, // parent_session_id
        None, // parent_name
        None, // agent_id
        None,
        Some(tool),
        false, // background
        None,  // tag
        None,  // wait_timeout
        None,  // subagent_timeout
        None,  // hints
        Some(&cwd_override),
    );

    debug_assert!(relocation.is_none() && platform_migration.is_none());
    if !initialized {
        bail!("Could not create identity '{target_name}'");
    }

    if let Some(ref sid) = session_id {
        let old_root = if current_name.is_empty() {
            target_name.as_str()
        } else {
            current_name.as_str()
        };
        restore_child_links_after_root_rebind(db, &child_links, sid, old_root, &target_name)?;
        if old_root != target_name {
            db.rebind_claude_root_actor_state(sid, old_root, &target_name)?;
        }
    }

    // Restore cursor position + mark as announced
    {
        let mut updates = serde_json::Map::new();
        if let Some(eid) = last_event_id {
            updates.insert("last_event_id".into(), serde_json::json!(eid));
        }
        updates.insert("name_announced".into(), serde_json::json!(1));
        if let Err(e) = db.update_instance_fields(&target_name, &updates) {
            eprintln!("[hcom] warn: update_instance_fields failed for {target_name}: {e}");
        }
    }

    // Create bindings
    if let Some(ref sid) = session_id {
        if let Err(e) = db.set_session_binding(sid, &target_name) {
            eprintln!("[hcom] warn: set_session_binding failed for {target_name}: {e}");
        } else if ctx.tool == crate::tool::Tool::Claude
            && let Err(e) = db.mark_claude_session_validated(sid, &target_name)
        {
            // The cache still names the identity being replaced, and it is keyed
            // by session generation, so it does not expire on its own. Left
            // stale, every hook for this session resolves to no_instance: no
            // status, no delivery, and the reclaimed row is flagged
            // launch_failed ~30s later while the session is alive and bound.
            eprintln!("[hcom] warn: mark_claude_session_validated failed for {target_name}: {e}");
        }
    }
    if let Some(ref process_id) = ctx.process_id {
        let sid = session_id.as_deref().unwrap_or("");
        if let Err(e) = db.set_process_binding(process_id, sid, &target_name) {
            eprintln!("[hcom] warn: set_process_binding failed for {target_name}: {e}");
        }

        // Migrate notify endpoints before notify so wake reaches correct port
        if !current_name.is_empty()
            && current_name != target_name
            && let Err(e) = db.migrate_notify_endpoints(&current_name, &target_name)
        {
            eprintln!("[hcom] warn: migrate_notify_endpoints failed: {e}");
        }

        crate::notify::wake(db, &target_name, crate::notify::WakeKind::DELIVERY_LOOPS);
    }

    finish_rebind_output(db, &hcom_dir, &target_name, ctx, &current_name, false)
}

fn finish_rebind_output(
    db: &HcomDb,
    hcom_dir: &std::path::Path,
    target_name: &str,
    ctx: &HcomContext,
    current_name: &str,
    relocated: bool,
) -> Result<i32> {
    let tool = ctx.tool.as_str();
    let hcom_config = HcomConfig::load(None).unwrap_or_else(|_| {
        let mut c = HcomConfig::default();
        c.normalize();
        c
    });

    let bootstrap_text = bootstrap::get_bootstrap(
        db,
        hcom_dir,
        target_name,
        tool,
        false,
        false,
        &ctx.notes,
        &hcom_config.tag,
        relay::is_relay_enabled(&hcom_config),
        None,
    );

    println!("[hcom:{}]", target_name);
    println!("{}", bootstrap_text);
    // Same reason as bare start: keep the new name visible in a tailed snapshot.
    println!("[hcom:{}]", target_name);

    log_info(
        "start",
        "rebind.complete",
        &format!(
            "from={} to={} relocated={}",
            current_name, target_name, relocated
        ),
    );

    Ok(0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexRelocationProof {
    session_id: String,
    transcript_path: String,
    stopped_event_id: i64,
    stopped_event_data: String,
    last_event_id: i64,
}

const MAX_CODEX_SESSION_META_BYTES: u64 = 2 * 1024 * 1024;
const MAX_PLATFORM_AUTHORITY_BYTES: u64 = 4 * 1024 * 1024;
const MAX_CODEX_TRANSCRIPT_ENTRIES: usize = 1_000_000;
const PLATFORM_MIGRATION_ENDPOINT_STALE_SECS: f64 = 90.0;
// Windows can take about two seconds to return ConnectionRefused for a closed
// loopback port. Keep a bounded margin so that refusal can actually be observed.
#[cfg(windows)]
const PLATFORM_MIGRATION_ENDPOINT_CONNECT_TIMEOUT_MS: u64 = 3000;
#[cfg(not(windows))]
const PLATFORM_MIGRATION_ENDPOINT_CONNECT_TIMEOUT_MS: u64 = 100;
const TRUSTED_PLATFORM_REGISTRY_PATH: &str = r"D:\Projects\hcc-migration\board\DEV-REGISTRY.json";
const TRUSTED_PLATFORM_MIGRATION_ROLE: &str = "nsfw-studio";

fn metadata_is_link_or_reparse(metadata: &std::fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn ensure_path_chain_has_no_links(path: &Path, label: &str) -> Result<()> {
    let mut current = Some(path);
    while let Some(component) = current {
        let metadata = std::fs::symlink_metadata(component).map_err(|error| {
            anyhow::anyhow!(
                "Could not inspect {label} path component '{}': {error}",
                component.display()
            )
        })?;
        if metadata_is_link_or_reparse(&metadata) {
            bail!(
                "{label} path component '{}' must not be a link or reparse point",
                component.display()
            );
        }
        current = component.parent().filter(|parent| *parent != component);
    }
    Ok(())
}

fn open_regular_file_no_follow(
    path: &Path,
    label: &str,
) -> Result<(std::fs::File, std::fs::Metadata)> {
    ensure_path_chain_has_no_links(path, label)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options
        .open(path)
        .map_err(|error| anyhow::anyhow!("Could not open {label} '{}': {error}", path.display()))?;
    let metadata = file.metadata().map_err(|error| {
        anyhow::anyhow!(
            "Could not inspect opened {label} '{}': {error}",
            path.display()
        )
    })?;
    if metadata_is_link_or_reparse(&metadata) || !metadata.is_file() {
        bail!(
            "Opened {label} '{}' must be a regular file, not a link",
            path.display()
        );
    }
    Ok((file, metadata))
}

fn validate_platform_registry_anchor(
    registry_path: &Path,
    trusted_registry_path: &Path,
) -> Result<PathBuf> {
    if !registry_path.is_absolute() {
        bail!("--registry must be an absolute path");
    }
    if !trusted_registry_path.is_absolute() {
        bail!("The configured trusted platform registry path is not absolute");
    }
    if !same_path(
        registry_path.to_string_lossy().as_ref(),
        trusted_registry_path.to_string_lossy().as_ref(),
    ) {
        bail!(
            "--registry must be the trusted platform registry '{}'",
            trusted_registry_path.display()
        );
    }
    ensure_path_chain_has_no_links(registry_path, "DEV-REGISTRY.json")?;
    ensure_path_chain_has_no_links(trusted_registry_path, "Trusted DEV-REGISTRY.json")?;
    let canonical_registry = std::fs::canonicalize(registry_path)?;
    let canonical_trusted = std::fs::canonicalize(trusted_registry_path)?;
    if !same_path(
        canonical_registry.to_string_lossy().as_ref(),
        canonical_trusted.to_string_lossy().as_ref(),
    ) {
        bail!("--registry does not resolve to the trusted platform registry");
    }
    Ok(canonical_registry)
}

struct NoDuplicateJson;

impl<'de> DeserializeSeed<'de> for NoDuplicateJson {
    type Value = serde_json::Value;

    fn deserialize<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(NoDuplicateJsonVisitor)
    }
}

struct NoDuplicateJsonVisitor;

impl<'de> Visitor<'de> for NoDuplicateJsonVisitor {
    type Value = serde_json::Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("valid JSON without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> std::result::Result<Self::Value, E> {
        Ok(serde_json::Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> std::result::Result<Self::Value, E> {
        Ok(serde_json::Value::Number(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> std::result::Result<Self::Value, E> {
        Ok(serde_json::Value::Number(value.into()))
    }

    fn visit_f64<E>(self, value: f64) -> std::result::Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E> {
        Ok(serde_json::Value::String(value.to_string()))
    }

    fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E> {
        Ok(serde_json::Value::String(value))
    }

    fn visit_none<E>(self) -> std::result::Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }

    fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }

    fn visit_some<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        NoDuplicateJson.deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(NoDuplicateJson)? {
            values.push(value);
        }
        Ok(serde_json::Value::Array(values))
    }

    fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut keys = HashSet::new();
        let mut values = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                return Err(serde::de::Error::custom(format!(
                    "duplicate JSON object key '{key}'"
                )));
            }
            let value = map.next_value_seed(NoDuplicateJson)?;
            values.insert(key, value);
        }
        Ok(serde_json::Value::Object(values))
    }
}

fn parse_authority_json(bytes: &[u8], label: &str) -> Result<serde_json::Value> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = NoDuplicateJson
        .deserialize(&mut deserializer)
        .map_err(|error| anyhow::anyhow!("{label} is invalid JSON: {error}"))?;
    deserializer
        .end()
        .map_err(|error| anyhow::anyhow!("{label} is invalid JSON: {error}"))?;
    Ok(value)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LegacyNotifyEndpointProof {
    kind: String,
    port: u16,
    updated_at_bits: u64,
}

impl LegacyNotifyEndpointProof {
    fn updated_at(&self) -> f64 {
        f64::from_bits(self.updated_at_bits)
    }
}

/// Prove the exact incident-specific legacy delivery-loop shape.
///
/// This deliberately requires one stale `pty` row and one stale `inject` row.
/// It is not a general platform-ownership converter: missing or extra endpoint
/// state is unknown state and therefore fails closed.
fn validate_legacy_platform_endpoints(
    connection: &rusqlite::Connection,
    target_name: &str,
    now: f64,
) -> Result<Vec<LegacyNotifyEndpointProof>> {
    validate_legacy_platform_endpoints_with_probe(connection, target_name, now, |address| {
        std::net::TcpStream::connect_timeout(
            &address,
            std::time::Duration::from_millis(PLATFORM_MIGRATION_ENDPOINT_CONNECT_TIMEOUT_MS),
        )
        .map(drop)
    })
}

fn validate_legacy_platform_endpoints_with_probe(
    connection: &rusqlite::Connection,
    target_name: &str,
    now: f64,
    mut probe: impl FnMut(std::net::SocketAddr) -> std::io::Result<()>,
) -> Result<Vec<LegacyNotifyEndpointProof>> {
    if !now.is_finite() || now <= 0.0 {
        bail!("Could not establish a valid endpoint-check timestamp");
    }

    let mut statement = connection.prepare(
        "SELECT kind, port, updated_at
         FROM notify_endpoints
         WHERE instance = ?1
         ORDER BY kind",
    )?;
    let raw_endpoints = statement
        .query_map(rusqlite::params![target_name], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, f64>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);

    let kinds: Vec<&str> = raw_endpoints
        .iter()
        .map(|(kind, _, _)| kind.as_str())
        .collect();
    if kinds != ["inject", "pty"] {
        bail!(
            "Refusing to migrate '{target_name}': legacy endpoint proof must be exactly one 'pty' and one 'inject' row"
        );
    }

    let mut endpoints = Vec::with_capacity(raw_endpoints.len());
    for (kind, raw_port, updated_at) in raw_endpoints {
        let port = u16::try_from(raw_port)
            .ok()
            .filter(|port| *port != 0)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Refusing to migrate '{target_name}': legacy '{kind}' endpoint has an invalid port"
                )
            })?;
        if !updated_at.is_finite() || updated_at <= 0.0 || updated_at > now {
            bail!(
                "Refusing to migrate '{target_name}': legacy '{kind}' endpoint has an invalid or future timestamp"
            );
        }
        let age = now - updated_at;
        if age < PLATFORM_MIGRATION_ENDPOINT_STALE_SECS {
            bail!(
                "Refusing to migrate '{target_name}': legacy '{kind}' endpoint is only {age:.1}s old"
            );
        }

        let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        match probe(address) {
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {}
            Ok(()) => {
                bail!(
                    "Refusing to migrate '{target_name}': legacy '{kind}' endpoint on 127.0.0.1:{port} is still reachable"
                );
            }
            Err(error) => {
                // A timeout can mean a live listener has a full accept queue;
                // permission and resource errors also establish no ownership
                // fact. Only an explicit refusal proves this port is closed.
                bail!(
                    "Refusing to migrate '{target_name}': legacy '{kind}' endpoint on 127.0.0.1:{port} could not be proven closed: {error}"
                );
            }
        }
        endpoints.push(LegacyNotifyEndpointProof {
            kind,
            port,
            updated_at_bits: updated_at.to_bits(),
        });
    }
    Ok(endpoints)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexPlatformMigrationProof {
    session_id: String,
    transcript_path: String,
    stopped_event_id: i64,
    stopped_event_data: String,
    last_event_id: i64,
    from_tool: String,
    from_session_id: String,
    from_transcript_path: String,
    registry_path: PathBuf,
    registry_bytes: Vec<u8>,
    role_path: PathBuf,
    role_bytes: Vec<u8>,
    codex_sessions_root: PathBuf,
    claude_projects_root: PathBuf,
    legacy_endpoints: Vec<LegacyNotifyEndpointProof>,
}

fn read_platform_authority(path: &Path, label: &str) -> Result<Vec<u8>> {
    if !path.is_absolute() {
        bail!("{label} path must be absolute");
    }
    let (file, metadata) = open_regular_file_no_follow(path, label)?;
    if metadata.len() == 0 || metadata.len() > MAX_PLATFORM_AUTHORITY_BYTES {
        bail!(
            "{label} '{}' has an invalid size (maximum {} bytes)",
            path.display(),
            MAX_PLATFORM_AUTHORITY_BYTES
        );
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_PLATFORM_AUTHORITY_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_PLATFORM_AUTHORITY_BYTES {
        bail!("{label} '{}' exceeds the size limit", path.display());
    }
    Ok(bytes)
}

fn canonicalize_platform_root(root: &Path, label: &str) -> Result<PathBuf> {
    if !root.is_absolute() {
        bail!("{label} must be absolute");
    }
    ensure_path_chain_has_no_links(root, label)?;
    std::fs::canonicalize(root)
        .map_err(|error| anyhow::anyhow!("Could not resolve {label} '{}': {error}", root.display()))
}

fn validate_codex_platform_transcript_path(
    path: &Path,
    session_id: &str,
    trusted_sessions_root: &Path,
) -> Result<std::fs::File> {
    if !path.is_absolute() {
        bail!("Codex transcript path is not absolute");
    }
    let expected_suffix = format!("-{session_id}.jsonl");
    let basename = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    if !basename.starts_with("rollout-") || !basename.ends_with(&expected_suffix) {
        bail!("Codex transcript filename does not match the current task");
    }
    let sessions_root = canonicalize_platform_root(trusted_sessions_root, "Codex sessions root")?;
    ensure_path_chain_has_no_links(path, "Codex transcript")?;
    let canonical_path = std::fs::canonicalize(path).map_err(|error| {
        anyhow::anyhow!(
            "Could not resolve Codex transcript '{}': {error}",
            path.display()
        )
    })?;
    if !canonical_path.starts_with(&sessions_root) {
        bail!("Codex transcript is outside the canonical sessions root");
    }
    let (file, _) = open_regular_file_no_follow(path, "Codex transcript")?;
    Ok(file)
}

fn resolve_codex_platform_transcript(
    session_id: &str,
    trusted_sessions_root: &Path,
) -> Result<String> {
    if session_id.is_empty()
        || !session_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        bail!("CODEX_THREAD_ID is not safe for transcript lookup");
    }
    let sessions_root = canonicalize_platform_root(trusted_sessions_root, "Codex sessions root")?;
    let expected_suffix = format!("-{session_id}.jsonl");
    let mut matches = Vec::new();
    let mut pending = vec![sessions_root.clone()];
    let mut entries_seen = 0_usize;
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).map_err(|error| {
            anyhow::anyhow!(
                "Could not enumerate Codex transcript directory '{}': {error}",
                directory.display()
            )
        })? {
            let entry = entry.map_err(|error| {
                anyhow::anyhow!("Could not inspect Codex transcript entry: {error}")
            })?;
            entries_seen += 1;
            if entries_seen > MAX_CODEX_TRANSCRIPT_ENTRIES {
                bail!("Codex transcript lookup exceeded its bounded entry limit");
            }
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
                anyhow::anyhow!(
                    "Could not inspect Codex transcript entry '{}': {error}",
                    path.display()
                )
            })?;
            let basename = entry.file_name();
            let basename = basename.to_string_lossy();
            if basename.starts_with("rollout-") && basename.ends_with(&expected_suffix) {
                matches.push(path.clone());
                if matches.len() > 1 {
                    bail!("Multiple Codex transcripts match the current CODEX_THREAD_ID");
                }
            }
            if metadata.is_dir() && !metadata_is_link_or_reparse(&metadata) {
                pending.push(path);
            }
        }
    }
    let path = matches
        .pop()
        .ok_or_else(|| anyhow::anyhow!("Could not locate the active Codex transcript"))?;
    validate_codex_platform_transcript_path(&path, session_id, &sessions_root)?;
    Ok(path.to_string_lossy().to_string())
}

fn validate_stopped_claude_transcript_path(
    path: &Path,
    session_id: &str,
    trusted_projects_root: &Path,
) -> Result<std::fs::File> {
    if !path.is_absolute() {
        bail!("Stopped Claude transcript path is not absolute");
    }
    if path.file_name().and_then(|value| value.to_str()) != Some(&format!("{session_id}.jsonl")) {
        bail!("Stopped Claude transcript filename does not match the stopped session");
    }
    let canonical_root = canonicalize_platform_root(trusted_projects_root, "Claude projects root")?;
    ensure_path_chain_has_no_links(path, "Stopped Claude transcript")?;
    let canonical_path = std::fs::canonicalize(path).map_err(|error| {
        anyhow::anyhow!(
            "Could not resolve stopped Claude transcript '{}': {error}",
            path.display()
        )
    })?;
    if !canonical_path.starts_with(&canonical_root) {
        bail!("Stopped Claude transcript is outside the canonical Claude projects root");
    }
    let (file, _) = open_regular_file_no_follow(path, "Stopped Claude transcript")?;
    Ok(file)
}

fn registry_authorizes_platform_migration(
    registry_bytes: &[u8],
    target_name: &str,
    current_tool: &str,
    current_directory: &str,
    current_thread_id: &str,
) -> Result<String> {
    let registry = parse_authority_json(registry_bytes, "DEV-REGISTRY.json")?;
    let devs = registry
        .get("devs")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("DEV-REGISTRY.json has no devs array"))?;
    let accounts = registry
        .get("accounts")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("DEV-REGISTRY.json has no accounts object"))?;

    let mut seen_roles = HashSet::new();
    let mut target = None;
    for dev in devs {
        let role = dev
            .get("role")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("DEV-REGISTRY.json contains a dev without a role"))?;
        if !seen_roles.insert(role.to_ascii_lowercase()) {
            bail!("DEV-REGISTRY.json contains duplicate role '{role}'");
        }
        let parked = match dev.get("parked") {
            None => false,
            Some(serde_json::Value::Bool(value)) => *value,
            Some(_) => bail!("DEV-REGISTRY.json role '{role}' has a malformed parked flag"),
        };
        let directory = dev
            .get("dir")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("DEV-REGISTRY.json role '{role}' has no directory"))?;
        if !parked && role != target_name && same_path(directory, current_directory) {
            bail!(
                "DEV-REGISTRY.json assigns directory '{current_directory}' to both '{target_name}' and '{role}'"
            );
        }
        if role == target_name {
            if target.is_some() {
                bail!("DEV-REGISTRY.json contains duplicate role '{target_name}'");
            }
            target = Some((dev, parked));
        }
    }

    let (target, parked) = target.ok_or_else(|| {
        anyhow::anyhow!("DEV-REGISTRY.json does not declare role '{target_name}'")
    })?;
    if parked {
        bail!("DEV-REGISTRY.json role '{target_name}' is parked");
    }
    let platform = target
        .get("platform")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("DEV-REGISTRY.json role '{target_name}' has no platform"))?;
    if platform != current_tool {
        bail!(
            "DEV-REGISTRY.json assigns role '{target_name}' to platform '{platform}', not '{current_tool}'"
        );
    }
    let directory = target
        .get("dir")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if !same_path(directory, current_directory) {
        bail!(
            "DEV-REGISTRY.json assigns role '{target_name}' to directory '{directory}', not '{current_directory}'"
        );
    }
    let account = target
        .get("account")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("DEV-REGISTRY.json role '{target_name}' has no account"))?;
    let account_platform = accounts
        .get(account)
        .and_then(serde_json::Value::as_object)
        .and_then(|record| record.get("platform"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            anyhow::anyhow!("DEV-REGISTRY.json account '{account}' has no platform declaration")
        })?;
    if account_platform != platform {
        bail!(
            "DEV-REGISTRY.json account '{account}' belongs to platform '{account_platform}', not '{platform}'"
        );
    }
    let relay = target
        .get("relay")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| {
            anyhow::anyhow!("DEV-REGISTRY.json role '{target_name}' has no valid relay authority")
        })?;
    if relay.get("kind").and_then(serde_json::Value::as_str) != Some("codex_task") {
        bail!("DEV-REGISTRY.json role '{target_name}' relay kind is not 'codex_task'");
    }
    if relay.get("thread_id").and_then(serde_json::Value::as_str) != Some(current_thread_id) {
        bail!(
            "DEV-REGISTRY.json role '{target_name}' relay does not authorize current CODEX_THREAD_ID"
        );
    }
    if relay.get("host_id").and_then(serde_json::Value::as_str) != Some("local") {
        bail!("DEV-REGISTRY.json role '{target_name}' relay host is not 'local'");
    }
    let owner = target
        .get("owner")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("DEV-REGISTRY.json role '{target_name}' has no owner"))?;
    Ok(owner.to_string())
}

fn role_file_authorizes_platform_migration(
    role_bytes: &[u8],
    target_name: &str,
    expected_owner: &str,
) -> Result<()> {
    let role = parse_authority_json(role_bytes, ".estate/role.json")?;
    if role
        .get("estate_schema")
        .and_then(serde_json::Value::as_u64)
        != Some(1)
    {
        bail!(".estate/role.json does not use estate_schema 1");
    }
    if role.get("role").and_then(serde_json::Value::as_str) != Some(target_name) {
        bail!(".estate/role.json does not declare role '{target_name}'");
    }
    if role.get("owner").and_then(serde_json::Value::as_str) != Some(expected_owner) {
        bail!(".estate/role.json owner does not match DEV-REGISTRY.json");
    }
    Ok(())
}

fn validate_stopped_claude_transcript(
    transcript_path: &str,
    session_id: &str,
    expected_directory: &str,
    trusted_projects_root: &Path,
) -> Result<()> {
    let path = Path::new(transcript_path);
    let file = validate_stopped_claude_transcript_path(path, session_id, trusted_projects_root)?;
    let mut reader = BufReader::new(file).take(MAX_CODEX_SESSION_META_BYTES + 1);
    let mut line = String::new();
    let mut total_bytes = 0_u64;

    loop {
        line.clear();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            break;
        }
        total_bytes = total_bytes.saturating_add(bytes as u64);
        if total_bytes > MAX_CODEX_SESSION_META_BYTES {
            bail!("Stopped Claude transcript provenance is missing or too large");
        }
        let record_text = line.trim_end_matches(['\r', '\n']);
        if record_text.is_empty() {
            continue;
        }
        let record =
            parse_authority_json(record_text.as_bytes(), "Stopped Claude transcript record")?;
        let Some(object) = record.as_object() else {
            bail!("Stopped Claude transcript record is not a JSON object");
        };
        if let Some(record_session_id) = object
            .get("sessionId")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            && record_session_id != session_id
        {
            bail!("Stopped Claude transcript does not match the stopped session");
        }

        let Some(entrypoint) = object.get("entrypoint") else {
            continue;
        };
        let Some(entrypoint) = entrypoint.as_str() else {
            bail!("Stopped Claude transcript has malformed entrypoint provenance");
        };
        if entrypoint != "claude-desktop" {
            bail!("Stopped Claude transcript is not from Claude Desktop");
        }
        if object.get("sessionId").and_then(serde_json::Value::as_str) != Some(session_id) {
            bail!("Stopped Claude Desktop provenance does not match the stopped session");
        }
        let cwd = object
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if cwd.is_empty() || !same_path(cwd, expected_directory) {
            bail!("Stopped Claude Desktop provenance does not match the authorized directory");
        }
        if object.get("parentUuid") != Some(&serde_json::Value::Null)
            || object
                .get("isSidechain")
                .and_then(serde_json::Value::as_bool)
                != Some(false)
            || !json_identity_field_is_empty(object.get("agentId"))
            || !json_identity_field_is_empty(object.get("agent_id"))
        {
            bail!("Stopped Claude Desktop provenance belongs to a child or agent task");
        }
        return Ok(());
    }

    bail!("Stopped Claude transcript has no top-level Claude Desktop provenance")
}

fn platform_migration_background_is_valid(value: Option<&serde_json::Value>) -> bool {
    match value {
        None | Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::Number(value)) => matches!(value.as_i64(), Some(0 | 1)),
        _ => false,
    }
}

fn validate_codex_platform_migration(
    db: &HcomDb,
    target_name: &str,
    ctx: &HcomContext,
    resolved_session_id: Option<&str>,
    current_name: &str,
    registry_path: &Path,
) -> Result<CodexPlatformMigrationProof> {
    let user_home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not resolve the local user profile"))?;
    let codex_sessions_root = user_home.join(".codex").join("sessions");
    let claude_projects_root = user_home.join(".claude").join("projects");
    validate_codex_platform_migration_with_registry_anchor(
        db,
        target_name,
        ctx,
        resolved_session_id,
        current_name,
        registry_path,
        CodexPlatformMigrationTrust {
            registry_path: Path::new(TRUSTED_PLATFORM_REGISTRY_PATH),
            codex_sessions_root: &codex_sessions_root,
            claude_projects_root: &claude_projects_root,
        },
    )
}

struct CodexPlatformMigrationTrust<'a> {
    registry_path: &'a Path,
    codex_sessions_root: &'a Path,
    claude_projects_root: &'a Path,
}

fn validate_codex_platform_migration_with_registry_anchor(
    db: &HcomDb,
    target_name: &str,
    ctx: &HcomContext,
    resolved_session_id: Option<&str>,
    current_name: &str,
    registry_path: &Path,
    trust: CodexPlatformMigrationTrust<'_>,
) -> Result<CodexPlatformMigrationProof> {
    let registry_path = validate_platform_registry_anchor(registry_path, trust.registry_path)?;
    if target_name != TRUSTED_PLATFORM_MIGRATION_ROLE {
        bail!(
            "--migrate-platform is narrowly authorized only for '{TRUSTED_PLATFORM_MIGRATION_ROLE}'"
        );
    }
    let codex_sessions_root =
        canonicalize_platform_root(trust.codex_sessions_root, "Codex sessions root")?;
    let claude_projects_root =
        canonicalize_platform_root(trust.claude_projects_root, "Claude projects root")?;
    if ctx.tool != crate::tool::Tool::Codex
        || ctx.is_launched
        || ctx.is_background
        || ctx.is_fork
        || ctx.process_id.is_some()
        || ctx.launched_by.is_some()
        || ctx.launch_batch_id.is_some()
        || ctx.launch_event_id.is_some()
    {
        bail!(
            "--migrate-platform is supported only from an unlaunched top-level Codex Desktop task"
        );
    }
    if !current_name.is_empty() {
        bail!(
            "Refusing to migrate '{target_name}': this task is already bound as '{current_name}'"
        );
    }
    if db.get_instance_full(target_name)?.is_some() {
        bail!("Refusing to migrate '{target_name}': the identity is still live");
    }
    if !ctx.cwd.is_absolute() || !ctx.cwd.is_dir() {
        bail!(
            "Refusing to migrate '{target_name}': current directory '{}' is not an existing absolute directory",
            ctx.cwd.display()
        );
    }
    if registry_path.file_name().and_then(|name| name.to_str()) != Some("DEV-REGISTRY.json") {
        bail!("--registry must name DEV-REGISTRY.json");
    }

    let thread_id = resolve_codex_session_id(ctx)
        .ok_or_else(|| anyhow::anyhow!("Codex platform migration requires CODEX_THREAD_ID"))?;
    let session_env = ctx
        .raw_env
        .get("CODEX_SESSION_ID")
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Codex platform migration requires CODEX_SESSION_ID"))?;
    if thread_id != session_env {
        bail!(
            "Refusing Codex platform migration: CODEX_THREAD_ID '{}' conflicts with CODEX_SESSION_ID '{}'",
            thread_id,
            session_env
        );
    }
    if resolved_session_id != Some(thread_id.as_str()) {
        bail!("Refusing Codex platform migration: current session identity is not canonical");
    }

    let current_directory = ctx.cwd.to_string_lossy().to_string();
    let registry_bytes = read_platform_authority(&registry_path, "DEV-REGISTRY.json")?;
    let owner = registry_authorizes_platform_migration(
        &registry_bytes,
        target_name,
        ctx.tool.as_str(),
        &current_directory,
        &thread_id,
    )?;
    let role_path = ctx.cwd.join(".estate").join("role.json");
    let role_bytes = read_platform_authority(&role_path, ".estate/role.json")?;
    role_file_authorizes_platform_migration(&role_bytes, target_name, &owner)?;

    let transcript_path = resolve_codex_platform_transcript(&thread_id, &codex_sessions_root)?;
    validate_codex_session_meta(
        &transcript_path,
        &thread_id,
        &current_directory,
        &codex_sessions_root,
    )?;

    let latest_life: Option<(i64, String)> = db
        .conn()
        .query_row(
            "SELECT id, data FROM events
             WHERE type = 'life' AND instance = ?1
             ORDER BY id DESC LIMIT 1",
            rusqlite::params![target_name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (stopped_event_id, stopped_event_data) = latest_life
        .ok_or_else(|| anyhow::anyhow!("No lifecycle identity found for '{target_name}'"))?;
    let data = parse_authority_json(stopped_event_data.as_bytes(), "Stopped lifecycle event")?;
    if data.get("action").and_then(serde_json::Value::as_str) != Some("stopped") {
        bail!("Refusing to migrate '{target_name}': latest lifecycle event is not stopped");
    }
    let snapshot = data
        .get("snapshot")
        .ok_or_else(|| anyhow::anyhow!("Stopped identity '{target_name}' has no snapshot"))?;
    let from_tool = snapshot
        .get("tool")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if from_tool != "claude" {
        bail!(
            "Refusing to migrate '{target_name}': this recovery supports stopped Claude to Codex only"
        );
    }
    if !json_identity_field_is_empty(snapshot.get("origin_device_id")) {
        bail!("Refusing to migrate '{target_name}': stopped identity is remote");
    }
    if !json_identity_field_is_empty(snapshot.get("parent_name"))
        || !json_identity_field_is_empty(snapshot.get("parent_session_id"))
        || !json_identity_field_is_empty(snapshot.get("agent_id"))
    {
        bail!("Refusing to migrate '{target_name}': stopped identity is a child task");
    }
    if !platform_migration_background_is_valid(snapshot.get("background")) {
        bail!("Refusing to migrate '{target_name}': stopped background marker is malformed");
    }
    if snapshot.get("name").and_then(serde_json::Value::as_str) != Some(target_name) {
        bail!("Refusing to migrate '{target_name}': stopped snapshot does not name this role");
    }
    let stopped_directory = snapshot
        .get("directory")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if stopped_directory.is_empty() || !same_path(stopped_directory, &current_directory) {
        bail!("Refusing to migrate '{target_name}': stopped and current directories do not match");
    }
    let from_session_id = snapshot
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Stopped identity '{target_name}' has no session"))?
        .to_string();
    if from_session_id == thread_id {
        bail!("Refusing to migrate '{target_name}': platform sessions are not distinct");
    }
    let from_transcript_path = snapshot
        .get("transcript_path")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Stopped identity '{target_name}' has no transcript"))?
        .to_string();
    validate_stopped_claude_transcript(
        &from_transcript_path,
        &from_session_id,
        &current_directory,
        &claude_projects_root,
    )?;
    let last_event_id = snapshot
        .get("last_event_id")
        .and_then(serde_json::Value::as_i64)
        .filter(|value| *value >= 0)
        .ok_or_else(|| {
            anyhow::anyhow!("Stopped identity '{target_name}' has no valid event cursor")
        })?;
    let current_event_id = db.get_last_event_id();
    if last_event_id >= stopped_event_id || last_event_id > current_event_id {
        bail!(
            "Refusing to migrate '{target_name}': stopped event cursor is not before its lifecycle event"
        );
    }

    for session_id in [&thread_id, &from_session_id] {
        if let Some(owner) = db.get_session_binding(session_id)? {
            bail!(
                "Refusing to migrate '{target_name}': session '{session_id}' is already bound to '{owner}'"
            );
        }
        let instance_owner = db
            .conn()
            .query_row(
                "SELECT name FROM instances WHERE session_id = ?1 LIMIT 1",
                rusqlite::params![session_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(owner) = instance_owner {
            bail!(
                "Refusing to migrate '{target_name}': session '{session_id}' is already owned by '{owner}'"
            );
        }
    }
    let binding_collision: bool = db.conn().query_row(
        "SELECT EXISTS(
             SELECT 1 FROM session_bindings WHERE instance_name = ?1
             UNION ALL
             SELECT 1 FROM process_bindings
             WHERE instance_name = ?1 OR session_id IN (?2, ?3)
         )",
        rusqlite::params![target_name, &thread_id, &from_session_id],
        |row| row.get(0),
    )?;
    if binding_collision {
        bail!("Refusing to migrate '{target_name}': a live binding already exists");
    }
    for row in db.iter_instances_full()? {
        if same_path(&row.directory, &current_directory) {
            bail!(
                "Refusing to migrate '{target_name}': live identity '{}' already occupies this directory",
                row.name
            );
        }
    }
    let legacy_endpoints = validate_legacy_platform_endpoints(
        db.conn(),
        target_name,
        crate::shared::time::now_epoch_f64(),
    )?;

    Ok(CodexPlatformMigrationProof {
        session_id: thread_id,
        transcript_path,
        stopped_event_id,
        stopped_event_data,
        last_event_id,
        from_tool: from_tool.to_string(),
        from_session_id,
        from_transcript_path,
        registry_path,
        registry_bytes,
        role_path,
        role_bytes,
        codex_sessions_root,
        claude_projects_root,
        legacy_endpoints,
    })
}

fn validate_codex_relocation(
    db: &HcomDb,
    target_name: &str,
    ctx: &HcomContext,
    resolved_session_id: Option<&str>,
    current_name: &str,
) -> Result<CodexRelocationProof> {
    if ctx.tool != crate::tool::Tool::Codex {
        bail!("--relocate is supported only from the owning Codex Desktop task");
    }
    if db.get_instance_full(target_name)?.is_some() {
        bail!("Refusing to relocate '{target_name}': the identity is still live");
    }
    if !current_name.is_empty() && current_name != target_name {
        bail!(
            "Refusing to relocate '{target_name}': this task is already bound as '{current_name}'"
        );
    }
    if !ctx.cwd.is_absolute() || !ctx.cwd.is_dir() {
        bail!(
            "Refusing to relocate '{target_name}': current directory '{}' is not an existing absolute directory",
            ctx.cwd.display()
        );
    }

    let thread_id = resolve_codex_session_id(ctx)
        .ok_or_else(|| anyhow::anyhow!("Codex relocation requires CODEX_THREAD_ID"))?;
    let session_env = ctx
        .raw_env
        .get("CODEX_SESSION_ID")
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Codex relocation requires CODEX_SESSION_ID"))?;
    if thread_id != session_env {
        bail!(
            "Refusing Codex relocation: CODEX_THREAD_ID '{}' conflicts with CODEX_SESSION_ID '{}'",
            thread_id,
            session_env
        );
    }
    if resolved_session_id != Some(thread_id.as_str()) {
        bail!("Refusing Codex relocation: current session identity is not canonical");
    }
    if let Some(owner) = db.get_session_binding(&thread_id)? {
        bail!(
            "Refusing to relocate '{target_name}': session '{thread_id}' is already bound to '{owner}'"
        );
    }

    let (stopped_event_id, stopped_event_data): (i64, String) = db
        .conn()
        .query_row(
            "SELECT id, data FROM events
             WHERE type = 'life'
               AND instance = ?1
               AND json_extract(data, '$.action') = 'stopped'
             ORDER BY id DESC LIMIT 1",
            rusqlite::params![target_name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| anyhow::anyhow!("No stopped identity found for '{target_name}'"))?;
    let data: serde_json::Value = serde_json::from_str(&stopped_event_data)?;
    let snapshot = data
        .get("snapshot")
        .ok_or_else(|| anyhow::anyhow!("Stopped identity '{target_name}' has no snapshot"))?;

    if snapshot.get("tool").and_then(serde_json::Value::as_str) != Some("codex") {
        bail!("Refusing to relocate '{target_name}': stopped identity is not Codex");
    }
    if !json_identity_field_is_empty(snapshot.get("origin_device_id")) {
        bail!("Refusing to relocate '{target_name}': stopped identity is remote");
    }
    if !json_identity_field_is_empty(snapshot.get("parent_name"))
        || !json_identity_field_is_empty(snapshot.get("parent_session_id"))
        || !json_identity_field_is_empty(snapshot.get("agent_id"))
    {
        bail!("Refusing to relocate '{target_name}': stopped identity is a child task");
    }
    if snapshot.get("background").is_some_and(|value| {
        !value.is_null() && value.as_i64().is_none_or(|background| background != 0)
    }) {
        bail!("Refusing to relocate '{target_name}': stopped identity is not top-level");
    }
    if snapshot
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        != Some(thread_id.as_str())
    {
        bail!("Refusing to relocate '{target_name}': stopped session does not match this task");
    }

    let stopped_directory = snapshot
        .get("directory")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let current_directory = ctx.cwd.to_string_lossy();
    if stopped_directory.is_empty() || same_path(stopped_directory, &current_directory) {
        bail!(
            "Refusing to relocate '{target_name}': stopped and current directories are not distinct"
        );
    }

    let snapshot_transcript = snapshot
        .get("transcript_path")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Stopped identity '{target_name}' has no transcript"))?;
    let derived_transcript = crate::hooks::codex::derive_codex_transcript_path(&thread_id)
        .ok_or_else(|| anyhow::anyhow!("Could not locate the active Codex transcript"))?;
    if !same_path(snapshot_transcript, &derived_transcript) {
        bail!("Refusing to relocate '{target_name}': stopped and active transcript paths differ");
    }
    let relocation_sessions_root = std::env::var("CODEX_HOME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| paths::get_project_root().join(".codex"))
        .join("sessions");
    validate_codex_session_meta(
        &derived_transcript,
        &thread_id,
        stopped_directory,
        &relocation_sessions_root,
    )?;

    let last_event_id = snapshot
        .get("last_event_id")
        .and_then(serde_json::Value::as_i64)
        .filter(|value| *value >= 0)
        .ok_or_else(|| {
            anyhow::anyhow!("Stopped identity '{target_name}' has no valid event cursor")
        })?;

    Ok(CodexRelocationProof {
        session_id: thread_id,
        transcript_path: derived_transcript,
        stopped_event_id,
        stopped_event_data,
        last_event_id,
    })
}

fn json_identity_field_is_empty(value: Option<&serde_json::Value>) -> bool {
    match value {
        None | Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::String(value)) => value.is_empty(),
        Some(_) => false,
    }
}

fn commit_codex_relocation(
    db: &HcomDb,
    target_name: &str,
    ctx: &HcomContext,
    proof: &CodexRelocationProof,
) -> Result<()> {
    let directory = ctx.cwd.to_string_lossy().to_string();
    let launch_context =
        crate::hooks::codex::directory_override_launch_context(&directory, &proof.session_id);
    let created_at = crate::shared::time::now_epoch_f64();
    let status_time = crate::shared::time::now_epoch_i64();
    let wait_timeout = HcomConfig::effective_timeout();

    db.with_immediate_transaction(|txn| {
        let latest_stop = txn
            .query_row(
                "SELECT id, data FROM events
                 WHERE type = 'life'
                   AND instance = ?1
                   AND json_extract(data, '$.action') = 'stopped'
                 ORDER BY id DESC LIMIT 1",
                rusqlite::params![target_name],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        if latest_stop.as_ref()
            != Some(&(
                proof.stopped_event_id,
                proof.stopped_event_data.clone(),
            ))
        {
            bail!(
                "Refusing to relocate '{target_name}': stopped identity changed during verification"
            );
        }

        let target_exists: bool = txn.query_row(
            "SELECT EXISTS(SELECT 1 FROM instances WHERE name = ?1)",
            rusqlite::params![target_name],
            |row| row.get(0),
        )?;
        if target_exists {
            bail!(
                "Refusing to relocate '{target_name}': another task registered it during verification"
            );
        }

        let session_owner = txn
            .query_row(
                "SELECT instance_name FROM session_bindings WHERE session_id = ?1",
                rusqlite::params![&proof.session_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(owner) = session_owner {
            bail!(
                "Refusing to relocate '{target_name}': session is already bound to '{owner}'"
            );
        }

        let target_binding = txn
            .query_row(
                "SELECT session_id FROM session_bindings WHERE instance_name = ?1 LIMIT 1",
                rusqlite::params![target_name],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if target_binding.is_some() {
            bail!(
                "Refusing to relocate '{target_name}': target binding changed during verification"
            );
        }

        let instance_session_owner = txn
            .query_row(
                "SELECT name FROM instances WHERE session_id = ?1 LIMIT 1",
                rusqlite::params![&proof.session_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(owner) = instance_session_owner {
            bail!(
                "Refusing to relocate '{target_name}': session is already owned by '{owner}'"
            );
        }

        let process_collision: bool = txn.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM process_bindings
                 WHERE instance_name = ?1 OR session_id = ?2
             )",
            rusqlite::params![target_name, &proof.session_id],
            |row| row.get(0),
        )?;
        if process_collision {
            bail!(
                "Refusing to relocate '{target_name}': a process binding changed during verification"
            );
        }

        let initial_event_id = proof.last_event_id;

        txn.execute(
            "INSERT INTO instances (
                 name, session_id, last_event_id, last_stop, status, status_time,
                 status_context, directory, created_at, transcript_path, tool,
                 background, wait_timeout, name_announced, launch_context
             ) VALUES (
                 ?1, ?2, ?3, 0, 'inactive', ?4,
                 'new', ?5, ?6, ?7, 'codex',
                 0, ?8, 1, ?9
             )",
            rusqlite::params![
                target_name,
                &proof.session_id,
                initial_event_id,
                status_time,
                &directory,
                created_at,
                &proof.transcript_path,
                wait_timeout,
                &launch_context,
            ],
        )?;
        txn.execute(
            "INSERT INTO session_bindings (session_id, instance_name, created_at)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![&proof.session_id, target_name, created_at],
        )?;

        let written: (Option<String>, String, String, String, i64, String) = txn.query_row(
            "SELECT session_id, tool, directory, transcript_path, last_event_id, launch_context
             FROM instances WHERE name = ?1",
            rusqlite::params![target_name],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )?;
        let binding: String = txn.query_row(
            "SELECT instance_name FROM session_bindings WHERE session_id = ?1",
            rusqlite::params![&proof.session_id],
            |row| row.get(0),
        )?;
        if written.0.as_deref() != Some(proof.session_id.as_str())
            || written.1 != "codex"
            || written.2 != directory
            || written.3 != proof.transcript_path
            || written.4 != initial_event_id
            || written.5 != launch_context
            || binding != target_name
        {
            bail!("Relocated Codex identity failed transactional verification");
        }
        Ok(())
    })?;

    let row = db
        .get_instance_full(target_name)?
        .ok_or_else(|| anyhow::anyhow!("Relocated Codex identity disappeared after commit"))?;
    let binding = db.get_session_binding(&proof.session_id)?;
    if row.tool != "codex"
        || row.session_id.as_deref() != Some(proof.session_id.as_str())
        || !same_path(&row.directory, &directory)
        || !same_path(&row.transcript_path, &proof.transcript_path)
        || binding.as_deref() != Some(target_name)
        || crate::hooks::codex::pinned_directory_from_launch_context(
            row.launch_context.as_deref(),
            &proof.session_id,
        )
        .is_none_or(|pinned| !same_path(&pinned, &directory))
    {
        bail!("Relocated Codex identity failed final bound-state verification");
    }

    let _ = db.log_event(
        "life",
        target_name,
        &serde_json::json!({
            "action": "created",
            "by": "explicit-start-relocate-v1",
            "is_hcom_launched": false,
            "is_subagent": false,
            "parent_name": "",
        }),
    );
    Ok(())
}

fn commit_codex_platform_migration(
    db: &HcomDb,
    target_name: &str,
    ctx: &HcomContext,
    proof: &CodexPlatformMigrationProof,
) -> Result<()> {
    let directory = ctx.cwd.to_string_lossy().to_string();
    let launch_context =
        crate::hooks::codex::directory_override_launch_context(&directory, &proof.session_id);
    let created_at = crate::shared::time::now_epoch_f64();
    let status_time = crate::shared::time::now_epoch_i64();
    let wait_timeout = HcomConfig::effective_timeout();
    let hcom_config = HcomConfig::load(None)?;
    let auto_subscribe = hcom_config.auto_subscribe;
    let stopped_lineage_key = format!("claude_lineage_validated:{}", proof.from_session_id);
    let current_lineage_key = format!("claude_lineage_validated:{}", proof.session_id);
    let migration_event = serde_json::json!({
        "action": "created",
        "by": "explicit-start-migrate-platform-v1",
        "reason": "registry_authorized_platform_migration",
        "is_hcom_launched": false,
        "is_subagent": false,
        "parent_name": "",
        "from_tool": proof.from_tool.as_str(),
        "from_session_id": proof.from_session_id.as_str(),
    });
    let migration_event_data = serde_json::to_string(&migration_event)?;
    let migration_event_timestamp = crate::shared::time::now_iso();

    let migration_event_id = db.with_immediate_transaction(|txn| {
        if read_platform_authority(&proof.registry_path, "DEV-REGISTRY.json")?
            != proof.registry_bytes
            || read_platform_authority(&proof.role_path, ".estate/role.json")?
                != proof.role_bytes
        {
            bail!(
                "Refusing to migrate '{target_name}': platform authority changed during verification"
            );
        }
        let resolved_transcript =
            resolve_codex_platform_transcript(&proof.session_id, &proof.codex_sessions_root)?;
        if !same_path(&resolved_transcript, &proof.transcript_path) {
            bail!(
                "Refusing to migrate '{target_name}': Codex transcript changed during verification"
            );
        }
        validate_codex_session_meta(
            &proof.transcript_path,
            &proof.session_id,
            &directory,
            &proof.codex_sessions_root,
        )?;
        validate_stopped_claude_transcript(
            &proof.from_transcript_path,
            &proof.from_session_id,
            &directory,
            &proof.claude_projects_root,
        )?;

        let latest_life = txn
            .query_row(
                "SELECT id, data FROM events
                 WHERE type = 'life' AND instance = ?1
                 ORDER BY id DESC LIMIT 1",
                rusqlite::params![target_name],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        if latest_life.as_ref()
            != Some(&(
                proof.stopped_event_id,
                proof.stopped_event_data.clone(),
            ))
        {
            bail!(
                "Refusing to migrate '{target_name}': stopped identity changed during verification"
            );
        }

        let target_exists: bool = txn.query_row(
            "SELECT EXISTS(SELECT 1 FROM instances WHERE name = ?1)",
            rusqlite::params![target_name],
            |row| row.get(0),
        )?;
        if target_exists {
            bail!(
                "Refusing to migrate '{target_name}': another task registered it during verification"
            );
        }

        for session_id in [&proof.session_id, &proof.from_session_id] {
            let session_owner = txn
                .query_row(
                    "SELECT instance_name FROM session_bindings WHERE session_id = ?1",
                    rusqlite::params![session_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if let Some(owner) = session_owner {
                bail!(
                    "Refusing to migrate '{target_name}': session '{session_id}' is already bound to '{owner}'"
                );
            }
            let instance_owner = txn
                .query_row(
                    "SELECT name FROM instances WHERE session_id = ?1 LIMIT 1",
                    rusqlite::params![session_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if let Some(owner) = instance_owner {
                bail!(
                    "Refusing to migrate '{target_name}': session '{session_id}' is already owned by '{owner}'"
                );
            }
        }

        let target_binding = txn
            .query_row(
                "SELECT session_id FROM session_bindings WHERE instance_name = ?1 LIMIT 1",
                rusqlite::params![target_name],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if target_binding.is_some() {
            bail!(
                "Refusing to migrate '{target_name}': target binding changed during verification"
            );
        }
        let process_collision: bool = txn.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM process_bindings
                 WHERE instance_name = ?1 OR session_id IN (?2, ?3)
             )",
            rusqlite::params![target_name, &proof.session_id, &proof.from_session_id],
            |row| row.get(0),
        )?;
        if process_collision {
            bail!(
                "Refusing to migrate '{target_name}': a process binding changed during verification"
            );
        }

        let live_directories: Vec<(String, String)> = {
            let mut statement = txn.prepare("SELECT name, directory FROM instances")?;
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        if let Some((name, _)) = live_directories
            .iter()
            .find(|(_, live_directory)| same_path(live_directory, &directory))
        {
            bail!(
                "Refusing to migrate '{target_name}': live identity '{name}' occupied this directory during verification"
            );
        }

        let current_endpoints = validate_legacy_platform_endpoints(
            txn,
            target_name,
            crate::shared::time::now_epoch_f64(),
        )?;
        if current_endpoints != proof.legacy_endpoints {
            bail!(
                "Refusing to migrate '{target_name}': legacy endpoint proof changed during verification"
            );
        }

        // A stopped holder may have left a port belonging to its old process.
        // Never carry that wake endpoint across a platform boundary.
        let deleted_endpoints = txn.execute(
            "DELETE FROM notify_endpoints WHERE instance = ?1",
            rusqlite::params![target_name],
        )?;
        if deleted_endpoints != proof.legacy_endpoints.len() {
            bail!(
                "Refusing to migrate '{target_name}': legacy endpoint cleanup was not exact"
            );
        }
        txn.execute(
            "DELETE FROM kv WHERE key IN (?1, ?2)",
            rusqlite::params![&stopped_lineage_key, &current_lineage_key],
        )?;
        txn.execute(
            "DELETE FROM claude_actor_capabilities
             WHERE instance_name = ?1 OR session_id IN (?2, ?3)",
            rusqlite::params![target_name, &proof.from_session_id, &proof.session_id],
        )?;

        txn.execute(
            "INSERT INTO instances (
                 name, session_id, last_event_id, last_stop, status, status_time,
                 status_context, directory, created_at, transcript_path, tool,
                 background, wait_timeout, name_announced, launch_context
             ) VALUES (
                 ?1, ?2, ?3, 0, 'inactive', ?4,
                 'new', ?5, ?6, ?7, 'codex',
                 0, ?8, 1, ?9
             )",
            rusqlite::params![
                target_name,
                &proof.session_id,
                proof.last_event_id,
                status_time,
                &directory,
                created_at,
                &proof.transcript_path,
                wait_timeout,
                &launch_context,
            ],
        )?;
        txn.execute(
            "INSERT INTO session_bindings (session_id, instance_name, created_at)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![&proof.session_id, target_name, created_at],
        )?;

        let written: (Option<String>, String, String, String, i64, String) = txn.query_row(
            "SELECT session_id, tool, directory, transcript_path, last_event_id, launch_context
             FROM instances WHERE name = ?1",
            rusqlite::params![target_name],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )?;
        let binding: String = txn.query_row(
            "SELECT instance_name FROM session_bindings WHERE session_id = ?1",
            rusqlite::params![&proof.session_id],
            |row| row.get(0),
        )?;
        if written.0.as_deref() != Some(proof.session_id.as_str())
            || written.1 != "codex"
            || written.2 != directory
            || written.3 != proof.transcript_path
            || written.4 != proof.last_event_id
            || written.5 != launch_context
            || binding != target_name
        {
            bail!("Platform-migrated Codex identity failed transactional verification");
        }
        txn.execute(
            "INSERT INTO events (timestamp, type, instance, data)
             VALUES (?1, 'life', ?2, ?3)",
            rusqlite::params![
                &migration_event_timestamp,
                target_name,
                &migration_event_data
            ],
        )?;
        let event_id = txn.last_insert_rowid();
        crate::db::subscriptions::replace_default_event_subscriptions(
            txn,
            target_name,
            &auto_subscribe,
            created_at,
            event_id,
        )?;
        Ok(event_id)
    })?;

    let row = db
        .get_instance_full(target_name)?
        .ok_or_else(|| anyhow::anyhow!("Platform-migrated Codex identity disappeared"))?;
    let binding = db.get_session_binding(&proof.session_id)?;
    if row.tool != "codex"
        || row.session_id.as_deref() != Some(proof.session_id.as_str())
        || !same_path(&row.directory, &directory)
        || !same_path(&row.transcript_path, &proof.transcript_path)
        || row.last_event_id != proof.last_event_id
        || binding.as_deref() != Some(target_name)
        || crate::hooks::codex::pinned_directory_from_launch_context(
            row.launch_context.as_deref(),
            &proof.session_id,
        )
        .is_none_or(|pinned| !same_path(&pinned, &directory))
    {
        bail!("Platform-migrated Codex identity failed final bound-state verification");
    }

    crate::db::subscriptions::process_logged_event(
        db,
        migration_event_id,
        "life",
        target_name,
        &migration_event,
    );
    Ok(())
}

fn validate_codex_session_meta(
    transcript_path: &str,
    session_id: &str,
    stopped_directory: &str,
    trusted_sessions_root: &Path,
) -> Result<()> {
    let file = validate_codex_platform_transcript_path(
        Path::new(transcript_path),
        session_id,
        trusted_sessions_root,
    )?;
    let mut reader = BufReader::new(file).take(MAX_CODEX_SESSION_META_BYTES + 1);
    let mut first_line = String::new();
    let bytes = reader.read_line(&mut first_line)?;
    if bytes == 0 || bytes as u64 > MAX_CODEX_SESSION_META_BYTES {
        bail!("Codex transcript session metadata is missing or too large");
    }
    let meta = parse_authority_json(
        first_line.trim_end().as_bytes(),
        "Codex transcript session metadata",
    )?;
    let payload = meta
        .get("payload")
        .ok_or_else(|| anyhow::anyhow!("Codex transcript has no session metadata payload"))?;
    if meta.get("type").and_then(serde_json::Value::as_str) != Some("session_meta")
        || payload.get("id").and_then(serde_json::Value::as_str) != Some(session_id)
        || payload
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            != Some(session_id)
        || payload
            .get("originator")
            .and_then(serde_json::Value::as_str)
            != Some("Codex Desktop")
        || payload.get("source").and_then(serde_json::Value::as_str) != Some("vscode")
        || payload
            .get("thread_source")
            .and_then(serde_json::Value::as_str)
            != Some("user")
        || payload
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .is_none_or(|cwd| !same_path(cwd, stopped_directory))
    {
        bail!("Codex transcript does not describe this top-level Desktop task");
    }
    for field in [
        "parent_thread_id",
        "parent_session_id",
        "source_thread_id",
        "forked_from",
    ] {
        if payload
            .get(field)
            .is_some_and(|value| !value.is_null() && value.as_str().is_none_or(|s| !s.is_empty()))
        {
            bail!("Codex transcript describes a child or forked task");
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RebindTargetMetadata {
    tool: String,
    directory: String,
    last_event_id: i64,
}

fn ensure_rebind_compatible(
    target_name: &str,
    meta: &RebindTargetMetadata,
    ctx: &HcomContext,
    allow_directory_relocation: bool,
) -> Result<()> {
    let current_tool = ctx.tool.as_str();
    if !meta.tool.is_empty() && meta.tool != current_tool {
        bail!(
            "Refusing to reclaim '{target_name}': latest identity used tool '{}' but current session is '{}'",
            meta.tool,
            current_tool
        );
    }

    let current_dir = ctx.cwd.to_string_lossy();
    if !allow_directory_relocation
        && !meta.directory.is_empty()
        && !same_path(&meta.directory, &current_dir)
    {
        bail!(
            "Refusing to reclaim '{target_name}': latest identity used directory '{}' but current session is '{}'",
            meta.directory,
            current_dir
        );
    }

    Ok(())
}

fn same_path(left: &str, right: &str) -> bool {
    normalize_path_for_compare(left) == normalize_path_for_compare(right)
}

fn normalize_path_for_compare(path: &str) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
}

/// Load rebind metadata from the live row first, then the latest stopped snapshot.
fn load_rebind_target_metadata(db: &HcomDb, name: &str) -> Result<RebindTargetMetadata> {
    if let Some(inst) = db.get_instance_full(name)? {
        return Ok(RebindTargetMetadata {
            tool: inst.tool,
            directory: inst.directory,
            last_event_id: inst.last_event_id,
        });
    }

    let mut stmt = db.conn().prepare(
        "SELECT data FROM events WHERE type='life' AND instance=? ORDER BY id DESC LIMIT 10",
    )?;

    let rows: Vec<String> = stmt
        .query_map(rusqlite::params![name], |row| row.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .collect();

    for data_str in &rows {
        if let Ok(data) = serde_json::from_str::<serde_json::Value>(data_str)
            && data.get("action").and_then(|v| v.as_str()) == Some("stopped")
            && let Some(snapshot) = data.get("snapshot")
        {
            return Ok(RebindTargetMetadata {
                tool: snapshot
                    .get("tool")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                directory: snapshot
                    .get("directory")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                last_event_id: snapshot
                    .get("last_event_id")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0),
            });
        }
    }

    bail!("No rebind metadata found for '{}'", name)
}

/// Resolve the Claude session id visible to a CLI invocation.
///
/// Two sources, in order:
/// - `HCOM_CLAUDE_UNIX_SESSION_ID`: hcom's own SessionStart hook appends this
///   export to `CLAUDE_ENV_FILE`, which Claude runs before each Bash command.
/// - `CLAUDE_CODE_SESSION_ID`: Claude sets this directly in every Bash and
///   PowerShell subprocess, and it matches the `session_id` hooks receive.
///
/// The env-file round trip is the fragile one: it needs `CLAUDE_ENV_FILE` to
/// exist and our SessionStart to have run in this session generation. Without
/// the second source, a session that misses it cannot be recognized on a repeat
/// `hcom start`, which then mints a SECOND identity — the first stays bound to
/// nothing and later reports as launch_failed. Both values are set by Claude
/// for the session running this command, so either one binds identity.
fn resolve_claude_session_id(env: &HashMap<String, String>) -> Option<String> {
    ["HCOM_CLAUDE_UNIX_SESSION_ID", "CLAUDE_CODE_SESSION_ID"]
        .into_iter()
        .find_map(|key| env.get(key).filter(|value| !value.is_empty()).cloned())
}

/// Resolve the stable Codex task identity exposed to commands inside Desktop.
fn resolve_codex_session_id(ctx: &HcomContext) -> Option<String> {
    ctx.codex_thread_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

/// Live local Claude instances in this directory that no session id points at.
///
/// These are the plausible earlier identities of a session that exposes no id
/// of its own — the only useful thing to say when hcom cannot recognize it.
fn unbound_claude_candidates(db: &HcomDb, ctx: &HcomContext, exclude: &str) -> Vec<String> {
    let cwd = ctx.cwd.to_string_lossy();
    let mut rows: Vec<InstanceRow> = db
        .iter_instances_full()
        .unwrap_or_default()
        .into_iter()
        .filter(|row| {
            row.tool == "claude"
                && row.status != "stopped"
                && row.name != exclude
                && row.directory == cwd
                && row.session_id.is_none()
                && row.parent_name.is_none()
                && !crate::instances::is_remote_instance(row)
        })
        .collect();
    rows.sort_by(|a, b| b.created_at.total_cmp(&a.created_at));
    rows.truncate(4);
    rows.into_iter().map(|row| row.name).collect()
}

/// Path C: Bare start — detect tool or create adhoc instance.
fn start_bare(
    db: &HcomDb,
    hcom_dir: &std::path::Path,
    ctx: &HcomContext,
    explicit_name: Option<&str>,
) -> Result<i32> {
    let explicit_name = explicit_name
        .map(|name| identity::resolve_display_name(db, name).unwrap_or_else(|| name.to_string()));
    let explicit_name = explicit_name.as_deref();

    // Skip vanilla detection if --name is provided with an existing instance
    let has_valid_identity = explicit_name
        .and_then(|n| db.get_instance_full(n).ok().flatten())
        .is_some();

    // Vanilla tool detection: auto-install hooks for unmanaged AI tools.
    // Identity is already canonical on HcomContext, so route every released
    // hook-bearing integration through the typed Tool hook adapter. This keeps
    // bare `hcom start` aligned with `hcom hooks add` as integrations evolve.
    if !has_valid_identity && ctx.detect_vanilla_tool().is_some() {
        let vanilla_tool = ctx.tool;
        if !vanilla_tool.hooks().is_empty() && !vanilla_tool.verify_hooks_installed(false) {
            println!("Installing {} hooks...", vanilla_tool.as_str());
            let include_perms = crate::config::load_config_snapshot().core.auto_approve;
            match vanilla_tool.try_setup_hooks(include_perms) {
                Ok(()) => {
                    println!(
                        "\nRestart {} to enable automatic message delivery.",
                        vanilla_tool.spec().label
                    );
                    println!("Then run: hcom start");
                }
                Err(error) if error.is_empty() => {
                    eprintln!(
                        "Failed to install hooks. Run: hcom hooks add {}",
                        vanilla_tool.as_str()
                    );
                }
                Err(error) => {
                    eprintln!(
                        "Failed to install {} hooks: {error}\nRun: hcom hooks add {}",
                        vanilla_tool.as_str(),
                        vanilla_tool.as_str()
                    );
                }
            }
            return Ok(1);
        }

        // Gemini: ensure hooksConfig.enabled is set (self-heal for v0.26.0+)
        if vanilla_tool == crate::tool::Tool::Gemini {
            let _ = crate::hooks::gemini::ensure_hooks_enabled();
        }
    }

    let tool = ctx.tool.as_str();
    let session_id = match ctx.tool {
        crate::tool::Tool::Claude => resolve_claude_session_id(&ctx.raw_env),
        crate::tool::Tool::Codex => resolve_codex_session_id(ctx),
        _ => None,
    };

    if explicit_name.is_none()
        && let Some(ref session_id) = session_id
        && let Some(bound_name) = db.get_session_binding(session_id)?
    {
        // Only hcom writes session bindings, so a row keyed by this session's
        // own id is trusted identity evidence. Heal Claude bindings created by
        // older versions before returning the existing row.
        if ctx.tool == crate::tool::Tool::Claude {
            db.mark_claude_session_validated(session_id, &bound_name)?;
        }
        println!("hcom already started for {bound_name}");
        return Ok(0);
    }

    // Resolve or generate name
    let name = if let Some(n) = explicit_name {
        n.to_string()
    } else {
        instance_names::generate_unique_name(db)?
    };

    // Remote instances are relay mirrors. Starting them remotely is intentionally
    // unsupported because the useful remote lifecycle operations are launch/resume/kill.
    if let Ok(Some(ref existing)) = db.get_instance_full(&name)
        && crate::instances::is_remote_instance(existing)
    {
        bail!("Remote start is not supported for '{name}'. Start it on the owning device instead.");
    }

    // Check if already exists and active (only for explicit names —
    // generate_unique_name creates a placeholder row we must skip past)
    if explicit_name.is_some()
        && let Ok(Some(existing)) = db.get_instance_full(&name)
        && existing.status != "stopped"
    {
        println!("hcom already started for {}", name);
        return Ok(0);
    }

    instance_binding::initialize_instance_in_position_file(
        db,
        &name,
        session_id.as_deref(),
        None, // parent_session_id
        None, // parent_name
        None, // agent_id
        None, // transcript_path
        Some(tool),
        false, // background
        None,  // tag
        None,  // wait_timeout
        None,  // subagent_timeout
        None,  // hints
        None,  // cwd_override
    );

    if let Some(ref session_id) = session_id {
        db.set_session_binding(session_id, &name)?;
        if ctx.tool == crate::tool::Tool::Claude {
            db.mark_claude_session_validated(session_id, &name)?;
        }
    }

    // Bind process if we have a process_id
    if let Some(ref process_id) = ctx.process_id
        && let Err(e) = db.set_process_binding(process_id, "", &name)
    {
        eprintln!("[hcom] warn: set_process_binding failed for {name}: {e}");
    }

    // Claude builds old enough to expose neither session id leave nothing to
    // recognize this session by, so a later `hcom start` here mints another
    // identity. Say what was just created and name the way back instead of
    // letting the duplicate appear silently.
    if explicit_name.is_none() && ctx.tool == crate::tool::Tool::Claude && session_id.is_none() {
        let candidates = unbound_claude_candidates(db, ctx, &name);
        eprintln!(
            "[hcom] warn: this Claude session exposes no session id, so it was registered \
             as a new identity '{name}'. If it already had one{}, reclaim it with \
             `hcom start --as <name>` and drop this one with `hcom kill {name}`.",
            if candidates.is_empty() {
                String::new()
            } else {
                format!(" (unbound here: {})", candidates.join(", "))
            }
        );
    }

    // Print bootstrap
    let hcom_config = HcomConfig::load(None).unwrap_or_else(|e| {
        eprintln!("[hcom] warn: config load failed, using defaults: {e}");
        let mut c = HcomConfig::default();
        c.normalize();
        c
    });

    let bootstrap_text = bootstrap::get_bootstrap(
        db,
        hcom_dir,
        &name,
        tool,
        false,
        ctx.is_launched,
        &ctx.notes,
        &hcom_config.tag,
        relay::is_relay_enabled(&hcom_config),
        None,
    );

    println!("[hcom:{}]", name);
    println!("{}", bootstrap_text);
    // Repeated deliberately: the header above sits on top of a long bootstrap, so
    // `hcom start | tail -n` shows none of it. A caller that cannot see its own
    // name re-runs start, which is one way duplicate identities appear.
    println!("[hcom:{}]", name);

    // Log
    db.log_event(
        "life",
        &name,
        &json!({
            "action": "started",
            "tool": tool,
            "name": name,
        }),
    )
    .ok();

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use rusqlite::params;
    use serde_json::json;
    use serial_test::serial;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn make_ctx(tool_env: &[(&str, &str)], cwd: &str) -> HcomContext {
        let mut env: HashMap<String, String> = std::env::vars().collect();
        for (k, v) in tool_env {
            env.insert((*k).to_string(), (*v).to_string());
        }
        HcomContext::from_env(&env, PathBuf::from(cwd))
    }

    /// Claude context carrying exactly one session-id source, so an ambient
    /// value from the shell running the tests cannot decide the outcome.
    fn make_claude_ctx(session: Option<(&str, &str)>, cwd: &str) -> HcomContext {
        let mut env: HashMap<String, String> = std::env::vars().collect();
        env.remove("HCOM_CLAUDE_UNIX_SESSION_ID");
        env.remove("CLAUDE_CODE_SESSION_ID");
        env.insert("CLAUDECODE".to_string(), "1".to_string());
        if let Some((key, value)) = session {
            env.insert(key.to_string(), value.to_string());
        }
        HcomContext::from_env(&env, PathBuf::from(cwd))
    }

    fn make_codex_ctx(thread_id: Option<&str>, cwd: &str) -> HcomContext {
        let mut env: HashMap<String, String> = std::env::vars().collect();
        for key in [
            "HCOM_PROCESS_ID",
            "HCOM_LAUNCHED",
            "HCOM_PTY_MODE",
            "HCOM_BACKGROUND",
            "HCOM_IS_FORK",
            "HCOM_LAUNCHED_BY",
            "HCOM_LAUNCH_BATCH_ID",
            "HCOM_LAUNCH_EVENT_ID",
        ] {
            env.remove(key);
        }
        env.remove("CODEX_SANDBOX");
        match thread_id {
            Some(value) => {
                env.insert("CODEX_THREAD_ID".to_string(), value.to_string());
                env.insert("CODEX_SESSION_ID".to_string(), value.to_string());
            }
            None => {
                env.remove("CODEX_THREAD_ID");
                env.remove("CODEX_SESSION_ID");
            }
        }
        HcomContext::from_env(&env, PathBuf::from(cwd))
    }

    fn write_codex_session_meta(home: &std::path::Path, session_id: &str, cwd: &str) -> String {
        let codex_home = home.join(".codex");
        let sessions = codex_home
            .join("sessions")
            .join("2026")
            .join("09")
            .join("02");
        std::fs::create_dir_all(&sessions).unwrap();
        unsafe { std::env::set_var("CODEX_HOME", &codex_home) };
        let transcript = sessions.join(format!("rollout-test-{session_id}.jsonl"));
        let meta = serde_json::json!({
            "type": "session_meta",
            "payload": {
                "id": session_id,
                "session_id": session_id,
                "cwd": cwd,
                "originator": "Codex Desktop",
                "source": "vscode",
                "thread_source": "user"
            }
        });
        std::fs::write(&transcript, format!("{meta}\n")).unwrap();
        transcript.to_string_lossy().to_string()
    }

    fn log_stopped_snapshot(
        db: &HcomDb,
        name: &str,
        tool: &str,
        directory: &str,
        session_id: &str,
        last_event_id: i64,
    ) {
        db.log_event(
            "life",
            name,
            &json!({
                "action": "stopped",
                "snapshot": {
                    "tool": tool,
                    "directory": directory,
                    "session_id": session_id,
                    "last_event_id": last_event_id
                }
            }),
        )
        .unwrap();
    }

    fn write_claude_session_meta(home: &Path, session_id: &str, cwd: &str) -> String {
        let transcript = home
            .join(".claude")
            .join("projects")
            .join("test")
            .join(format!("{session_id}.jsonl"));
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(
            &transcript,
            format!(
                "{}\n{}\n",
                json!({"type": "last-prompt", "sessionId": session_id}),
                json!({
                    "type": "attachment",
                    "parentUuid": null,
                    "isSidechain": false,
                    "entrypoint": "claude-desktop",
                    "cwd": cwd,
                    "sessionId": session_id
                })
            ),
        )
        .unwrap();
        transcript.to_string_lossy().to_string()
    }

    fn insert_test_claude_actor_capability(
        db: &HcomDb,
        token: &str,
        session_id: &str,
        instance_name: &str,
    ) {
        db.conn()
            .execute(
                "INSERT INTO claude_actor_capabilities (
                     token, session_id, tool_use_id, agent_id, instance_name,
                     created_at, expires_at, last_seen
                 ) VALUES (?1, ?2, ?1, '', ?3, 1, 9223372036854775807, 1)",
                rusqlite::params![token, session_id, instance_name],
            )
            .unwrap();
    }

    #[cfg(unix)]
    fn create_test_file_symlink(source: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(source, link)
    }

    #[cfg(windows)]
    fn create_test_file_symlink(source: &Path, link: &Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_file(source, link)
    }

    #[cfg(unix)]
    fn create_test_dir_symlink(source: &Path, link: &Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(source, link)
    }

    #[cfg(windows)]
    fn create_test_dir_symlink(source: &Path, link: &Path) -> std::io::Result<()> {
        std::os::windows::fs::symlink_dir(source, link)
    }

    fn unused_loopback_ports() -> (u16, u16) {
        let first = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let second = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let ports = (
            first.local_addr().unwrap().port(),
            second.local_addr().unwrap().port(),
        );
        drop((first, second));
        ports
    }

    fn setup_platform_migration_fixture(
        db: &HcomDb,
        home: &Path,
        project: &Path,
        target_name: &str,
        current_session_id: &str,
        stopped_session_id: &str,
    ) -> (HcomContext, PathBuf) {
        std::fs::create_dir_all(project.join(".estate")).unwrap();
        let directory = project.to_string_lossy().to_string();
        let authority_dir = home.join("authority");
        std::fs::create_dir_all(&authority_dir).unwrap();
        let registry_path = authority_dir.join("DEV-REGISTRY.json");
        std::fs::write(
            &registry_path,
            serde_json::to_vec_pretty(&json!({
                "accounts": {
                    "gmc": {"platform": "codex"}
                },
                "devs": [{
                    "role": target_name,
                    "owner": "GMC",
                    "platform": "codex",
                    "account": "gmc",
                    "dir": directory,
                    "relay": {
                        "kind": "codex_task",
                        "thread_id": current_session_id,
                        "host_id": "local"
                    }
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            project.join(".estate").join("role.json"),
            serde_json::to_vec_pretty(&json!({
                "estate_schema": 1,
                "role": target_name,
                "owner": "GMC"
            }))
            .unwrap(),
        )
        .unwrap();

        write_codex_session_meta(home, current_session_id, &directory);
        let stopped_transcript = write_claude_session_meta(home, stopped_session_id, &directory);
        db.log_event(
            "life",
            target_name,
            &json!({
                "action": "stopped",
                "by": "system",
                "reason": "inactive_cleanup",
                "snapshot": {
                    "name": target_name,
                    "tool": "claude",
                    "directory": directory,
                    "session_id": stopped_session_id,
                    "transcript_path": stopped_transcript,
                    "parent_name": null,
                    "parent_session_id": null,
                    "agent_id": null,
                    "origin_device_id": null,
                    "background": 1,
                    "last_event_id": 0
                }
            }),
        )
        .unwrap();
        let (pty_port, inject_port) = unused_loopback_ports();
        db.conn()
            .execute(
                "INSERT INTO notify_endpoints (instance, kind, port, updated_at)
                 VALUES (?1, 'pty', ?2, 1), (?1, 'inject', ?3, 1)",
                rusqlite::params![target_name, pty_port, inject_port],
            )
            .unwrap();

        (
            make_codex_ctx(Some(current_session_id), &directory),
            registry_path,
        )
    }

    fn validate_test_codex_platform_migration(
        db: &HcomDb,
        target_name: &str,
        ctx: &HcomContext,
        resolved_session_id: Option<&str>,
        current_name: &str,
        registry_path: &Path,
    ) -> Result<CodexPlatformMigrationProof> {
        let codex_sessions_root =
            PathBuf::from(std::env::var("CODEX_HOME").expect("test fixture must set CODEX_HOME"))
                .join("sessions");
        let claude_projects_root = paths::get_project_root().join(".claude").join("projects");
        validate_codex_platform_migration_with_registry_anchor(
            db,
            target_name,
            ctx,
            resolved_session_id,
            current_name,
            registry_path,
            CodexPlatformMigrationTrust {
                registry_path,
                codex_sessions_root: &codex_sessions_root,
                claude_projects_root: &claude_projects_root,
            },
        )
    }
    #[test]
    fn test_start_args_bare() {
        let args = StartArgs::try_parse_from(["start"]).unwrap();
        assert!(args.orphan.is_none());
        assert!(args.as_name.is_none());
        assert!(!args.relocate);
        assert!(!args.migrate_platform);
        assert!(args.registry.is_none());
    }

    #[test]
    fn test_start_args_orphan() {
        let args = StartArgs::try_parse_from(["start", "--orphan", "1234"]).unwrap();
        assert_eq!(args.orphan, Some("1234".to_string()));
        assert!(args.as_name.is_none());
    }

    #[test]
    fn test_start_args_rebind() {
        let args = StartArgs::try_parse_from(["start", "--as", "luna"]).unwrap();
        assert!(args.orphan.is_none());
        assert_eq!(args.as_name, Some("luna".to_string()));
        assert!(!args.relocate);
        assert!(!args.migrate_platform);
    }

    #[test]
    fn test_start_args_explicit_relocation() {
        let args =
            StartArgs::try_parse_from(["start", "--as", "cultivation", "--relocate"]).unwrap();
        assert_eq!(args.as_name.as_deref(), Some("cultivation"));
        assert!(args.relocate);
    }

    #[test]
    fn test_start_args_explicit_platform_migration() {
        let args = StartArgs::try_parse_from([
            "start",
            "--as",
            "nsfw-studio",
            "--migrate-platform",
            "--registry",
            "/authority/DEV-REGISTRY.json",
        ])
        .unwrap();
        assert_eq!(args.as_name.as_deref(), Some("nsfw-studio"));
        assert!(args.migrate_platform);
        assert_eq!(
            args.registry.as_deref(),
            Some(Path::new("/authority/DEV-REGISTRY.json"))
        );
        assert!(!args.relocate);
    }

    #[test]
    fn test_start_args_bare_as_errors() {
        let err = StartArgs::try_parse_from(["start", "--as"]);
        assert!(err.is_err());
    }

    #[test]
    fn test_start_args_bare_orphan_errors() {
        let err = StartArgs::try_parse_from(["start", "--orphan"]);
        assert!(err.is_err());
    }

    #[test]
    fn test_start_args_unknown_flag_errors() {
        let err = StartArgs::try_parse_from(["start", "--bogus"]);
        assert!(err.is_err());
    }

    #[test]
    #[serial]
    fn test_start_relocation_requires_as_and_rejects_orphan() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let flags = crate::router::GlobalFlags::default();
        let missing_as = run(&["start".into(), "--relocate".into()], &flags).unwrap_err();
        assert!(missing_as.to_string().contains("requires --as"));

        let conflict = run(
            &[
                "start".into(),
                "--as".into(),
                "cultivation".into(),
                "--relocate".into(),
                "--orphan".into(),
                "old".into(),
            ],
            &flags,
        )
        .unwrap_err();
        assert!(conflict.to_string().contains("cannot be combined"));
    }

    #[test]
    #[serial]
    fn test_start_platform_migration_requires_as_registry_and_exclusive_mode() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let flags = crate::router::GlobalFlags::default();
        let missing_as = run(&["start".into(), "--migrate-platform".into()], &flags).unwrap_err();
        assert!(missing_as.to_string().contains("requires --as"));

        let missing_registry = run(
            &[
                "start".into(),
                "--as".into(),
                "nsfw-studio".into(),
                "--migrate-platform".into(),
            ],
            &flags,
        )
        .unwrap_err();
        assert!(missing_registry.to_string().contains("requires --registry"));

        let conflict = run(
            &[
                "start".into(),
                "--as".into(),
                "nsfw-studio".into(),
                "--migrate-platform".into(),
                "--registry".into(),
                "/authority/DEV-REGISTRY.json".into(),
                "--relocate".into(),
            ],
            &flags,
        )
        .unwrap_err();
        assert!(conflict.to_string().contains("cannot be combined"));
    }

    #[test]
    #[serial]
    fn test_start_rejects_remote_instances() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances (name, origin_device_id, created_at) VALUES (?1, ?2, ?3)",
                params![
                    "luna:ABCD",
                    "remote-device",
                    crate::shared::time::now_epoch_f64()
                ],
            )
            .unwrap();

        let flags = crate::router::GlobalFlags {
            name: Some("luna:ABCD".to_string()),
            go: false,
        };
        let err = run(&["start".to_string()], &flags).unwrap_err();
        assert!(
            err.to_string().contains("Remote start is not supported"),
            "unexpected error: {err}"
        );
    }

    #[test]
    #[serial]
    fn test_vanilla_claude_start_immediately_binds_exported_session() {
        struct RestoreEnv(Option<std::ffi::OsString>);
        impl Drop for RestoreEnv {
            fn drop(&mut self) {
                unsafe {
                    match self.0.take() {
                        Some(value) => std::env::set_var("HCOM_CLAUDE_UNIX_SESSION_ID", value),
                        None => std::env::remove_var("HCOM_CLAUDE_UNIX_SESSION_ID"),
                    }
                }
            }
        }

        let (_dir, hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        assert!(crate::hooks::claude::setup_claude_hooks(false));

        let _restore = RestoreEnv(std::env::var_os("HCOM_CLAUDE_UNIX_SESSION_ID"));
        unsafe {
            std::env::set_var("HCOM_CLAUDE_UNIX_SESSION_ID", "sess-vanilla");
        }
        let ctx = make_ctx(&[("CLAUDECODE", "1")], "/tmp/project");

        assert_eq!(start_bare(&db, &hcom_dir, &ctx, None).unwrap(), 0);
        let name = db
            .get_session_binding("sess-vanilla")
            .unwrap()
            .expect("bare vanilla start must bind immediately");
        let row = db.get_instance_full(&name).unwrap().unwrap();
        assert_eq!(row.session_id.as_deref(), Some("sess-vanilla"));
        assert_eq!(row.tool, "claude");
        assert_eq!(
            db.get_validated_claude_session_owner("sess-vanilla")
                .unwrap()
                .as_deref(),
            Some(name.as_str()),
            "CLI-created Claude bindings must be immediately trusted by hooks"
        );

        let transcript = hcom_dir.join("vanilla.jsonl");
        std::fs::write(&transcript, "{\"sessionId\":\"sess-vanilla\"}\n").unwrap();
        let mut hook_ctx = ctx.clone();
        hook_ctx.process_id = None;
        let (resolved, _, _) = crate::hooks::common::init_hook_context(
            &db,
            &hook_ctx,
            "sess-vanilla",
            transcript.to_str().unwrap(),
        );
        assert_eq!(resolved.as_deref(), Some(name.as_str()));

        assert_eq!(start_bare(&db, &hcom_dir, &ctx, None).unwrap(), 0);
        assert_eq!(
            db.get_session_binding("sess-vanilla").unwrap().as_deref(),
            Some(name.as_str()),
            "repeated bare start must retain the existing vanilla identity"
        );
    }

    #[test]
    fn test_resolve_claude_session_id_sources() {
        let env = |pairs: &[(&str, &str)]| -> HashMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect()
        };

        assert_eq!(
            resolve_claude_session_id(&env(&[
                ("HCOM_CLAUDE_UNIX_SESSION_ID", "hook-sess"),
                ("CLAUDE_CODE_SESSION_ID", "claude-sess"),
            ])),
            Some("hook-sess".to_string()),
            "our own export stays the first source"
        );
        assert_eq!(
            resolve_claude_session_id(&env(&[("CLAUDE_CODE_SESSION_ID", "claude-sess")])),
            Some("claude-sess".to_string()),
            "Claude's own Bash env carries identity when the env file cannot"
        );
        assert_eq!(
            resolve_claude_session_id(&env(&[
                ("HCOM_CLAUDE_UNIX_SESSION_ID", ""),
                ("CLAUDE_CODE_SESSION_ID", "claude-sess"),
            ])),
            Some("claude-sess".to_string()),
            "an empty export is not identity"
        );
        assert_eq!(resolve_claude_session_id(&env(&[])), None);
    }

    #[test]
    fn test_resolve_codex_session_id_trims_and_rejects_empty_values() {
        assert_eq!(
            resolve_codex_session_id(&make_codex_ctx(Some("  thread-1  "), "/tmp/project")),
            Some("thread-1".to_string())
        );
        assert_eq!(
            resolve_codex_session_id(&make_codex_ctx(Some("   "), "/tmp/project")),
            None
        );
        assert_eq!(
            resolve_codex_session_id(&make_codex_ctx(None, "/tmp/project")),
            None
        );
    }

    #[test]
    #[serial]
    fn test_bare_codex_start_binds_and_reuses_thread_id() {
        let (_dir, hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        assert!(crate::hooks::codex::setup_codex_hooks(false));
        let ctx = make_codex_ctx(Some("thread-bare-codex"), "/tmp/project");

        assert_eq!(start_bare(&db, &hcom_dir, &ctx, None).unwrap(), 0);
        let name = db
            .get_session_binding("thread-bare-codex")
            .unwrap()
            .expect("the first bare start must bind the Desktop task");
        let row = db.get_instance_full(&name).unwrap().unwrap();
        assert_eq!(row.tool, "codex");
        assert_eq!(row.session_id.as_deref(), Some("thread-bare-codex"));

        assert_eq!(start_bare(&db, &hcom_dir, &ctx, None).unwrap(), 0);
        assert_eq!(
            db.get_session_binding("thread-bare-codex")
                .unwrap()
                .as_deref(),
            Some(name.as_str())
        );
        let codex_rows: Vec<String> = db
            .iter_instances_full()
            .unwrap()
            .into_iter()
            .filter(|row| row.tool == "codex")
            .map(|row| row.name)
            .collect();
        assert_eq!(
            codex_rows,
            vec![name],
            "repeat start must not mint a duplicate"
        );
    }

    #[test]
    #[serial]
    fn test_vanilla_claude_start_reuses_claude_code_session_id() {
        let (_dir, hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        assert!(crate::hooks::claude::setup_claude_hooks(false));

        // No CLAUDE_ENV_FILE round trip, so HCOM_CLAUDE_UNIX_SESSION_ID never
        // arrives — the case that used to mint a second identity per start.
        let ctx = make_claude_ctx(
            Some(("CLAUDE_CODE_SESSION_ID", "sess-claude-env")),
            "/tmp/project",
        );

        assert_eq!(start_bare(&db, &hcom_dir, &ctx, None).unwrap(), 0);
        let name = db
            .get_session_binding("sess-claude-env")
            .unwrap()
            .expect("CLAUDE_CODE_SESSION_ID must bind identity");
        assert_eq!(
            db.get_validated_claude_session_owner("sess-claude-env")
                .unwrap()
                .as_deref(),
            Some(name.as_str()),
            "hooks must trust the binding the CLI just created"
        );

        assert_eq!(start_bare(&db, &hcom_dir, &ctx, None).unwrap(), 0);
        assert_eq!(
            db.get_session_binding("sess-claude-env")
                .unwrap()
                .as_deref(),
            Some(name.as_str()),
            "repeat start must return the first identity, not mint a second"
        );
        let claude_rows: Vec<String> = db
            .iter_instances_full()
            .unwrap()
            .into_iter()
            .filter(|row| row.tool == "claude")
            .map(|row| row.name)
            .collect();
        assert_eq!(claude_rows, vec![name], "exactly one identity per session");
    }

    #[test]
    #[serial]
    fn test_vanilla_claude_rebind_binds_session_and_drops_old_identity() {
        let (_dir, hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        assert!(crate::hooks::claude::setup_claude_hooks(false));

        let ctx = make_claude_ctx(
            Some(("CLAUDE_CODE_SESSION_ID", "sess-rebind")),
            "/tmp/project",
        );
        assert_eq!(start_bare(&db, &hcom_dir, &ctx, None).unwrap(), 0);
        let first = db.get_session_binding("sess-rebind").unwrap().unwrap();

        assert_eq!(start_rebind(&db, "nova", &ctx, None).unwrap(), 0);
        assert_eq!(
            db.get_session_binding("sess-rebind").unwrap().as_deref(),
            Some("nova"),
            "a reclaimed name must own the session that reclaimed it"
        );
        assert!(
            db.get_instance_full(&first).unwrap().is_none(),
            "the identity being replaced must not be left behind"
        );
        assert_eq!(
            db.get_validated_claude_session_owner("sess-rebind")
                .unwrap()
                .as_deref(),
            Some("nova"),
            "hooks must resolve the reclaimed name, not reject the session"
        );

        assert_eq!(start_bare(&db, &hcom_dir, &ctx, None).unwrap(), 0);
        assert_eq!(
            db.get_session_binding("sess-rebind").unwrap().as_deref(),
            Some("nova"),
            "a start after the rebind returns the reclaimed identity"
        );
    }

    #[test]
    #[serial]
    fn test_codex_rebind_uses_thread_id_without_process_binding() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances (name, tool, directory, status, created_at) VALUES (?1, 'codex', ?2, 'active', ?3)",
                params!["desktop-old", "/tmp/project", crate::shared::time::now_epoch_f64()],
            )
            .unwrap();
        let ctx = make_codex_ctx(Some("thread-desktop"), "/tmp/project");

        assert_eq!(start_rebind(&db, "desktop-old", &ctx, None).unwrap(), 0);
        let row = db.get_instance_full("desktop-old").unwrap().unwrap();
        assert_eq!(row.tool, "codex");
        assert_eq!(row.session_id.as_deref(), Some("thread-desktop"));
        assert_eq!(
            db.get_session_binding("thread-desktop").unwrap().as_deref(),
            Some("desktop-old")
        );

        db.conn()
            .execute(
                "UPDATE instances SET created_at = ?1 WHERE name = 'desktop-old'",
                params![
                    crate::shared::time::now_epoch_f64()
                        - (crate::instance_lifecycle::LAUNCH_PLACEHOLDER_TIMEOUT + 1) as f64
                ],
            )
            .unwrap();
        let aged = db.get_instance_full("desktop-old").unwrap().unwrap();
        let computed = crate::instance_lifecycle::get_instance_status(&aged, &db);
        assert_ne!(computed.context, "launch_failed");
        let stored = db.get_instance_full("desktop-old").unwrap().unwrap();
        assert_eq!(stored.session_id.as_deref(), Some("thread-desktop"));
        assert_eq!(stored.status_context, "new");
        let launch_failed_events: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM events
                 WHERE instance = 'desktop-old' AND type = 'status'
                   AND json_extract(data, '$.context') = 'launch_failed'",
                [],
                |event| event.get(0),
            )
            .unwrap();
        assert_eq!(launch_failed_events, 0);
    }

    #[test]
    #[serial]
    fn test_codex_rebind_ignores_empty_thread_id() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances (name, tool, directory, status, created_at) VALUES (?1, 'codex', ?2, 'active', ?3)",
                params!["desktop-empty", "/tmp/project", crate::shared::time::now_epoch_f64()],
            )
            .unwrap();
        let ctx = make_codex_ctx(Some("   "), "/tmp/project");

        assert_eq!(start_rebind(&db, "desktop-empty", &ctx, None).unwrap(), 0);
        let row = db.get_instance_full("desktop-empty").unwrap().unwrap();
        assert!(row.session_id.is_none());
        assert!(db.get_session_binding("").unwrap().is_none());
    }

    #[test]
    #[serial]
    fn test_codex_rebind_rejects_conflicting_process_session_before_mutation() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, session_id, tool, directory, status, created_at)
                 VALUES ('current-codex', 'thread-stale', 'codex', '/tmp/project', 'active', 1)",
                [],
            )
            .unwrap();
        db.set_process_binding("process-current", "thread-stale", "current-codex")
            .unwrap();
        log_stopped_snapshot(
            &db,
            "cultivation",
            "codex",
            "/tmp/project",
            "thread-current",
            41,
        );
        let mut ctx = make_codex_ctx(Some("thread-current"), "/tmp/project");
        ctx.process_id = Some("process-current".to_string());

        let err = start_rebind(&db, "cultivation", &ctx, None).unwrap_err();
        assert!(err.to_string().contains("conflicts with existing session"));
        assert!(db.get_instance_full("cultivation").unwrap().is_none());
        assert!(db.get_instance_full("current-codex").unwrap().is_some());
        assert_eq!(
            db.get_process_binding_full("process-current")
                .unwrap()
                .unwrap()
                .0
                .as_deref(),
            Some("thread-stale")
        );
    }

    #[test]
    #[serial]
    fn test_codex_rebind_rejects_conflicting_explicit_row_session_before_mutation() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, session_id, tool, directory, status, created_at)
                 VALUES ('current-codex', 'thread-stale', 'codex', '/tmp/project', 'active', 1)",
                [],
            )
            .unwrap();
        log_stopped_snapshot(
            &db,
            "cultivation",
            "codex",
            "/tmp/project",
            "thread-current",
            42,
        );
        let ctx = make_codex_ctx(Some("thread-current"), "/tmp/project");

        let err = start_rebind(&db, "cultivation", &ctx, Some("current-codex")).unwrap_err();
        assert!(err.to_string().contains("conflicts with existing session"));
        assert!(db.get_instance_full("cultivation").unwrap().is_none());
        let current = db.get_instance_full("current-codex").unwrap().unwrap();
        assert_eq!(current.session_id.as_deref(), Some("thread-stale"));
    }

    #[test]
    #[serial]
    fn test_codex_cli_rebind_still_rejects_cross_directory_for_same_env_session() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        log_stopped_snapshot(
            &db,
            "cultivation",
            "codex",
            "/tmp/old-project",
            "thread-cultivation",
            43,
        );
        let ctx = make_codex_ctx(Some("thread-cultivation"), "/tmp/cultivation");

        let err = start_rebind(&db, "cultivation", &ctx, None).unwrap_err();
        assert!(
            err.to_string()
                .contains("Refusing to reclaim 'cultivation'")
        );
        assert!(db.get_instance_full("cultivation").unwrap().is_none());
        assert!(
            db.get_session_binding("thread-cultivation")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    #[serial]
    fn test_codex_explicit_relocation_uses_verified_transcript_and_persists_pin() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let session_id = "thread-cultivation-relocate";
        let old_directory = tmp.path().join("old-scaffold");
        let new_directory = tmp.path().join("cultivation");
        std::fs::create_dir_all(&old_directory).unwrap();
        std::fs::create_dir_all(&new_directory).unwrap();
        let old_directory = old_directory.to_string_lossy().to_string();
        let new_directory = new_directory.to_string_lossy().to_string();
        let transcript = write_codex_session_meta(&home, session_id, &old_directory);
        db.log_event(
            "life",
            "cultivation",
            &serde_json::json!({
                "action": "stopped",
                "snapshot": {
                    "tool": "codex",
                    "directory": old_directory,
                    "session_id": session_id,
                    "transcript_path": transcript,
                    "parent_name": null,
                    "parent_session_id": null,
                    "agent_id": null,
                    "origin_device_id": null,
                    "last_event_id": 73
                }
            }),
        )
        .unwrap();
        let ctx = make_codex_ctx(Some(session_id), &new_directory);

        assert_eq!(
            start_rebind_with_options(&db, "cultivation", &ctx, None, true, false, None).unwrap(),
            0
        );
        let row = db.get_instance_full("cultivation").unwrap().unwrap();
        assert_eq!(row.session_id.as_deref(), Some(session_id));
        assert_eq!(row.last_event_id, 73);
        assert!(same_path(&row.directory, &new_directory));
        assert!(same_path(&row.transcript_path, &transcript));
        assert_eq!(
            crate::hooks::codex::pinned_directory_from_launch_context(
                row.launch_context.as_deref(),
                session_id,
            )
            .as_deref(),
            Some(new_directory.as_str())
        );
        assert_eq!(
            db.get_session_binding(session_id).unwrap().as_deref(),
            Some("cultivation")
        );
    }

    #[test]
    #[serial]
    fn test_codex_relocation_binding_failure_rolls_back_instance() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let session_id = "thread-binding-rollback";
        let old_directory = tmp.path().join("old-scaffold");
        let new_directory = tmp.path().join("cultivation");
        std::fs::create_dir_all(&old_directory).unwrap();
        std::fs::create_dir_all(&new_directory).unwrap();
        let old_directory = old_directory.to_string_lossy().to_string();
        let new_directory = new_directory.to_string_lossy().to_string();
        let transcript = write_codex_session_meta(&home, session_id, &old_directory);
        db.log_event(
            "life",
            "cultivation",
            &json!({
                "action": "stopped",
                "snapshot": {
                    "tool": "codex",
                    "directory": old_directory,
                    "session_id": session_id,
                    "transcript_path": transcript,
                    "last_event_id": 31
                }
            }),
        )
        .unwrap();
        let ctx = make_codex_ctx(Some(session_id), &new_directory);
        let proof =
            validate_codex_relocation(&db, "cultivation", &ctx, Some(session_id), "").unwrap();
        db.conn()
            .execute_batch(
                "CREATE TRIGGER deny_relocation_binding
                 BEFORE INSERT ON session_bindings
                 BEGIN SELECT RAISE(ABORT, 'forced binding failure'); END;",
            )
            .unwrap();

        let error = commit_codex_relocation(&db, "cultivation", &ctx, &proof).unwrap_err();
        assert!(error.to_string().contains("forced binding failure"));
        assert!(db.get_instance_full("cultivation").unwrap().is_none());
        assert!(db.get_session_binding(session_id).unwrap().is_none());
    }

    #[test]
    #[serial]
    fn test_codex_relocation_preserves_target_winner_after_proof() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let session_id = "thread-target-race";
        let old_directory = tmp.path().join("old-scaffold");
        let new_directory = tmp.path().join("cultivation");
        let winner_directory = tmp.path().join("winner");
        std::fs::create_dir_all(&old_directory).unwrap();
        std::fs::create_dir_all(&new_directory).unwrap();
        std::fs::create_dir_all(&winner_directory).unwrap();
        let old_directory = old_directory.to_string_lossy().to_string();
        let new_directory = new_directory.to_string_lossy().to_string();
        let winner_directory = winner_directory.to_string_lossy().to_string();
        let transcript = write_codex_session_meta(&home, session_id, &old_directory);
        db.log_event(
            "life",
            "cultivation",
            &json!({
                "action": "stopped",
                "snapshot": {
                    "tool": "codex",
                    "directory": old_directory,
                    "session_id": session_id,
                    "transcript_path": transcript,
                    "last_event_id": 32
                }
            }),
        )
        .unwrap();
        let ctx = make_codex_ctx(Some(session_id), &new_directory);
        let proof =
            validate_codex_relocation(&db, "cultivation", &ctx, Some(session_id), "").unwrap();

        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, session_id, tool, directory, status, created_at)
                 VALUES ('cultivation', 'thread-winner', 'codex', ?1, 'active', 1)",
                params![winner_directory],
            )
            .unwrap();
        db.set_session_binding("thread-winner", "cultivation")
            .unwrap();

        let error = commit_codex_relocation(&db, "cultivation", &ctx, &proof).unwrap_err();
        assert!(error.to_string().contains("another task registered"));
        let winner = db.get_instance_full("cultivation").unwrap().unwrap();
        assert_eq!(winner.session_id.as_deref(), Some("thread-winner"));
        assert_eq!(winner.directory, winner_directory);
        assert_eq!(
            db.get_session_binding("thread-winner").unwrap().as_deref(),
            Some("cultivation")
        );
        assert!(db.get_session_binding(session_id).unwrap().is_none());
    }

    #[test]
    #[serial]
    fn test_codex_relocation_preserves_session_winner_after_proof() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let session_id = "thread-session-race";
        let old_directory = tmp.path().join("old-scaffold");
        let new_directory = tmp.path().join("cultivation");
        std::fs::create_dir_all(&old_directory).unwrap();
        std::fs::create_dir_all(&new_directory).unwrap();
        let old_directory = old_directory.to_string_lossy().to_string();
        let new_directory = new_directory.to_string_lossy().to_string();
        let transcript = write_codex_session_meta(&home, session_id, &old_directory);
        db.log_event(
            "life",
            "cultivation",
            &json!({
                "action": "stopped",
                "snapshot": {
                    "tool": "codex",
                    "directory": old_directory,
                    "session_id": session_id,
                    "transcript_path": transcript,
                    "last_event_id": 33
                }
            }),
        )
        .unwrap();
        let ctx = make_codex_ctx(Some(session_id), &new_directory);
        let proof =
            validate_codex_relocation(&db, "cultivation", &ctx, Some(session_id), "").unwrap();

        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, session_id, tool, directory, status, created_at)
                 VALUES ('session-winner', ?1, 'codex', ?2, 'active', 1)",
                params![session_id, new_directory],
            )
            .unwrap();
        db.set_session_binding(session_id, "session-winner")
            .unwrap();

        let error = commit_codex_relocation(&db, "cultivation", &ctx, &proof).unwrap_err();
        assert!(error.to_string().contains("session is already bound"));
        assert!(db.get_instance_full("cultivation").unwrap().is_none());
        assert_eq!(
            db.get_session_binding(session_id).unwrap().as_deref(),
            Some("session-winner")
        );
    }

    #[test]
    #[serial]
    fn test_codex_relocation_rejects_malformed_snapshot_identity_fields() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let cases = [
            ("origin_device_id", json!(7)),
            ("parent_name", json!({"unexpected": true})),
            ("agent_id", json!(["unexpected"])),
            ("background", json!(true)),
            ("background", json!(1)),
        ];

        for (index, (field, bad_value)) in cases.into_iter().enumerate() {
            let bad_value_display = bad_value.to_string();
            let name = format!("cultivation-{index}");
            let session_id = format!("thread-malformed-{index}");
            let old_directory = tmp.path().join(format!("old-{index}"));
            let new_directory = tmp.path().join(format!("new-{index}"));
            std::fs::create_dir_all(&old_directory).unwrap();
            std::fs::create_dir_all(&new_directory).unwrap();
            let old_directory = old_directory.to_string_lossy().to_string();
            let new_directory = new_directory.to_string_lossy().to_string();
            let transcript = write_codex_session_meta(&home, &session_id, &old_directory);
            let mut snapshot = json!({
                "tool": "codex",
                "directory": old_directory,
                "session_id": session_id,
                "transcript_path": transcript,
                "last_event_id": 34
            });
            snapshot
                .as_object_mut()
                .unwrap()
                .insert(field.to_string(), bad_value);
            db.log_event(
                "life",
                &name,
                &json!({"action": "stopped", "snapshot": snapshot}),
            )
            .unwrap();
            let ctx = make_codex_ctx(Some(&session_id), &new_directory);

            let error = validate_codex_relocation(&db, &name, &ctx, Some(session_id.as_str()), "")
                .unwrap_err();
            assert!(
                error.to_string().contains("stopped identity"),
                "{field}={bad_value_display} unexpectedly produced: {error}"
            );
            assert!(db.get_instance_full(&name).unwrap().is_none());
            assert!(db.get_session_binding(&session_id).unwrap().is_none());
        }
    }

    #[test]
    #[serial]
    fn test_codex_relocation_rejects_mismatched_session_env_without_mutation() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let session_id = "thread-owned";
        let old_directory = tmp.path().join("old-scaffold");
        let new_directory = tmp.path().join("cultivation");
        std::fs::create_dir_all(&old_directory).unwrap();
        std::fs::create_dir_all(&new_directory).unwrap();
        let old_directory = old_directory.to_string_lossy().to_string();
        let new_directory = new_directory.to_string_lossy().to_string();
        let transcript = write_codex_session_meta(&home, session_id, &old_directory);
        db.log_event(
            "life",
            "cultivation",
            &serde_json::json!({
                "action": "stopped",
                "snapshot": {
                    "tool": "codex",
                    "directory": old_directory,
                    "session_id": session_id,
                    "transcript_path": transcript,
                    "origin_device_id": null,
                    "last_event_id": 73
                }
            }),
        )
        .unwrap();
        let mut ctx = make_codex_ctx(Some(session_id), &new_directory);
        ctx.raw_env
            .insert("CODEX_SESSION_ID".into(), "thread-foreign".into());

        let error = start_rebind_with_options(&db, "cultivation", &ctx, None, true, false, None)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("conflicts with CODEX_SESSION_ID")
        );
        assert!(db.get_instance_full("cultivation").unwrap().is_none());
        assert!(db.get_session_binding(session_id).unwrap().is_none());
    }

    #[test]
    #[serial]
    fn test_codex_relocation_rejects_remote_snapshot_without_mutation() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let session_id = "thread-remote";
        let old_directory = tmp.path().join("old-scaffold");
        let new_directory = tmp.path().join("cultivation");
        std::fs::create_dir_all(&old_directory).unwrap();
        std::fs::create_dir_all(&new_directory).unwrap();
        let old_directory = old_directory.to_string_lossy().to_string();
        let new_directory = new_directory.to_string_lossy().to_string();
        let transcript = write_codex_session_meta(&home, session_id, &old_directory);
        db.log_event(
            "life",
            "cultivation",
            &serde_json::json!({
                "action": "stopped",
                "snapshot": {
                    "tool": "codex",
                    "directory": old_directory,
                    "session_id": session_id,
                    "transcript_path": transcript,
                    "origin_device_id": "remote-device",
                    "last_event_id": 73
                }
            }),
        )
        .unwrap();
        let ctx = make_codex_ctx(Some(session_id), &new_directory);

        let error = start_rebind_with_options(&db, "cultivation", &ctx, None, true, false, None)
            .unwrap_err();
        assert!(error.to_string().contains("stopped identity is remote"));
        assert!(db.get_instance_full("cultivation").unwrap().is_none());
        assert!(db.get_session_binding(session_id).unwrap().is_none());
    }

    #[test]
    fn test_platform_authority_json_rejects_duplicate_keys_at_any_depth() {
        let registry_error = parse_authority_json(
            br#"{"accounts":{"gmc":{"platform":"codex","platform":"claude"}},"devs":[]}"#,
            "DEV-REGISTRY.json",
        )
        .unwrap_err();
        assert!(
            registry_error
                .to_string()
                .contains("duplicate JSON object key")
        );

        let role_error = parse_authority_json(
            br#"{"estate_schema":1,"role":"nsfw-studio","role":"other","owner":"GMC"}"#,
            ".estate/role.json",
        )
        .unwrap_err();
        assert!(role_error.to_string().contains("duplicate JSON object key"));
    }

    #[test]
    #[serial]
    fn test_codex_platform_migration_reclaims_stopped_claude_role_with_all_proofs() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let project = tmp.path().join("NSFWStudio");
        let current_session = "01a02cf5-e56f-7bf3-95f6-36a2accf2df2";
        let stopped_session = "760ed88a-1a03-4e8f-b743-3927f2d24f66";
        let (ctx, registry_path) = setup_platform_migration_fixture(
            &db,
            &home,
            &project,
            "nsfw-studio",
            current_session,
            stopped_session,
        );
        let (fixture_stop_id, fixture_stop_data): (i64, String) = db
            .conn()
            .query_row(
                "SELECT id, data FROM events
                 WHERE type='life' AND instance='nsfw-studio'
                 ORDER BY id DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let mut measured_stop: serde_json::Value =
            serde_json::from_str(&fixture_stop_data).unwrap();
        measured_stop["snapshot"]["last_event_id"] = json!(62_258);
        db.conn()
            .execute(
                "UPDATE events SET id=63309, data=?1 WHERE id=?2",
                rusqlite::params![
                    serde_json::to_string(&measured_stop).unwrap(),
                    fixture_stop_id
                ],
            )
            .unwrap();

        let proof = validate_test_codex_platform_migration(
            &db,
            "nsfw-studio",
            &ctx,
            Some(current_session),
            "",
            &registry_path,
        )
        .unwrap();
        assert_eq!(proof.from_tool, "claude");
        assert_eq!(proof.from_session_id, stopped_session);
        let stopped_lineage_key = format!("claude_lineage_validated:{stopped_session}");
        let current_lineage_key = format!("claude_lineage_validated:{current_session}");
        let unrelated_lineage_key = "claude_lineage_validated:unrelated-session";
        db.kv_set(&stopped_lineage_key, Some("nsfw-studio"))
            .unwrap();
        db.kv_set(&current_lineage_key, Some("poisoned-current-owner"))
            .unwrap();
        db.kv_set(unrelated_lineage_key, Some("unrelated-owner"))
            .unwrap();
        insert_test_claude_actor_capability(
            &db,
            "stopped-role-capability",
            "other-stale-session",
            "nsfw-studio",
        );
        insert_test_claude_actor_capability(
            &db,
            "stopped-session-capability",
            stopped_session,
            "stale-child",
        );
        insert_test_claude_actor_capability(
            &db,
            "current-session-capability",
            current_session,
            "stale-current-owner",
        );
        insert_test_claude_actor_capability(
            &db,
            "unrelated-capability",
            "unrelated-session",
            "unrelated-owner",
        );
        commit_codex_platform_migration(&db, "nsfw-studio", &ctx, &proof).unwrap();

        let row = db.get_instance_full("nsfw-studio").unwrap().unwrap();
        assert_eq!(row.tool, "codex");
        assert_eq!(row.session_id.as_deref(), Some(current_session));
        assert_eq!(row.last_event_id, 62_258);
        assert!(same_path(
            &row.directory,
            project.to_string_lossy().as_ref()
        ));
        assert_eq!(
            db.get_session_binding(current_session).unwrap().as_deref(),
            Some("nsfw-studio")
        );
        assert!(db.get_session_binding(stopped_session).unwrap().is_none());
        let audit: String = db
            .conn()
            .query_row(
                "SELECT data FROM events WHERE type='life' AND instance='nsfw-studio' ORDER BY id DESC LIMIT 1",
                [],
                |result| result.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&audit)
                .unwrap()
                .get("by")
                .and_then(serde_json::Value::as_str),
            Some("explicit-start-migrate-platform-v1")
        );

        assert_eq!(
            db.get_session_binding(current_session).unwrap().as_deref(),
            Some("nsfw-studio"),
            "the migration must leave a durable session-to-role binding"
        );
        let notify_count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM notify_endpoints WHERE instance='nsfw-studio'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            notify_count, 0,
            "stale Claude wake endpoints must be removed"
        );
        assert!(
            db.kv_get(&stopped_lineage_key).unwrap().is_none(),
            "the migrated Claude generation must lose its cached lineage authority"
        );
        assert!(
            db.kv_get(&current_lineage_key).unwrap().is_none(),
            "the Codex task id must not retain poisoned Claude lineage authority"
        );
        assert_eq!(
            db.kv_get(unrelated_lineage_key).unwrap().as_deref(),
            Some("unrelated-owner"),
            "migration must preserve unrelated Claude lineage authority"
        );
        let stale_capability_count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM claude_actor_capabilities
                 WHERE instance_name='nsfw-studio'
                    OR session_id IN (?1, ?2)",
                rusqlite::params![stopped_session, current_session],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            stale_capability_count, 0,
            "migration must revoke Claude actor capabilities tied to the role or either session"
        );
        let unrelated_capability_count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM claude_actor_capabilities
                 WHERE token='unrelated-capability'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            unrelated_capability_count, 1,
            "migration must preserve unrelated Claude actor capabilities"
        );
        let subscription_count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM kv
                 WHERE key LIKE 'events_sub:%'
                   AND json_extract(value, '$.caller')='nsfw-studio'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            subscription_count > 0,
            "a migrated released tool must regain configured automatic subscriptions"
        );
    }

    #[test]
    #[serial]
    fn test_codex_platform_migration_requires_exact_codex_relay_authority() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let project = tmp.path().join("NSFWStudio");
        let current_session = "current-relay-authority";
        let (ctx, registry_path) = setup_platform_migration_fixture(
            &db,
            &home,
            &project,
            "nsfw-studio",
            current_session,
            "stopped-relay-authority",
        );
        let baseline: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&registry_path).unwrap()).unwrap();

        for (label, relay, expected) in [
            ("missing", None, "no valid relay authority"),
            (
                "malformed",
                Some(json!("codex_task")),
                "no valid relay authority",
            ),
            (
                "wrong task",
                Some(json!({
                    "kind": "codex_task",
                    "thread_id": "another-task",
                    "host_id": "local"
                })),
                "does not authorize current CODEX_THREAD_ID",
            ),
            (
                "wrong kind",
                Some(json!({
                    "kind": "claude_session",
                    "thread_id": current_session,
                    "host_id": "local"
                })),
                "relay kind is not 'codex_task'",
            ),
            (
                "wrong host",
                Some(json!({
                    "kind": "codex_task",
                    "thread_id": current_session,
                    "host_id": "remote"
                })),
                "relay host is not 'local'",
            ),
        ] {
            let mut registry = baseline.clone();
            let dev = registry
                .get_mut("devs")
                .and_then(serde_json::Value::as_array_mut)
                .and_then(|devs| devs.first_mut())
                .and_then(serde_json::Value::as_object_mut)
                .unwrap();
            match relay {
                Some(value) => {
                    dev.insert("relay".to_string(), value);
                }
                None => {
                    dev.remove("relay");
                }
            }
            std::fs::write(
                &registry_path,
                serde_json::to_vec_pretty(&registry).unwrap(),
            )
            .unwrap();

            let error = validate_test_codex_platform_migration(
                &db,
                "nsfw-studio",
                &ctx,
                Some(current_session),
                "",
                &registry_path,
            )
            .unwrap_err();
            assert!(
                error.to_string().contains(expected),
                "{label} relay produced unexpected error: {error}"
            );
            assert!(db.get_instance_full("nsfw-studio").unwrap().is_none());
        }
    }

    #[test]
    #[serial]
    fn test_codex_platform_migration_is_narrowly_scoped_to_nsfw_studio() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let project = tmp.path().join("OtherStudio");
        let current_session = "current-other-studio";
        let (ctx, registry_path) = setup_platform_migration_fixture(
            &db,
            &home,
            &project,
            "other-studio",
            current_session,
            "stopped-other-studio",
        );

        let error = validate_test_codex_platform_migration(
            &db,
            "other-studio",
            &ctx,
            Some(current_session),
            "",
            &registry_path,
        )
        .unwrap_err();
        assert!(error.to_string().contains("narrowly authorized only"));
        assert!(db.get_instance_full("other-studio").unwrap().is_none());
    }

    #[test]
    #[serial]
    fn test_codex_platform_transcript_resolution_rejects_zero_or_multiple_matches() {
        let (tmp, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let sessions_root = tmp.path().join(".codex").join("sessions");
        std::fs::create_dir_all(sessions_root.join("a")).unwrap();
        std::fs::create_dir_all(sessions_root.join("b")).unwrap();
        let missing =
            resolve_codex_platform_transcript("missing-task", &sessions_root).unwrap_err();
        assert!(missing.to_string().contains("Could not locate"));

        for child in ["a", "b"] {
            std::fs::write(
                sessions_root
                    .join(child)
                    .join("rollout-test-duplicate-task.jsonl"),
                "{}\n",
            )
            .unwrap();
        }
        let ambiguous =
            resolve_codex_platform_transcript("duplicate-task", &sessions_root).unwrap_err();
        assert!(ambiguous.to_string().contains("Multiple Codex transcripts"));
    }

    #[test]
    #[serial]
    fn test_codex_platform_migration_rejects_duplicate_session_meta_keys() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let project = tmp.path().join("NSFWStudio");
        let current_session = "current-duplicate-meta";
        let (ctx, registry_path) = setup_platform_migration_fixture(
            &db,
            &home,
            &project,
            "nsfw-studio",
            current_session,
            "stopped-duplicate-meta",
        );
        let transcript = home
            .join(".codex")
            .join("sessions")
            .join("2026")
            .join("09")
            .join("02")
            .join(format!("rollout-test-{current_session}.jsonl"));
        std::fs::write(
            transcript,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{current_session}\",\"id\":\"other\",\"session_id\":\"{current_session}\",\"cwd\":{},\"originator\":\"Codex Desktop\",\"source\":\"vscode\",\"thread_source\":\"user\"}}}}\n",
                serde_json::to_string(project.to_string_lossy().as_ref()).unwrap()
            ),
        )
        .unwrap();

        let error = validate_test_codex_platform_migration(
            &db,
            "nsfw-studio",
            &ctx,
            Some(current_session),
            "",
            &registry_path,
        )
        .unwrap_err();
        assert!(error.to_string().contains("duplicate JSON object key"));
        assert!(db.get_instance_full("nsfw-studio").unwrap().is_none());
    }

    #[test]
    #[serial]
    fn test_codex_platform_migration_rejects_nested_duplicate_stopped_transcript_keys() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let project = tmp.path().join("NSFWStudio");
        let current_session = "current-duplicate-stopped-transcript";
        let stopped_session = "stopped-duplicate-stopped-transcript";
        let (ctx, registry_path) = setup_platform_migration_fixture(
            &db,
            &home,
            &project,
            "nsfw-studio",
            current_session,
            stopped_session,
        );
        let stopped_transcript = home
            .join(".claude")
            .join("projects")
            .join("test")
            .join(format!("{stopped_session}.jsonl"));
        std::fs::write(
            stopped_transcript,
            format!(
                "{{\"type\":\"attachment\",\"metadata\":{{\"owner\":\"one\",\"owner\":\"two\"}},\"parentUuid\":null,\"isSidechain\":false,\"entrypoint\":\"claude-desktop\",\"cwd\":{},\"sessionId\":\"{stopped_session}\"}}\n",
                serde_json::to_string(project.to_string_lossy().as_ref()).unwrap()
            ),
        )
        .unwrap();

        let error = validate_test_codex_platform_migration(
            &db,
            "nsfw-studio",
            &ctx,
            Some(current_session),
            "",
            &registry_path,
        )
        .unwrap_err();
        assert!(error.to_string().contains("duplicate JSON object key"));
        assert!(db.get_instance_full("nsfw-studio").unwrap().is_none());
    }

    #[test]
    #[serial]
    fn test_codex_platform_transcript_rejects_file_symlink_when_supported() {
        let (tmp, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let sessions_root = tmp.path().join(".codex").join("sessions");
        let session_dir = sessions_root.join("2026").join("09").join("02");
        std::fs::create_dir_all(&session_dir).unwrap();
        let target = session_dir.join("real.jsonl");
        let link = session_dir.join("rollout-test-symlink-task.jsonl");
        std::fs::write(&target, "{}\n").unwrap();
        if create_test_file_symlink(&target, &link).is_err() {
            return;
        }

        let error = resolve_codex_platform_transcript("symlink-task", &sessions_root).unwrap_err();
        assert!(error.to_string().contains("link or reparse point"));
    }

    #[test]
    #[serial]
    fn test_codex_platform_transcript_rejects_parent_link_when_supported() {
        let (tmp, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let sessions_root = tmp.path().join(".codex").join("sessions");
        let real_dir = tmp.path().join("real-session-dir");
        std::fs::create_dir_all(&sessions_root).unwrap();
        std::fs::create_dir_all(&real_dir).unwrap();
        std::fs::write(real_dir.join("rollout-test-parent-link-task.jsonl"), "{}\n").unwrap();
        let linked_dir = sessions_root.join("linked");
        if create_test_dir_symlink(&real_dir, &linked_dir).is_err() {
            return;
        }

        let error =
            resolve_codex_platform_transcript("parent-link-task", &sessions_root).unwrap_err();
        assert!(
            error.to_string().contains("link or reparse point")
                || error.to_string().contains("Could not locate"),
            "unexpected fail-closed error: {error}"
        );
    }

    #[test]
    #[serial]
    fn test_codex_platform_migration_rejects_stopped_transcript_symlink_when_supported() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let project = tmp.path().join("NSFWStudio");
        let current_session = "current-stopped-transcript-link";
        let stopped_session = "stopped-transcript-link";
        let (ctx, registry_path) = setup_platform_migration_fixture(
            &db,
            &home,
            &project,
            "nsfw-studio",
            current_session,
            stopped_session,
        );
        let stopped_transcript = home
            .join(".claude")
            .join("projects")
            .join("test")
            .join(format!("{stopped_session}.jsonl"));
        let real_transcript = stopped_transcript.with_extension("real.jsonl");
        std::fs::rename(&stopped_transcript, &real_transcript).unwrap();
        if create_test_file_symlink(&real_transcript, &stopped_transcript).is_err() {
            return;
        }

        let error = validate_test_codex_platform_migration(
            &db,
            "nsfw-studio",
            &ctx,
            Some(current_session),
            "",
            &registry_path,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("link or reparse point"),
            "unexpected fail-closed error: {error}"
        );
        assert!(db.get_instance_full("nsfw-studio").unwrap().is_none());
    }

    #[test]
    #[serial]
    fn test_codex_platform_migration_requires_exact_legacy_endpoint_shape() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let project = tmp.path().join("NSFWStudio");
        let current_session = "current-endpoint-shape";
        let stopped_session = "stopped-endpoint-shape";
        let (ctx, registry_path) = setup_platform_migration_fixture(
            &db,
            &home,
            &project,
            "nsfw-studio",
            current_session,
            stopped_session,
        );
        let endpoints = validate_legacy_platform_endpoints(
            db.conn(),
            "nsfw-studio",
            crate::shared::time::now_epoch_f64(),
        )
        .unwrap();

        for missing_kind in ["pty", "inject"] {
            let endpoint = endpoints
                .iter()
                .find(|endpoint| endpoint.kind == missing_kind)
                .unwrap();
            db.conn()
                .execute(
                    "DELETE FROM notify_endpoints
                     WHERE instance='nsfw-studio' AND kind=?1",
                    rusqlite::params![missing_kind],
                )
                .unwrap();
            let error = validate_test_codex_platform_migration(
                &db,
                "nsfw-studio",
                &ctx,
                Some(current_session),
                "",
                &registry_path,
            )
            .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("exactly one 'pty' and one 'inject'"),
                "missing {missing_kind} produced unexpected error: {error}"
            );
            db.conn()
                .execute(
                    "INSERT INTO notify_endpoints (instance, kind, port, updated_at)
                     VALUES ('nsfw-studio', ?1, ?2, ?3)",
                    rusqlite::params![missing_kind, endpoint.port, endpoint.updated_at()],
                )
                .unwrap();
        }

        let (extra_port, _) = unused_loopback_ports();
        db.conn()
            .execute(
                "INSERT INTO notify_endpoints (instance, kind, port, updated_at)
                 VALUES ('nsfw-studio', 'hook', ?1, 1)",
                rusqlite::params![extra_port],
            )
            .unwrap();
        let error = validate_test_codex_platform_migration(
            &db,
            "nsfw-studio",
            &ctx,
            Some(current_session),
            "",
            &registry_path,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("exactly one 'pty' and one 'inject'")
        );
        assert!(db.get_instance_full("nsfw-studio").unwrap().is_none());
    }

    #[test]
    fn test_codex_platform_migration_rejects_duplicate_legacy_endpoint_rows() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE notify_endpoints (
                     instance TEXT NOT NULL,
                     kind TEXT NOT NULL,
                     port INTEGER NOT NULL,
                     updated_at REAL NOT NULL
                 );
                 INSERT INTO notify_endpoints VALUES
                     ('nsfw-studio', 'pty', 41001, 1),
                     ('nsfw-studio', 'pty', 41002, 1),
                     ('nsfw-studio', 'inject', 41003, 1);",
            )
            .unwrap();
        let error = validate_legacy_platform_endpoints(
            &connection,
            "nsfw-studio",
            crate::shared::time::now_epoch_f64(),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("exactly one 'pty' and one 'inject'")
        );
    }

    #[test]
    fn test_codex_platform_migration_rejects_invalid_legacy_endpoint_port() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE notify_endpoints (
                     instance TEXT NOT NULL,
                     kind TEXT NOT NULL,
                     port INTEGER NOT NULL,
                     updated_at REAL NOT NULL
                 );
                 INSERT INTO notify_endpoints VALUES
                     ('nsfw-studio', 'inject', 0, 1),
                     ('nsfw-studio', 'pty', 41001, 1);",
            )
            .unwrap();
        let error = validate_legacy_platform_endpoints(
            &connection,
            "nsfw-studio",
            crate::shared::time::now_epoch_f64(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("invalid port"));
    }

    #[test]
    #[serial]
    fn test_codex_platform_migration_rejects_fresh_or_future_legacy_endpoint() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let project = tmp.path().join("NSFWStudio");
        let current_session = "current-endpoint-time";
        let (ctx, registry_path) = setup_platform_migration_fixture(
            &db,
            &home,
            &project,
            "nsfw-studio",
            current_session,
            "stopped-endpoint-time",
        );
        let now = crate::shared::time::now_epoch_f64();

        db.conn()
            .execute(
                "UPDATE notify_endpoints SET updated_at=?1
                 WHERE instance='nsfw-studio' AND kind='pty'",
                rusqlite::params![now],
            )
            .unwrap();
        let fresh_error = validate_test_codex_platform_migration(
            &db,
            "nsfw-studio",
            &ctx,
            Some(current_session),
            "",
            &registry_path,
        )
        .unwrap_err();
        assert!(fresh_error.to_string().contains("endpoint is only"));

        db.conn()
            .execute(
                "UPDATE notify_endpoints SET updated_at=?1
                 WHERE instance='nsfw-studio' AND kind='pty'",
                rusqlite::params![now + 60.0],
            )
            .unwrap();
        let future_error = validate_test_codex_platform_migration(
            &db,
            "nsfw-studio",
            &ctx,
            Some(current_session),
            "",
            &registry_path,
        )
        .unwrap_err();
        assert!(
            future_error
                .to_string()
                .contains("invalid or future timestamp")
        );
        assert!(db.get_instance_full("nsfw-studio").unwrap().is_none());
    }

    #[test]
    fn test_codex_platform_migration_endpoint_probe_requires_connection_refused() {
        use std::io::{Error, ErrorKind};

        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE notify_endpoints (
                     instance TEXT, kind TEXT, port INTEGER, updated_at REAL
                 );
                 INSERT INTO notify_endpoints VALUES
                     ('nsfw-studio', 'inject', 31234, 1.0),
                     ('nsfw-studio', 'pty', 31235, 1.0);",
            )
            .unwrap();

        let mut checked_ports = Vec::new();
        let proof = validate_legacy_platform_endpoints_with_probe(
            &connection,
            "nsfw-studio",
            1000.0,
            |address| {
                assert!(address.ip().is_loopback());
                checked_ports.push(address.port());
                Err(Error::from(ErrorKind::ConnectionRefused))
            },
        )
        .unwrap();
        assert_eq!(checked_ports, [31234, 31235]);
        assert_eq!(proof.len(), 2);

        for uncertain_port in [31234, 31235] {
            for error_kind in [
                ErrorKind::TimedOut,
                ErrorKind::WouldBlock,
                ErrorKind::PermissionDenied,
                ErrorKind::AddrNotAvailable,
                ErrorKind::ConnectionReset,
                ErrorKind::Interrupted,
                ErrorKind::Other,
            ] {
                let error = validate_legacy_platform_endpoints_with_probe(
                    &connection,
                    "nsfw-studio",
                    1000.0,
                    |address| {
                        Err(Error::from(if address.port() == uncertain_port {
                            error_kind
                        } else {
                            ErrorKind::ConnectionRefused
                        }))
                    },
                )
                .unwrap_err();
                assert!(
                    error.to_string().contains("could not be proven closed"),
                    "unexpected {error_kind:?} error on {uncertain_port}: {error}"
                );
            }
        }

        let remaining_endpoints: i64 = connection
            .query_row("SELECT COUNT(*) FROM notify_endpoints", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining_endpoints, 2);
    }

    #[test]
    #[serial]
    fn test_codex_platform_migration_rejects_reachable_legacy_endpoint() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let project = tmp.path().join("NSFWStudio");
        let current_session = "current-live-endpoint";
        let (ctx, registry_path) = setup_platform_migration_fixture(
            &db,
            &home,
            &project,
            "nsfw-studio",
            current_session,
            "stopped-live-endpoint",
        );
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        db.conn()
            .execute(
                "UPDATE notify_endpoints SET port=?1, updated_at=1
                 WHERE instance='nsfw-studio' AND kind='pty'",
                rusqlite::params![port],
            )
            .unwrap();

        let error = validate_test_codex_platform_migration(
            &db,
            "nsfw-studio",
            &ctx,
            Some(current_session),
            "",
            &registry_path,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("still reachable"),
            "unexpected live-endpoint error: {error}"
        );
        assert!(db.get_instance_full("nsfw-studio").unwrap().is_none());
        drop(listener);
    }

    #[test]
    #[serial]
    fn test_codex_platform_migration_rejects_endpoint_change_at_commit() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let project = tmp.path().join("NSFWStudio");
        let current_session = "current-endpoint-change";
        let (ctx, registry_path) = setup_platform_migration_fixture(
            &db,
            &home,
            &project,
            "nsfw-studio",
            current_session,
            "stopped-endpoint-change",
        );
        let proof = validate_test_codex_platform_migration(
            &db,
            "nsfw-studio",
            &ctx,
            Some(current_session),
            "",
            &registry_path,
        )
        .unwrap();

        let (extra_port, _) = unused_loopback_ports();
        db.conn()
            .execute(
                "INSERT INTO notify_endpoints (instance, kind, port, updated_at)
                 VALUES ('nsfw-studio', 'hook', ?1, 1)",
                rusqlite::params![extra_port],
            )
            .unwrap();
        let extra_error =
            commit_codex_platform_migration(&db, "nsfw-studio", &ctx, &proof).unwrap_err();
        assert!(
            extra_error
                .to_string()
                .contains("exactly one 'pty' and one 'inject'")
        );
        assert!(db.get_instance_full("nsfw-studio").unwrap().is_none());

        db.conn()
            .execute(
                "DELETE FROM notify_endpoints
                 WHERE instance='nsfw-studio' AND kind='hook'",
                [],
            )
            .unwrap();
        db.conn()
            .execute(
                "UPDATE notify_endpoints SET updated_at=2
                 WHERE instance='nsfw-studio' AND kind='pty'",
                [],
            )
            .unwrap();
        let changed_error =
            commit_codex_platform_migration(&db, "nsfw-studio", &ctx, &proof).unwrap_err();
        assert!(changed_error.to_string().contains("endpoint proof changed"));
        assert!(db.get_instance_full("nsfw-studio").unwrap().is_none());
        assert!(db.get_session_binding(current_session).unwrap().is_none());
    }

    #[test]
    #[serial]
    fn test_codex_platform_migration_rejects_untrusted_self_authored_registry() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let project = tmp.path().join("NSFWStudio");
        let current_session = "current-untrusted-registry";
        let (ctx, registry_path) = setup_platform_migration_fixture(
            &db,
            &home,
            &project,
            "nsfw-studio",
            current_session,
            "stopped-untrusted-registry",
        );

        let error = validate_codex_platform_migration(
            &db,
            "nsfw-studio",
            &ctx,
            Some(current_session),
            "",
            &registry_path,
        )
        .unwrap_err();
        assert!(error.to_string().contains("trusted platform registry"));
        assert!(db.get_instance_full("nsfw-studio").unwrap().is_none());
    }

    #[test]
    #[serial]
    fn test_codex_platform_migration_rejects_non_desktop_claude_transcript() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let project = tmp.path().join("NSFWStudio");
        let current_session = "current-cli-transcript";
        let stopped_session = "stopped-cli-transcript";
        let (ctx, registry_path) = setup_platform_migration_fixture(
            &db,
            &home,
            &project,
            "nsfw-studio",
            current_session,
            stopped_session,
        );
        let stopped_transcript = home
            .join(".claude")
            .join("projects")
            .join("test")
            .join(format!("{stopped_session}.jsonl"));
        std::fs::write(
            stopped_transcript,
            format!(
                "{}\n{}\n",
                json!({"type": "last-prompt", "sessionId": stopped_session}),
                json!({
                    "type": "attachment",
                    "parentUuid": null,
                    "isSidechain": false,
                    "entrypoint": "cli",
                    "cwd": project.to_string_lossy(),
                    "sessionId": stopped_session
                })
            ),
        )
        .unwrap();

        let error = validate_test_codex_platform_migration(
            &db,
            "nsfw-studio",
            &ctx,
            Some(current_session),
            "",
            &registry_path,
        )
        .unwrap_err();
        assert!(error.to_string().contains("not from Claude Desktop"));
        assert!(db.get_instance_full("nsfw-studio").unwrap().is_none());
    }

    #[test]
    #[serial]
    fn test_codex_platform_migration_rejects_child_claude_transcript() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let project = tmp.path().join("NSFWStudio");
        let current_session = "current-child-transcript";
        let stopped_session = "stopped-child-transcript";
        let (ctx, registry_path) = setup_platform_migration_fixture(
            &db,
            &home,
            &project,
            "nsfw-studio",
            current_session,
            stopped_session,
        );
        let stopped_transcript = home
            .join(".claude")
            .join("projects")
            .join("test")
            .join(format!("{stopped_session}.jsonl"));
        std::fs::write(
            stopped_transcript,
            format!(
                "{}\n",
                json!({
                    "type": "attachment",
                    "parentUuid": "parent-message",
                    "isSidechain": true,
                    "agentId": "agent-1",
                    "entrypoint": "claude-desktop",
                    "cwd": project.to_string_lossy(),
                    "sessionId": stopped_session
                })
            ),
        )
        .unwrap();

        let error = validate_test_codex_platform_migration(
            &db,
            "nsfw-studio",
            &ctx,
            Some(current_session),
            "",
            &registry_path,
        )
        .unwrap_err();
        assert!(error.to_string().contains("child or agent task"));
        assert!(db.get_instance_full("nsfw-studio").unwrap().is_none());
    }

    #[test]
    #[serial]
    fn test_codex_platform_migration_audit_failure_rolls_back_all_state() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let project = tmp.path().join("NSFWStudio");
        let current_session = "current-audit-rollback";
        let (ctx, registry_path) = setup_platform_migration_fixture(
            &db,
            &home,
            &project,
            "nsfw-studio",
            current_session,
            "stopped-audit-rollback",
        );
        let proof = validate_test_codex_platform_migration(
            &db,
            "nsfw-studio",
            &ctx,
            Some(current_session),
            "",
            &registry_path,
        )
        .unwrap();
        db.conn()
            .execute_batch(
                "CREATE TRIGGER fail_platform_migration_audit
                 BEFORE INSERT ON events
                 WHEN NEW.data LIKE '%explicit-start-migrate-platform-v1%'
                 BEGIN
                   SELECT RAISE(ABORT, 'forced platform migration audit failure');
                 END;",
            )
            .unwrap();

        let error = commit_codex_platform_migration(&db, "nsfw-studio", &ctx, &proof).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("forced platform migration audit failure")
        );
        assert!(db.get_instance_full("nsfw-studio").unwrap().is_none());
        assert!(db.get_session_binding(current_session).unwrap().is_none());
        let stale_endpoint_count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM notify_endpoints WHERE instance='nsfw-studio'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            stale_endpoint_count, 2,
            "endpoint cleanup must roll back with the failed migration"
        );
        let restored_endpoints = validate_legacy_platform_endpoints(
            db.conn(),
            "nsfw-studio",
            crate::shared::time::now_epoch_f64(),
        )
        .unwrap();
        assert_eq!(restored_endpoints, proof.legacy_endpoints);
        let migration_event_count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM events
                 WHERE data LIKE '%explicit-start-migrate-platform-v1%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(migration_event_count, 0);
    }

    #[test]
    #[serial]
    fn test_codex_platform_migration_subscription_failure_rolls_back_and_recovers() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let project = tmp.path().join("NSFWStudio");
        let current_session = "current-subscription-rollback";
        let stopped_session = "stopped-subscription-rollback";
        let (ctx, registry_path) = setup_platform_migration_fixture(
            &db,
            &home,
            &project,
            "nsfw-studio",
            current_session,
            stopped_session,
        );
        let proof = validate_test_codex_platform_migration(
            &db,
            "nsfw-studio",
            &ctx,
            Some(current_session),
            "",
            &registry_path,
        )
        .unwrap();
        let stopped_lineage_key = format!("claude_lineage_validated:{stopped_session}");
        let current_lineage_key = format!("claude_lineage_validated:{current_session}");
        let unrelated_lineage_key = "claude_lineage_validated:rollback-unrelated-session";
        db.kv_set(&stopped_lineage_key, Some("nsfw-studio"))
            .unwrap();
        db.kv_set(&current_lineage_key, Some("rollback-current-owner"))
            .unwrap();
        db.kv_set(unrelated_lineage_key, Some("rollback-unrelated-owner"))
            .unwrap();
        insert_test_claude_actor_capability(
            &db,
            "rollback-role-capability",
            "other-rollback-session",
            "nsfw-studio",
        );
        insert_test_claude_actor_capability(
            &db,
            "rollback-stopped-session-capability",
            stopped_session,
            "rollback-stale-child",
        );
        insert_test_claude_actor_capability(
            &db,
            "rollback-current-session-capability",
            current_session,
            "rollback-current-owner",
        );
        insert_test_claude_actor_capability(
            &db,
            "rollback-unrelated-capability",
            "rollback-unrelated-session",
            "rollback-unrelated-owner",
        );
        db.conn()
            .execute_batch(
                "CREATE TRIGGER fail_platform_migration_subscription
                 BEFORE INSERT ON kv
                 WHEN NEW.key LIKE 'events_sub:%'
                 BEGIN
                   SELECT RAISE(ABORT, 'forced platform migration subscription failure');
                 END;",
            )
            .unwrap();

        let error = commit_codex_platform_migration(&db, "nsfw-studio", &ctx, &proof).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("forced platform migration subscription failure")
        );
        assert!(db.get_instance_full("nsfw-studio").unwrap().is_none());
        assert!(db.get_session_binding(current_session).unwrap().is_none());
        assert_eq!(
            db.kv_get(&stopped_lineage_key).unwrap().as_deref(),
            Some("nsfw-studio"),
            "lineage revocation must roll back when subscriptions cannot be restored"
        );
        assert_eq!(
            db.kv_get(&current_lineage_key).unwrap().as_deref(),
            Some("rollback-current-owner"),
            "current-session lineage revocation must roll back with the migration"
        );
        assert_eq!(
            db.kv_get(unrelated_lineage_key).unwrap().as_deref(),
            Some("rollback-unrelated-owner")
        );
        let rolled_back_capability_count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM claude_actor_capabilities
                 WHERE instance_name='nsfw-studio'
                    OR session_id IN (?1, ?2)",
                rusqlite::params![stopped_session, current_session],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            rolled_back_capability_count, 3,
            "actor-capability revocation must roll back when subscriptions cannot be restored"
        );
        let stale_endpoint_count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM notify_endpoints WHERE instance='nsfw-studio'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stale_endpoint_count, 2);
        let restored_endpoints = validate_legacy_platform_endpoints(
            db.conn(),
            "nsfw-studio",
            crate::shared::time::now_epoch_f64(),
        )
        .unwrap();
        assert_eq!(restored_endpoints, proof.legacy_endpoints);
        let migration_event_count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM events
                 WHERE data LIKE '%explicit-start-migrate-platform-v1%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(migration_event_count, 0);

        db.conn()
            .execute_batch("DROP TRIGGER fail_platform_migration_subscription;")
            .unwrap();
        commit_codex_platform_migration(&db, "nsfw-studio", &ctx, &proof).unwrap();
        assert_eq!(
            db.get_session_binding(current_session).unwrap().as_deref(),
            Some("nsfw-studio")
        );
        assert!(db.kv_get(&stopped_lineage_key).unwrap().is_none());
        assert!(db.kv_get(&current_lineage_key).unwrap().is_none());
        assert_eq!(
            db.kv_get(unrelated_lineage_key).unwrap().as_deref(),
            Some("rollback-unrelated-owner")
        );
        let stale_capability_count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM claude_actor_capabilities
                 WHERE instance_name='nsfw-studio'
                    OR session_id IN (?1, ?2)",
                rusqlite::params![stopped_session, current_session],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stale_capability_count, 0);
        let unrelated_capability_count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM claude_actor_capabilities
                 WHERE token='rollback-unrelated-capability'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(unrelated_capability_count, 1);
        let endpoint_count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM notify_endpoints WHERE instance='nsfw-studio'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(endpoint_count, 0);
        let subscription_count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM kv
                 WHERE key LIKE 'events_sub:%'
                   AND json_extract(value, '$.caller')='nsfw-studio'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(subscription_count > 0);
    }

    #[test]
    #[serial]
    fn test_codex_platform_migration_rejects_registry_platform_mismatch() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let project = tmp.path().join("NSFWStudio");
        let (ctx, registry_path) = setup_platform_migration_fixture(
            &db,
            &home,
            &project,
            "nsfw-studio",
            "current-registry-mismatch",
            "stopped-registry-mismatch",
        );
        let directory = project.to_string_lossy().to_string();
        std::fs::write(
            &registry_path,
            serde_json::to_vec(&json!({
                "accounts": {"hcc": {"platform": "claude"}},
                "devs": [{
                    "role": "nsfw-studio",
                    "owner": "GMC",
                    "platform": "claude",
                    "account": "hcc",
                    "dir": directory
                }]
            }))
            .unwrap(),
        )
        .unwrap();

        let error = validate_test_codex_platform_migration(
            &db,
            "nsfw-studio",
            &ctx,
            Some("current-registry-mismatch"),
            "",
            &registry_path,
        )
        .unwrap_err();
        assert!(error.to_string().contains("not 'codex'"));
        assert!(db.get_instance_full("nsfw-studio").unwrap().is_none());
    }

    #[test]
    #[serial]
    fn test_codex_platform_migration_rejects_future_stopped_cursor() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let project = tmp.path().join("NSFWStudio");
        let current_session = "current-future-cursor";
        let (_ctx, _registry_path) = setup_platform_migration_fixture(
            &db,
            &home,
            &project,
            "nsfw-studio",
            current_session,
            "stopped-future-cursor",
        );
        let (event_id, event_data): (i64, String) = db
            .conn()
            .query_row(
                "SELECT id, data FROM events
                 WHERE type='life' AND instance='nsfw-studio'
                 ORDER BY id DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let mut poisoned: serde_json::Value = serde_json::from_str(&event_data).unwrap();
        poisoned["snapshot"]["last_event_id"] = json!(i64::MAX);
        db.conn()
            .execute(
                "UPDATE events SET data=?1 WHERE id=?2",
                params![serde_json::to_string(&poisoned).unwrap(), event_id],
            )
            .unwrap();
        let ctx = make_codex_ctx(Some(current_session), project.to_string_lossy().as_ref());
        let registry_path = home.join("authority").join("DEV-REGISTRY.json");

        let error = validate_test_codex_platform_migration(
            &db,
            "nsfw-studio",
            &ctx,
            Some(current_session),
            "",
            &registry_path,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("stopped event cursor is not before its lifecycle event")
        );
        assert!(db.get_instance_full("nsfw-studio").unwrap().is_none());
    }

    #[test]
    #[serial]
    fn test_codex_platform_migration_never_takes_over_live_holder() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let project = tmp.path().join("NSFWStudio");
        let (ctx, registry_path) = setup_platform_migration_fixture(
            &db,
            &home,
            &project,
            "nsfw-studio",
            "current-live-holder",
            "stopped-live-holder",
        );
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, session_id, tool, directory, status, created_at)
                 VALUES ('nsfw-studio', 'revived-holder', 'claude', ?1, 'active', 1)",
                params![project.to_string_lossy().as_ref()],
            )
            .unwrap();

        let error = validate_test_codex_platform_migration(
            &db,
            "nsfw-studio",
            &ctx,
            Some("current-live-holder"),
            "",
            &registry_path,
        )
        .unwrap_err();
        assert!(error.to_string().contains("identity is still live"));
        assert_eq!(
            db.get_instance_full("nsfw-studio")
                .unwrap()
                .unwrap()
                .session_id
                .as_deref(),
            Some("revived-holder")
        );
    }

    #[test]
    #[serial]
    fn test_codex_platform_migration_rejects_newer_lifecycle_event_at_commit() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let project = tmp.path().join("NSFWStudio");
        let current_session = "current-life-race";
        let (ctx, registry_path) = setup_platform_migration_fixture(
            &db,
            &home,
            &project,
            "nsfw-studio",
            current_session,
            "stopped-life-race",
        );
        let proof = validate_test_codex_platform_migration(
            &db,
            "nsfw-studio",
            &ctx,
            Some(current_session),
            "",
            &registry_path,
        )
        .unwrap();
        db.log_event(
            "life",
            "nsfw-studio",
            &json!({"action": "started", "tool": "claude"}),
        )
        .unwrap();

        let error = commit_codex_platform_migration(&db, "nsfw-studio", &ctx, &proof).unwrap_err();
        assert!(error.to_string().contains("changed during verification"));
        assert!(db.get_instance_full("nsfw-studio").unwrap().is_none());
    }

    #[test]
    #[serial]
    fn test_codex_platform_migration_rejects_revived_old_session_at_commit() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let project = tmp.path().join("NSFWStudio");
        let current_session = "current-session-race";
        let stopped_session = "stopped-session-race";
        let (ctx, registry_path) = setup_platform_migration_fixture(
            &db,
            &home,
            &project,
            "nsfw-studio",
            current_session,
            stopped_session,
        );
        let proof = validate_test_codex_platform_migration(
            &db,
            "nsfw-studio",
            &ctx,
            Some(current_session),
            "",
            &registry_path,
        )
        .unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, session_id, tool, directory, status, created_at)
                 VALUES ('revived-claude', ?1, 'claude', '/different/project', 'active', 1)",
                params![stopped_session],
            )
            .unwrap();
        db.set_session_binding(stopped_session, "revived-claude")
            .unwrap();

        let error = commit_codex_platform_migration(&db, "nsfw-studio", &ctx, &proof).unwrap_err();
        assert!(error.to_string().contains("already bound"));
        assert!(db.get_instance_full("nsfw-studio").unwrap().is_none());
        assert_eq!(
            db.get_session_binding(stopped_session).unwrap().as_deref(),
            Some("revived-claude")
        );
    }

    #[test]
    #[serial]
    fn test_codex_platform_migration_rejects_authority_change_at_commit() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let project = tmp.path().join("NSFWStudio");
        let current_session = "current-authority-race";
        let (ctx, registry_path) = setup_platform_migration_fixture(
            &db,
            &home,
            &project,
            "nsfw-studio",
            current_session,
            "stopped-authority-race",
        );
        let proof = validate_test_codex_platform_migration(
            &db,
            "nsfw-studio",
            &ctx,
            Some(current_session),
            "",
            &registry_path,
        )
        .unwrap();
        std::fs::write(
            project.join(".estate").join("role.json"),
            br#"{"estate_schema":1,"role":"nsfw-studio","owner":"HCC"}"#,
        )
        .unwrap();

        let error = commit_codex_platform_migration(&db, "nsfw-studio", &ctx, &proof).unwrap_err();
        assert!(error.to_string().contains("authority changed"));
        assert!(db.get_instance_full("nsfw-studio").unwrap().is_none());
    }

    #[test]
    #[serial]
    fn test_unidentifiable_claude_start_lists_unbound_candidates() {
        let (_dir, hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        assert!(crate::hooks::claude::setup_claude_hooks(false));

        let cwd = std::env::current_dir().unwrap();
        let ctx = make_claude_ctx(None, cwd.to_str().unwrap());

        assert_eq!(start_bare(&db, &hcom_dir, &ctx, None).unwrap(), 0);
        let first = db
            .iter_instances_full()
            .unwrap()
            .into_iter()
            .find(|row| row.tool == "claude")
            .expect("first start creates an identity")
            .name;
        assert!(
            db.get_instance_full(&first)
                .unwrap()
                .unwrap()
                .session_id
                .is_none(),
            "a session with no id leaves the row unbound"
        );

        // Without any session id hcom still cannot recognize the session, so the
        // second start mints another identity — the warning names this one back.
        assert_eq!(start_bare(&db, &hcom_dir, &ctx, None).unwrap(), 0);
        let second = db
            .iter_instances_full()
            .unwrap()
            .into_iter()
            .find(|row| row.tool == "claude" && row.name != first)
            .expect("second start mints a second identity")
            .name;
        assert_eq!(
            unbound_claude_candidates(&db, &ctx, &second),
            vec![first],
            "the earlier unbound identity is the reclaim candidate"
        );
    }

    #[test]
    #[serial]
    fn test_root_rebind_preserves_child_hierarchy_and_actor_state() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();

        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, session_id, tool, status, status_time, last_seen, created_at)
                 VALUES ('nova', 'sess-1', 'claude', 'active', 0, 0, 0)",
                [],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, parent_session_id, parent_name, agent_id, tool, status,
                  status_time, last_seen, created_at)
                 VALUES ('nova_task_1', 'sess-1', 'nova', 'agent-1', 'claude',
                         'active', 0, 0, 0)",
                [],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, parent_session_id, parent_name, agent_id, tool, status,
                  status_time, last_seen, created_at)
                 VALUES ('nova_task_2', 'sess-1', 'nova_task_1', 'agent-2', 'claude',
                         'active', 0, 0, 0)",
                [],
            )
            .unwrap();

        let token = db
            .issue_claude_actor_capability("sess-1", "tool-root", None, "nova")
            .unwrap();

        let links = snapshot_child_links(&db, Some("sess-1")).unwrap();
        assert_eq!(links.len(), 2);
        db.delete_instance("nova").unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, session_id, tool, status, status_time, last_seen, created_at)
                 VALUES ('sol', 'sess-1', 'claude', 'active', 0, 0, 0)",
                [],
            )
            .unwrap();

        restore_child_links_after_root_rebind(&db, &links, "sess-1", "nova", "sol").unwrap();
        db.rebind_claude_root_actor_state("sess-1", "nova", "sol")
            .unwrap();

        let direct = db.get_instance_full("nova_task_1").unwrap().unwrap();
        assert_eq!(direct.parent_session_id.as_deref(), Some("sess-1"));
        assert_eq!(direct.parent_name.as_deref(), Some("sol"));
        let nested = db.get_instance_full("nova_task_2").unwrap().unwrap();
        assert_eq!(nested.parent_session_id.as_deref(), Some("sess-1"));
        assert_eq!(nested.parent_name.as_deref(), Some("nova_task_1"));
        assert_eq!(
            db.resolve_claude_actor_capability(&token, "sess-1")
                .unwrap(),
            Some("sol".to_string())
        );
    }

    #[test]
    #[serial]
    fn test_same_name_root_rebind_restores_child_session_links() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, session_id, tool, directory, status, status_time, last_seen, created_at)
                 VALUES ('nova', 'sess-1', 'claude', '/tmp/project', 'active', 0, 0, 1)",
                [],
            )
            .unwrap();
        db.set_session_binding("sess-1", "nova").unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, parent_session_id, parent_name, agent_id, tool, status,
                  status_time, last_seen, created_at)
                 VALUES ('nova_task_1', 'sess-1', 'nova', 'agent-1', 'claude',
                         'active', 0, 0, 2)",
                [],
            )
            .unwrap();
        let token = db
            .issue_claude_actor_capability("sess-1", "tool-child", Some("agent-1"), "nova_task_1")
            .unwrap();

        let ctx = make_ctx(&[("CLAUDECODE", "1")], "/tmp/project");
        assert_eq!(start_rebind(&db, "nova", &ctx, Some("nova")).unwrap(), 0);

        let child = db.get_instance_full("nova_task_1").unwrap().unwrap();
        assert_eq!(child.parent_session_id.as_deref(), Some("sess-1"));
        assert_eq!(child.parent_name.as_deref(), Some("nova"));
        assert_eq!(
            db.resolve_claude_actor_capability(&token, "sess-1")
                .unwrap(),
            Some("nova_task_1".to_string())
        );
    }

    #[test]
    #[serial]
    fn test_start_rebind_rejects_cross_tool_stopped_snapshot_hijack() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();

        log_stopped_snapshot(
            &db,
            "fama",
            "codex",
            "/tmp/dasha-code/.worktrees/layer1-basic-conversation-fixes",
            "sid-fama",
            42,
        );

        let ctx = make_ctx(
            &[("CLAUDECODE", "1")],
            "/tmp/hcom-gan-harness/.worktrees/bench-infra",
        );

        let err = start_rebind(&db, "fama", &ctx, None).unwrap_err();
        assert!(
            err.to_string().contains("Refusing to reclaim 'fama'"),
            "unexpected error: {err}"
        );

        assert!(db.get_instance_full("fama").unwrap().is_none());
        assert_eq!(db.get_session_binding("sid-fama").unwrap(), None);
    }

    #[test]
    #[serial]
    fn test_start_rebind_allows_matching_stopped_snapshot_reclaim() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();

        log_stopped_snapshot(
            &db,
            "nova",
            "claude",
            "/tmp/dasha-code/.worktrees/layer1-basic-conversation-fixes",
            "sid-nova",
            77,
        );

        let ctx = make_ctx(
            &[("CLAUDECODE", "1")],
            "/tmp/dasha-code/.worktrees/layer1-basic-conversation-fixes",
        );

        let exit_code = start_rebind(&db, "nova", &ctx, None).unwrap();
        assert_eq!(exit_code, 0);

        let inst = db.get_instance_full("nova").unwrap().unwrap();
        assert_eq!(inst.tool, "claude");
        assert_eq!(
            inst.directory,
            "/tmp/dasha-code/.worktrees/layer1-basic-conversation-fixes"
        );
        assert_eq!(inst.last_event_id, 77);
    }

    #[test]
    #[serial]
    fn test_start_rebind_rejects_cross_directory_stopped_snapshot_hijack() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();

        log_stopped_snapshot(
            &db,
            "mira",
            "claude",
            "/tmp/dasha-code/.worktrees/layer1-basic-conversation-fixes",
            "sid-mira",
            18,
        );

        let ctx = make_ctx(
            &[("CLAUDECODE", "1")],
            "/tmp/hcom-gan-harness/.worktrees/bench-infra",
        );

        let err = start_rebind(&db, "mira", &ctx, None).unwrap_err();
        assert!(
            err.to_string().contains("Refusing to reclaim 'mira'"),
            "unexpected error: {err}"
        );

        assert!(db.get_instance_full("mira").unwrap().is_none());
    }

    #[test]
    #[cfg(unix)]
    fn test_same_path_resolves_symlink_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        let alias = dir.path().join("alias");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();

        assert!(same_path(
            real.to_string_lossy().as_ref(),
            alias.to_string_lossy().as_ref()
        ));
    }
}
