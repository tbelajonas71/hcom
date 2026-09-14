# Hook-only Codex app registration retention — 2026-09-14

Owner: HCC helper session (parent hcom seat hcc2) on ORIGINPC. Tom, 2026-09-14:
"gmc is not offline for the 100th time you should assume its a comms problem and fix it".
Branch `fix/codex-app-hook-only-stale-retention`, code commit
`dab5f71` on top of estate tip `8ad959b`. BUILT, NOT DEPLOYED.

## Proven defect and scope

`cleanup_stale_instances` (src/instance_lifecycle.rs) runs on every `hcom list`
with a 3600-second stale threshold. Commit `2c039ee` (2026-09-07) exempted stale
rows only under `is_hook_only_claude_session`, and its record states that
"non-Claude cases remain subject to cleanup". A Codex desktop-app thread has
the same shape as Claude Desktop: it is bound only through Codex hooks, has no
process binding, and is silent between app turns. The Codex Stop hook writes
`listening`; with no TCP notify endpoint the row is computed `stale:listening`
10 s after its last hook, and 3600 s later `hcom list` calls `stop_instance`
with `stale_cleanup`. That deletes the row and its session binding. After that
`hcom send @name` is refused ("non-existent or stopped agents") until the
thread happens to take another turn and re-registers at a hook. To every other
node that is indistinguishable from "GMC offline".

## Evidence (C:\Users\tbela\.hcom\hcom.db, opened read-only)

- `image-media-studio` = Codex app thread `01a03982-34e7-7d01-8a7f-09fc3062c904`,
  cwd `D:\Projects\Image & Media Studio`. Life events 136053, 146304, 149870,
  150340, 162362 and 164567 all carry
  `{"action":"stopped","reason":"stale_cleanup","by":"system"}`. Each stop
  snapshot records `tool codex, pid null, launch_args null, background 0,
  origin_device_id null`. Every preceding `created` event has
  `is_hcom_launched: false`.
- Last cycle: status event 164303 set `listening` at 2026-09-14T01:17:06Z, and
  stale_cleanup 164567 followed at 02:17:19Z, 3613 s later.
- Scale: across all history, 189 stale_cleanup stops hit codex rows with no
  pid and no launch_args, against 5 on hcom-launched codex rows (pid and
  launch_args present).
- At build time the `image-media-studio` row is ABSENT: it is still reaped.
  Four other live codex rows (trove-enraged, cultivation, dnd5-game, vera)
  have the same shape: pid NULL, launch_args '', background 0, one session
  binding, zero process bindings.

## Predicate

hcom's own spawn marks, verified in source:
- `launcher.rs` binds `HCOM_PROCESS_ID` for every launch.
- The Codex launch branch always stores `launch_args` as a JSON array, `"[]"`
  when empty (`json_value_to_sql` stores arrays as strings).
- `finalize_background_launch` and pidtrack orphan recovery record `pid`.
- PTY exit deletes the row itself (`cleanup_deleted_instance`).

A Codex app row has none of these. The row reader maps '' to None.

```rust
pub(crate) fn is_hook_only_codex_app_session(data: &InstanceRow, db: &HcomDb) -> bool {
    if data.tool != "codex"
        || data.origin_device_id.as_deref().is_some_and(|device_id| !device_id.is_empty())
        || data.pid.is_some()
        || data.launch_args.is_some()
        || data.background != 0
        || db.has_process_binding_for_instance(&data.name)
    {
        return false;
    }
    data.session_id.as_deref().is_some_and(|session_id| {
        matches!(db.get_session_binding(session_id), Ok(Some(owner)) if owner == data.name)
    }) && db.session_binding_count_for_instance(&data.name) == 1
}

pub(crate) fn is_hook_only_app_session(data: &InstanceRow, db: &HcomDb) -> bool {
    is_hook_only_claude_session(data, db) || is_hook_only_codex_app_session(data, db)
}
```

`cleanup_stale_instances` now uses `is_hook_only_app_session`, with the same
limits as the Claude repair:
- It retains the row, its binding and its pending queue.
- It does not refresh `status_time` or `last_stop`, so the computed status
  stays `stale`.
