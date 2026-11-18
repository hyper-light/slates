# An fsynced file came back empty after a daemon kill under the macOS mount

Date: 2026-11-18. Area: `crates/server/src/nfs.rs` (the barrier), `crates/bridge-nfs/src/procedures.rs` (WRITE and
COMMIT). Design: §4.6 (NFS v3), §4.8 (the barrier, D-18), A-113, A-128, A-200; amendment A-358. Closes the GAPS entry
"2026-10-06: two fsync-acknowledged files came back empty after daemon kills on the macOS NFS mount".

## Description

A program wrote a file through `slates mount` on macOS (Apple's NFSv3 kernel client), called `fsync`, got 0, closed it
and got 0, and after the daemon was killed and restarted the file read back empty.

The first sighting was 2026-10-06: two 300 KB files out of 2,320 came back empty, in the kill run that also panicked the
Mac. The Linux kernel client never lost one (2026-11-06: 13,917 files over NFSv3 and 13,994 over NFSv4.2, 15 kills
each), so the macOS client was the open case.

It was run again on 2026-11-18 under Ada's go ("Just fucking do it"). The machine was Adas-Mac-Studio-2 (kiln runner
b24c5c95): Apple M4 Max, 14 cores, 36 GiB, macOS 26.2 (25C56), xnu-12377.61.12, NFS kext UUID
B922B1FE-2CDC-3A5B-8757-37D99A870355. The binary was main at `1cd57d18b`, built in release on d1323898.

The harness is `docs/wip/bench/kill_fsync_macos.sh`, the macOS counterpart of `kill_fsync.sh`:
- the same writer, files of 100 B to 300 KB written whole, fsynced and closed, each hashed only after both returned;
- the daemon `SIGKILL`ed at random 0.4 to 1.6 s intervals;
- afterwards the volume unmounted while a daemon serves, mounted again, and every file read back.

| run | kills | files checked | intact | empty | other corrupt | missing | panics |
|---|---|---|---|---|---|---|---|
| main `1cd57d18b` | 2 | 3,025 | 3,024 | 1 (`f1915`, 224,026 bytes) | 0 | 0 | 0 |
| diagnostic `59c43709c` (main plus a line per NFS call) | 15 | 6,607 | 6,606 | 1 (`f5365`, 52,177 bytes) | 0 | 0 | 0 |

The first run meant to make no kill. BSD `seq 1 0` counts down, so it made two; the harness now counts with a loop.
Each empty file was open across a kill: `f1915` from .827 to .840, killed at .832; `f5365` from .358 to .374, killed at
.364.

## Root cause

The diagnostic run logged every NFS call each daemon served. For `f5365` (inode 281474977305847):

| time (s) | daemon | call | answer |
|---|---|---|---|
| .358819 | D1 (pid 95721) | CREATE `f5365`, UNCHECKED | `NFS3_OK`. The barrier published the empty file. |
| .359171 | D1 | WRITE at 0, 52,177 bytes, `UNSTABLE` | `UNSTABLE`, verifier `0003521062c77488` (V1) |
| .359193 to .359211 | D1 | sent the WRITE's reply, consumed its request | |
| .364 | | D1 killed. Nothing published since the CREATE: D2 took back only the CREATE's held reply. | |
| .371777 | D2 (pid 95735) | adopted the held connection (A-113) | |
| .373525 | D2 | COMMIT at 52,177, count 0 | barrier `ok`; `NFS3_OK`, verifier `000352109a78ac30` (V2) |
| .373695 | D2 | GETATTR | then the next file. No WRITE to this inode ever again. |

slates kept RFC 1813's rule. An `UNSTABLE` write was answered `UNSTABLE` and left in the volume until the next
publication. The daemon's death lost it. The successor answered with a new per-boot verifier, which tells a client to
write such data again (§3.3.7).

Apple's client did not write it again. It also returned 0 from `fsync` and `close`. This was not one bad instance:
- Across the traced run, 3 COMMITs answered a verifier other than their file's WRITE, and 0 were followed by a WRITE.
- The two files whose bytes survived anyway owed it to another publication that carried them before the kill. The one
  no publication carried came back empty.
- Every COMMIT this client sent, 17,631 of 17,631, started where its WRITE's bytes ended, 15,938 of them with count 0.
- Every WRITE, 19,324 of 19,324, asked for `UNSTABLE`.

