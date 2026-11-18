# A WinFsp write answered before its publication was lost across a daemon kill

Date: 2026-11-18. Area: `crates/bridge-winfsp/src/host.rs` (`Op::needs_barrier`), `crates/server/src/winfsp.rs` (the
barrier). Design: §4.6 "Windows", §4.8 (the barrier, D-18), A-198, A-358; amendment A-379. Closes the GAPS entry "a
WinFsp write answered before its publication may be lost across a daemon kill". This record is A-358's sibling: the
bug record of 2026-11-18, "an fsynced file came back empty after a daemon kill under the macOS mount".

## Description

A program writes a new file through a WinFsp drive and the daemon answers the write. The daemon is then killed before
the file's handle is flushed or closed. The drive outlives the daemon (A-198), and the handle's `FlushFileBuffers` is
answered by the successor with success. The file holds nothing of the write.

`a_write_answered_before_its_daemons_kill_is_kept_or_its_flush_refused` (`crates/cli/tests/winfsp.rs`) runs on CI's two
Windows runners through diagnostics branches. Each of five rounds:
- creates a new file through the drive, opened unbuffered, so the write reaches the daemon when it is made;
- writes its first page;
- kills the daemon;
- waits for the successor on the same drive, then flushes and reads the page back uncached.

- On main (diagnostics run 44360312582) all 5 rounds lost their write on each runner. Every flush succeeded, and every
  new file then read back empty, 0 of its 4,096 bytes.
- With the fix (run 44360313388) every write was kept: 5 of 5 on each runner.

The test's first draft overwrote one page of an existing file, and kept 5 of 5 on main on both runners (diagnostics run
44354915582). An overwrite inside a length the last publication holds lands in place, in the file's open extent in
anchor-owned memory (§4.8), and survives with no publication. A new file's bytes run past every published length.

## Root cause

The WinFsp host published before a reply only for a namespace or size change, a flush, and the close of a handle that
changed its file (`Op::needs_barrier`). Its stated reason was that "Windows' cache writes back lazily, and its handle's
close or flush carries the barrier, as a FUSE close's `flush` does". That holds for FUSE because FUSE's write log keeps
every answered write in anchor-owned memory for a successor to replay (A-63). Windows has no write log: `write_log` is
`#[cfg(unix)]`.

Since A-198 a WinFsp drive outlives a killed daemon. A write answered before any publication therefore died with its
daemon, the successor recovered the last publication, and the next flush published that state and answered success.
Windows had already completed the write and had nothing left to send.

## Impact

Every WinFsp drive. A daemon death between a write's reply and the next flush or close lost that write's new bytes, and
the next flush reported success. Deaths include a crash, the watchdog's end of a stuck daemon, `TerminateProcess`, an
upgrade and memory pressure.

## Exact edits (A-379)

- `crates/bridge-winfsp/src/host.rs`: `Op::needs_barrier` names `Op::Write`, so every write publishes before its reply.
  A purge's own write is answered without a publication, as a purge's close already was: it changes nothing
  (`crate::purge`).
- `crates/server/src/winfsp.rs`: the module doc states the rule. The reply of a write claims no durability past the
  daemon (`Served::claims_durability` is a flush's), so on a live-shipped volume a write waits for no candidates and the
  flush still does (A-200).
- `crates/cli/tests/winfsp.rs`: the test above.
- §4.6 "Windows" in `docs/wip/SLATES_DESIGN.md`, the amendment log (A-379), and GAPS.

## Tests

- The test above: red on main and green with the fix, on CI's x86_64 and arm64 Windows runners (runs 44360312582 and
  44360313388).
- With the fix, the WinFsp bridge's library tests, the WinFsp CLI suite (A-198's twenty-kill takeover with no drive
  lost) and the WinFsp semantics suite pass on both runners (diagnostics runs 44356053019 and 44360313388).

## Siblings

How each transport keeps a write its daemon answered once that daemon dies (the table is in GAPS):
- **NFS:** published before its reply (A-358).
- **FUSE mounts (Linux):** logged in anchor-owned memory before the reply, and replayed by the successor (A-63).
- **Shards streams (Linux, macOS):** logged before the reply (`shards::log_write`; A-316, A-339).
  - A VM outlives its daemon on this route: resync, anchor-held DAX objects (A-365), and the guest's unsynced stores
    written back before the next daemon serves.
  - `a_stream_is_resynced_after_its_daemons_death_and_its_unanswered_create_is_answered_once` reads an acknowledged write
    back after a kill.
- **WinFsp:** published before its reply (this change).
- **Shards streams (Windows) and the vhost-user device: neither.**
  - A Windows stream ends with its daemon. crosvm takes a vhost-user back end's hangup for a crashed device, and the VM
    ends.
  - `slates_bridge_fuse::bridge::needs_barrier`, which both use, names no WRITE.
  - They are safe only while their guest dies with the daemon. The work that lets one outlive it ("Windows streams have
    no write log", g268; "a VM that outlives its device's daemon", Phase 8 step 4) must bring the write log or A-358's
    rule.