- It applies only to computed stale rows or stored inactive/exit:timeout rows.
- exit:killed, exit:closed, exit:interrupted and exit:session_switch are still
  reaped, as are unbound, wrong-owner, process-bound, pid, launch_args,
  background, second-session-binding and non-codex/non-claude rows.
- `is_hook_only_claude_session` is unchanged, and so is its other caller, the
  Claude Stop hook.

`send`'s `deliverable_instances` SQL gains the mirror branch for a codex row
stored inactive/exit:timeout. Codex hooks do not write that state today:
`hcom listen` writes it only for adhoc. The branch is defensive, so a retained
row is always addressable. Stale rows stored listening/active/blocked were
already deliverable while the row existed; the defect was only that the row
was deleted. New DB helper: `session_binding_count_for_instance`.

## Negative control

The eight new tests were written first and run against the unmodified
production code (`cargo test --offline --features estate-managed-update --bin
hcom -- codex_app hcom_launched_codex`). Result: 4 passed, 4 failed. The
failures:
- `cleanup_preserves_bound_hook_only_codex_app_timeout`: cleanup returned 1,
  not 0.
- `cleanup_preserves_bound_hook_only_codex_app_stale_states`: returned 1, not
  0, failing on the `listening`/3613 s case that reproduces event 164567.
- `send_mention_survives_cleanup_for_stale_hook_only_codex_app`: the row was
  reaped.
- `send_mention_queues_for_hook_only_codex_app_timeout`: the recipient was
  refused.

The four control tests passed as expected: exact-binding exclusions, explicit
stops, and the hcom-launched send exclusions.

After the production change, one existing test failed:
`cleanup_stale_preservation_requires_exact_hook_only_claude_binding`, case
`different-tool`. That case used a bound, unspawned `codex` row, which is
exactly the shape this repair retains, and asserted it was reaped, which
encodes the defect. It now uses `gemini` and keeps its intent (a tool with no
hook-only exemption is reaped); the codex shape is covered by the new tests.

## Tests

- **Lifecycle and send modules, after rustfmt:** 73 passed, 0 failed (42 s,
  Claude session environment).
- **Full suite, branch binary, neutral environment** (CLAUDE* variables
  removed from the test process; test binary rebuilt, 2174 unit tests):
  unit tests 2173 passed, 0 failed, 1 ignored, 165.06 s.