The window is a write's reply until the shard's next publication. Apple's client sends its COMMIT about 5 ms after the
WRITE is answered (D1 sat idle from .359211 to the kill), so a death in those 5 ms took the bytes.

The bug on slates' side is that an acknowledged write's survival rested on the client. The 2026-09-13 fix
(`docs/bugs/2026-09-13-nfs-write-acknowledged-without-publish.md`) made `UNSTABLE` honest and left its bytes to the
COMMIT. With A-113 a daemon's death became something a client lives through on one connection. From then on, every
death between an `UNSTABLE` reply and the next publication depended on the client re-sending.

## Impact

Every macOS mount: `slates mount` is Apple's NFSv3 client. Any daemon death between a WRITE's reply and the next
publication on its shard lost that write's bytes while the program's `fsync` returned 0. Deaths include a crash, the
watchdog's end of a stuck daemon (A-232), `kill`, an upgrade and memory pressure. The traced run hit it once in 15 kills.

The Linux kernel client re-sends after a changed verifier and lost nothing. FUSE mounts have the write log (A-63).

## Exact edits (A-358)

- `crates/server/src/nfs.rs`:
  - `needs_barrier` names every successful WRITE, whatever stability it asked for, so its bytes are in the recovery
    image before its reply leaves. It takes no arguments now that the stability no longer decides it.
  - `changes_volume`, the A-127 record's test, is folded into `needs_barrier`, which now says the same.
  - `unstable_write` (new) and `barrier`'s `claims_stability`: an `UNSTABLE` write's reply still claims nothing past
    its daemon. It waits for no live-shipped volume's candidates; its COMMIT does (A-200). On a live-shipped volume an
    `UNSTABLE` write is therefore still answered while the candidates are stopped (`live_fleet.rs`).
  - The module doc states the rule.
- `crates/bridge-nfs/src/procedures.rs`:
  - The WRITE and COMMIT docs, and those of `UNSTABLE`, `write_stable_how` and `is_unstable`, say what the host now
    makes true. The wire is unchanged: a WRITE is answered at the level it asked for.
- `crates/bridge-nfs/src/v4/v42.rs`: the COPY's `UNSTABLE4` doc. Its pieces are WRITEs, so they are published too.
- `crates/cli/tests/nfs_held.rs`: the test below, with `fsync_like_apples_client` and `read_what_is_there`.
- `docs/wip/bench/kill_fsync_macos.sh` (new): the recorded command.
- §4.8 and A-127's text in `docs/wip/SLATES_DESIGN.md`, the amendment log (A-358), GAPS, BENCHMARKS.

## Tests

`an_acknowledged_write_survives_its_daemons_kill_with_no_rewrite` (`crates/cli/tests/nfs_held.rs`, gated
`SLATES_TEST_CLI=1`, macOS) plays Apple's client over one held connection under the real anchor:
- for each of six files: CREATE, one `UNSTABLE` WRITE of 52,177 bytes, `kill -9` of the daemon as soon as the WRITE is
  answered, then a COMMIT from the bytes' end with count 0, and never a WRITE again;
- then one more kill, and every file read back.

On main (red, d1323898, 2026-11-18): all 6 COMMITs met a successor's verifier, and 6 of 6 files came back with 0 of
52,177 bytes. With the fix: see the GAPS entry and BENCHMARKS for the green run and the kernel-client runs.

## Siblings (reported, not changed here)

- **WinFsp, likely live.** A plain write is answered before any publication: only a namespace or size change, a flush,
  and the close of a handle that wrote publish first (`crates/server/src/winfsp.rs`'s module doc; `Op::needs_barrier`
  in `crates/bridge-winfsp/src/host.rs`). Windows has no write log (`write_log` is `#[cfg(unix)]`). A WinFsp drive
  outlives a killed daemon (A-198: `a_winfsp_drive_handle_and_view_survive_twenty_killed_daemons`, zero drives lost of
  twenty).
  - So a daemon killed between a paging write's reply and the next flush or close loses that write. Windows' cache
    manager has already marked the pages clean, and the successor's flush then publishes a state without them and
    answers success.
  - Unproven. It needs this record's test shape on a WinFsp drive: a write, a kill, then a flush, a close and a read
    back.
  - The fix would be A-358's rule (every write published before its reply), or a write log on Windows.
- **Shards streams on macOS, covered.** Since `1fa154230` (main, 2026-11-18) a stream outlives its daemon on macOS, and
  its writes go to the write log there too (`write_log` is `#[cfg(unix)]`), so a successor replays them.
