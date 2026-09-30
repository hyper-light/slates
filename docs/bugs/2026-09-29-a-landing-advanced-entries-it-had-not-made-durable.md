# A landing advanced entries it had not made durable, and hid its cleanup failures (2026-09-29, AUD-29-05)

## Description

§4.15 step 8 syncs every touched directory, and the media when the grant asks for it. Step 9 then advances
the landed entries out of the volume's overlay: from then on the volume reads them from the disk. The
failure matrix promises that a power loss after the report is masked, because the syncs came before it.

The engine did not keep that promise, and did not tell the caller when it failed:

- A touched directory that could not be opened for its sync was skipped (`let Ok(dir) = … else
  continue`), and the report still said the directories were synced.
- A sync failure the engine did not treat as a crash — a full disk (`ENOSPC`/`EDQUOT`) or a typed refusal
  — cleared `dirs_synced` and nothing else. The landing ended `Done`, and every entry was advanced out of
  the overlay although its directory entry was not known durable. A power loss could then lose the landed
  file while the volume no longer held the private copy.
- A failed media barrier the grant asked for likewise ended `Done` and advanced everything.
- The sweep's errors were dropped: `unwrap_or(0)` for the sweep, `.is_ok()` and `unwrap_or(false)` for
  each sibling. A temporary a failed write could not remove was dropped too (`let _ = unlink`). Nothing
  in the report said an entry of the landing's own stayed on the disk.
- The server's reply carried the counts only. Durability, every Degraded cell and the ramp's depth never
  reached the client, CLI, MCP or SDK.
- `terminal_state` counted every skip as done, so a landing whose entries were skipped with their parent
  missing, or after the grant ended, reported `Done`. The failure matrix says a grant ending mid-landing is
  "Partial with the reason".

Found while writing the tests: a `mkdir` that met an existing name counted as "already there" whatever
the name held. When an outsider had put a file there, the directory was advanced over that file, with the
private work beneath it at risk.

## Root cause

The engine treated durability as a report field, never as a condition of advancement. Only a crash-like
errno aborted a landing. Every other failure was either folded into a boolean or discarded, and the
server's reply type had no place for any of it.

## Impact

- **Private work could be lost.** An entry could leave the overlay before it was durable on the disk.
- **Reports were false.** `Done` was reported for landings that were not durable, or not complete.
- **Leftovers were invisible.** Entries of the landing's own could stay on the disk unreported.
- **Callers were blind.** No surface could see a durability failure or a Degraded cell.

## Exact edits

- **The durability boundary** (`crates/land/src/engine.rs`).
  - `sync_all` records every touched directory it cannot open or sync as `Degradation::Unsynced { dir,
    error }`. A failed requested media barrier becomes `MediaUnsynced { error }`, as against
    `BarriersOnly` when media was not asked for.
  - A crash-like errno still aborts (`note_crash`).
  - An entry advances only when `durable`: its directory synced (a rename's two), and the media barrier
    held when asked for.
  - The rest stay in the overlay and are counted in `LandingReport::held`. The resume re-validates them,
    marks their directories touched (validation already did), syncs, and advances them.
- **`Durability`** gains `media_requested`.
- **`terminal_state`** is `Done` only when every entry advances and nothing is held; otherwise `Partial`.
- **The sweep** returns its count and reports each sibling it cannot settle as `Leftover { path, error }`
  and each directory it cannot list as `Unswept { dir, error }`. The helpers it uses now return the host's
  own error type.
- **A failed write's temporary** that cannot be removed is `Leftover`. `NotFound`, for an unnamed
  temporary, is expected and not reported.
- **A `mkdir` that meets its name taken** is "already there" only when the entry is a directory
  (`existing_directory`); a file is `Conflict(TypeChanged)`.
- **`crates/vfs/src/host/sim.rs`:** targeted faults (`SimHost::fail`: a verb, a path prefix, the host's
  answer and a count) for open, directory sync, media sync, file sync and unlink.
- **`crates/ipc/src/protocol.rs`:** `LandingOutcome` gains `held`, `durability: LandingDurability`,
  `degraded: Vec<LandingDegradation>` (with `HostAnswer`) and `ramp_depth`.
- **Surfaces.** `crates/server/src/landing.rs` fills the new fields from the report. The CLI prints them,
  MCP's `outcome_json` (which the CLI's `--json` shares) carries them, and the Node SDK's landing object
  carries them.

Part of the engine and simulated-host edits was committed inside `ca53844` (the audit record), which
another session made with the working tree's changes staged. This record and the commit that follows it
complete the change.

## Evidence

- **Failing tests first.** `crates/land/tests/durability.rs` has five by-use histories. Run on the engine
  as it stood at `bc81da4`, with the new simulated faults and trimmed to what that engine can express:

  | History | Old engine |
  |---|---|
  | Directory sync answering `ENOSPC` | `Done`, both entries advanced |
  | `mkdir` meeting an outsider's file | `Skipped(AlreadyThere)` |
  | Media barrier answering `ENOSPC` | `Done` |
  | Sibling the sweep could not remove | Everything advanced, so no resume ever swept it |

  The fifth history (a full disk mid-write, plus a temporary that cannot be removed) behaved safely before;
  only its report of the `Leftover` is new.
- **Now all five pass.** Each checks the public report, that the private work stays in the overlay, and
  that a resume reaches the reference with nothing held, no plan left and none of the landing's names on
  the disk.
- **The reply.** The daemon's grant scenario asserts that the landed reply carries its durability,
  `[BarriersOnly]`, `held == 0` and the ramp depth.
- **Suites.**
  - slates-land: unit 6, durability 5, grant_binding 3, oracle 20, os 5, os_removal 1, removal 2. The
    real-directory tests also ran on Linux `/dev/shm`.
  - slates-server: lib 124, daemon 16, recovery 7.
  - MCP, CLI (13) and the Node SDK suites.
  - Clippy clean on macOS, Linux and Windows; xtask check ok.

## Siblings found, open

- **A media barrier the filesystem cannot perform** (e.g. `F_FULLFSYNC` unsupported) would hold a landing's
  entries on every attempt. A grant asking for media durability on such a target should be refused before
  the lease, from a capability.
- **The land verb never asks for media durability** (`media_durability: false` in
  `crates/server/src/landing.rs`). Media durability is reachable only through the engine.