- **Full suite, branch binary, Claude Code session environment:** 2143 passed,
  30 failed, 1 ignored, 106.58 s. All 30 are Codex start, rebind, relocation
  and platform-migration tests in `commands::start::tests`. They detect the
  current session as Claude because CLAUDECODE=1 and
  CLAUDE_CODE_ENTRYPOINT=claude-desktop are set ("latest identity used tool
  'codex' but current session is 'claude'"). The same 30 names fail on
  untouched `8ad959b` in that environment (33 passed, 30 failed, identical
  set), and all pass in the neutral run above. Environmental, not this change.
- **Discarded run:** one "neutral" full-suite run (2164 passed, 2 failed) was
  NOT this branch. The HEAD comparison worktree shared the target directory,
  and cargo reused its identically named test binary (2167 tests = HEAD's
  count, with CARGO_MANIFEST_DIR baked under TEMP, which failed the two
  `rejects_non_temp` path tests). It was superseded by the rebuilt run above.
- **Integration `tests/cli_smoke.rs` on the branch:** 23 passed, 1 failed:
  `antigravity_e2e_hook_dispatch` ("Intent 'request' requires exactly one
  recipient; resolved 0"). It repeats in isolation. The fixture is
  tool=antigravity with a process binding, so neither changed path can match
  it. HEAD comparison: untouched `8ad959b` in the same repo and environment
  gives the identical 23 passed / 1 failed on the same test. Pre-existing.
- **Remaining integration targets on the branch** (the real-tool and most PTY
  tests are `#[ignore]`), all ok:
  - real_tool_claude: 4 passed, 2 ignored
  - real_tool_codex: 3 passed, 2 ignored
  - test_pty_delivery: 0 passed, 10 ignored
  - test_relay_roundtrip: 5 passed, 1 ignored
- rustfmt --check passes on the three changed files; `git diff --check` passes.

## Build

`cargo build --release --offline --features estate-managed-update`, toolchain
cargo 1.97.1. Built from source identical to `dab5f71`: the tree was clean
after the commit apart from the pre-existing untracked `Microsoft/` folder.
Only the existing unused `updated_at` warning remains.

- Artifact: `C:\Users\tbela\.hcom\source\hcom-codex-delivery-reliability-20260901\target\release\hcom.exe`
- 15,416,320 bytes, SHA256
  `FD40C68284B7F3F32BDBF419886E1379EED5A06BB6CEB68B2E83FE5AE15134A8`
- The binary contains the new send SQL marker
  `Mirrors instance_lifecycle::is_hook_only_codex_app_session` and the count
  helper SQL.
- `--version` run with HCOM_DIR set to an empty scratch directory printed
  `hcom 0.7.25`; the directory stayed empty.

Not touched: `C:\Users\tbela\.hcom\bin\hcom.exe` (15,417,344 bytes, SHA256
`17340DE9F26660B03B8D20C58E394DD0A6A693483491053483F034D67655CD7B`, matches the
pin), `hcom.db`, `config.toml`, the pin file. Observed only:
`C:\Users\tbela\.cargo\bin\hcom.exe` is still the 2026-09-07 build `2551386…`.

## Deployment steps for HCC (not done)

1. Stop nothing. At build time the only running hcom process was
   `hcom.exe relay-worker` (PID 75316). It does not call stale cleanup (the
   only production caller is `hcom list`), so it needs no restart for this fix.
2. Recheck both hashes: the source must be `FD40C682…34A8` and the canonical
   `C:\Users\tbela\.hcom\bin\hcom.exe` must be `17340DE9…CD7B`. If the canonical
   hash differs, stop and re-read the pin before continuing.
3. Wait until no hcom command is mid-write: no `hcom.exe` process in
   `Win32_Process` other than the long-lived relay-worker. Windows will not
   overwrite a running image but will rename it. Rename the canonical file to
   `hcom.exe.pre-FD40C682` (the existing `.pre-<newhash>` convention), then
   copy the new build to `hcom.exe`. Do not delete the renamed file, and do not
   kill the relay worker to free the file.
4. Verify the installed file's SHA256 and length, and that `hcom --version`
   prints `hcom 0.7.25`.
5. Round trip: send one `--intent request` to a live Codex app peer and confirm
   the correlated reply in the durable event log. Then confirm the fix itself:
   after that peer has been silent for more than 3600 s, run `hcom list`
   (which runs cleanup), check the row is still listed as stale, and check a
   send to it is accepted and queued.
6. Update `D:\Projects\hcc-migration\board\hcom-build-pin.json` (sha256, bytes,
   source commit `dab5f71`, status) only after steps 4 and 5 pass.
7. Rollback: rename `hcom.exe` aside, rename `hcom.exe.pre-FD40C682` back, and
   recheck the hash against the pin.

## Limitations and what is not proven

- Nothing here is live proof. No binary was installed and no live round trip
  was run.
- Deployment does not recreate the already-reaped `image-media-studio` row. It
  re-registers at its next Codex hook (a turn or SessionStart); until then
  sends to it are still refused. Other routes (a native wake) are unaffected.
- Retained Codex app rows no longer age out through the stale or inactive
  tiers. Codex hooks have no SessionEnd handler (hooks/codex.rs handles
  SessionStart, UserPromptSubmit, PreToolUse, PostToolUse and Stop), so an
  abandoned app thread stays listed as stale until `hcom stop <name>` or a
  rebind. Claude Desktop rows took the same trade-off on 2026-09-07.
- The predicate trusts pid, launch_args, background and process bindings as
  the spawn marks. No production path was found that clears all of them on a
  live hcom-launched codex row, but none was exhaustively ruled out.
- Retained registration does not mean alive, and a queued send does not mean
  read.
