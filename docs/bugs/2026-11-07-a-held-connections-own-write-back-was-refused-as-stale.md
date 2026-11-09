# A held NFS connection's own write-back was refused as stale (A-231's carry)

- **Date:** 2026-11-07. **Status:** fixed. **Found by:** main CI 42395077767 on e86fb417 (job 128470582463): fsx seed 1,
  10,000 operations, file length 262,144, over the macOS NFS mount, failed once the bug-sweep stack landed; the same lane
  passed on 7ce90ebb.

## Description

A-231 carries each held connection's byte states in every publication (`nfs_writeback::carry`). To carry them as of the
moment, it consumed every tracker's journal into the live states first. But a publication runs inside a call: a WRITE
publishes its barrier before its reply, and its own records are noted as the connection's (`nfs_writeback::made`) only
after. So the connection's own write was consumed as another path's change, and the bytes it had fetched and then
written went stale in its own states. Its next write-back of the same page, holding a new store, differed from the
file there and was refused `NFS3ERR_IO`. fsx's MAPWRITE writes a page back again and again, and on macOS the anchor
holds the mount's connection, so every publication carried: the lane failed.

## Evidence

- `a_held_connections_own_write_backs_are_never_refused_as_stale` (`crates/cli/tests/nfs_held.rs`, macOS, a connection
  the anchor holds): fetch a page, then write it back twenty times, each with one more byte stored. Before: rounds 1 to
  19 refused `NFS3ERR_IO` (status 5), every write-back after the first. After: all applied, the file the last page.
- The only change between the two runs is `carry` no longer consuming the live states: the cause is A-231's carry.
- CI, the A/B: `diag-bug-sweep-17` (run 42425914509, macos-latest, macOS 26.7.2) built e86fb417 and the fix each into
  its own target dir and ran `cargo xtask conformance all` four times, interleaved. e86fb417 failed fsx seed 1 over
  native-macos-nfs in both rounds (23:39 and 23:48 UTC, load 6.6 and 1.5); the fix passed both (23:44 and 23:53, load
  1.4 and 1.9). pjdfstest was the same in all four: 6,781 passed, 1,905 expected failures, 0 unexpected. Main's next
  macOS conformance, on 4811191a with the fix, passed (run 42434181746).
- `diag-bug-sweep-15` and `-16` passed every tree and do not count: the trees shared one `CARGO_TARGET_DIR`, and the
  conformance harness runs the binary in it, so each may have run the last one built.

## Root cause

A publication's carry changed the live states mid-call, before the call's own records were noted.

## Fix

`carry` reads a copy: each held connection's states cloned, the changes its tracker has not consumed applied to the
copy (a record not yet noted counts there as another path's, which only makes a successor more careful), and the copy
carried. The live states and the tracker's cursor are left to `judge` and the keeper, as before A-231. A-231's crash
model (`a_stale_write_back_is_refused_at_every_crash_point_of_its_history`) still refuses the stale write-back at all
six crash points.
