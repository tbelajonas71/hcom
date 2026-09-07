# Hook-only Claude registration retention — 2026-09-07

Owner: GMC / image-media-studio. Board ticket: `20260905T060953-image-media-studio`.
Tom authorized GMC to repair HCC communications in the existing desktop task.

## Proven defect and scope

Installed ancestor `d186d35b0a8e1a057f5b6a5ffd10869bdb5fd430` protected a hook-only
Claude row only when it was stored as inactive/exit:timeout. An older stored
active, listening or blocked row instead becomes computed inactive/stale, and
the list command's 3600-second stale cleanup removed its exact session binding.
Persisted HCC lifecycle event 113009 at 2026-09-07T02:22:19Z records stale_cleanup.
Later transcript metadata is NOT independent evidence of an active model turn.

Commit `2c039ee3860b8b1efc2034c193859bd499c87917` retains these age-derived stale
rows only under the existing exact hook-only Claude ownership predicate. No
timestamp, status, cursor or successful-delivery assertion is fabricated.
Unbound, wrong-owner, process-bound and non-Claude cases remain subject to
cleanup. Explicit stopped/closed/interrupted/session-switch cases remain reaped.

## Tests and build

- Negative control before the production patch: the new stale-bound-row test
  failed because cleanup returned 1 instead of 0.
- An initial wrong-owner test fixture violated the session-binding foreign key;
  it was corrected by inserting a real different owner. That failed run is not
  counted as a production pass.
- Final current-source serial debug suite: 2160 passed, 0 failed, 1 ignored,
  103.43 seconds. Tests include queue retention without advancing the unread
  cursor and explicit-stop/incorrect-owner controls.
- Independent source review: no P0/P1 found in this scoped diff; live post-install
  delivery remains a separate acceptance check.
- `cargo build --release --offline --features estate-managed-update` succeeded.
  Toolchain 1.97.1. Existing unused `updated_at` warning in commands/start.rs was
  not changed. Only owned Rust files were rustfmt-formatted; unrelated pre-existing
  whole-tree formatting differences were not rewritten. `git diff --check` passed.

## Deployment and rollback

New artifact: 15,399,424 bytes, SHA256
`2551386260A9500DA5D16ADB7195935F6DF6C4B16E6B98064C4829879227AEAD`.
Both copies independently verified after installation:

- `C:\Users\tbela\.hcom\bin\hcom.exe` (canonical)
- `C:\Users\tbela\.cargo\bin\hcom.exe`

The tracked estate pin is `D:\Projects\hcc-migration\board\hcom-build-pin.json`.
The canonical wrapper returned `hcom 0.7.25` after the pin update.
Source commit was pushed to fork branch `estate/managed-update-20260901` and
independently matched with `git ls-remote`.

Installer:
`D:\Projects\Image & Media Studio\research\hcc-hook-repair-20260907\Install-VerifiedHcom.ps1`.
It checks both prior hashes, stages/verifies the release, uses recoverable exact
file renames, and rolls back already-switched copies if a later step fails.
No process was killed and no browser/client was restarted.

Rollback copies and prior pin:
`D:\Projects\Image & Media Studio\research\hcc-hook-repair-20260907\rollback-2c039ee`.
Prior hash: `13E1B405547CCF608EB921A1B254A1867BC5ACA544C82CD1F336A595CE3AEAA3`.
Each target also retains its `.pre-2c039ee` sibling. A rollback must claim the
binary/pin resources, verify these old copies, recoverably rename both installed
files, restore both old copies and the old pin together, then recheck all hashes.
Do not delete rollback artifacts or kill desktop processes to replace a locked file.

## Recovery proof and limitations

Existing GCC used Claude native cross-session messaging to contact the existing
HCC task, not create a replacement. Native transport accepted message
`9fa9b9bf-4fb2-4433-a26a-3e7576550c0d` to `hcc-backup-7b [201dd1]`.
GMC independently matched the peer name through the exact local session metadata
file `C:\Users\tbela\.claude\sessions\68008.json` to UUID
`f8f5bb1d-9ca8-42ba-9118-43f96375316c`, PID 68008 and `D:\Projects\hcc-backup`.
No adjacent session key, identity or credential was copied.

HCC ran its own supported startup, then returned hcom ACK 113593 at 02:43:05Z
linked to GMC request 112910. This proves recovery before installation, not the
new binary. Post-install request 113797 was accepted at 02:55:57Z; its correlated
reply must be checked separately in the durable event log before scoring a pass.

Retained registration does not mean alive, and a queued send does not mean read.
This fix does not establish that every historical delay shares this cause, solve
every cross-platform idle wake, or close unrelated transcript cross-binding issues.
The broad parent ticket stays open for those separate acceptance gaps.

Relevant primary documentation:
https://code.claude.com/docs/en/cross-session-messaging — the native existing-peer
route is separate from hcom and supports current local Claude sessions. Estate
rule 137's blanket statement that nothing external can wake an app session is
too broad; HCC has been asked to update that shared guidance with this distinction.
