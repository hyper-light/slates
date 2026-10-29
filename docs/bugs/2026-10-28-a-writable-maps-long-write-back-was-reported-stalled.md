# A writable map's long write-back was reported stalled (2026-10-28)

**Status:** fixed the same day, failing test first.

## Description

CI's ThreadSanitizer lane (run 40816824486, slates 4552995b) failed
`a_writable_maps_object_changes_one_files_bytes_and_nothing_else`: the client's `release_map_write` of a 4 MiB writable
map answered `Stalled { after_ns: 1000000000 }`. The daemon was alive and working: the release compares the whole copy
back into the file before it replies (A-186), and under the lane's instrumentation that took longer than the client's
reply deadline, one liveness budget.

## Root cause

`slates_client::defers_reply` names the verbs whose reply the daemon defers until long work ends, which the client
waits for while its daemon lives rather than for one reply deadline. `MapRead` was there, since its copy is filled in
slices for as long as the file takes (A-171). A-186 added three verbs whose replies are deferred the same way and did
not add them: `MapWrite` (the same fill), `MapSync` and a writable map's `ReleaseMap` (the compare-back, in slices for
as long as the file takes). So any of them that outlasted one reply deadline (a large file, a slow or loaded machine)
was reported `Stalled`, while the daemon finished the work.

## Impact

A client mapping, syncing or releasing a large writable map on a slow machine got `Stalled` for work that then
completed: the store reached the file, but the client was told nothing did. No data was lost or misreported by the
daemon.

## Reproduction

`crates/client/tests/client.rs` `a_long_writable_map_is_waited_for_while_the_daemon_lives`: a client with a 1 ms reply
deadline maps a 16 MiB file read and write. Before the fix it failed at the map, `Stalled { after_ns: 1000000 }`
(adacachy, 2026-10-28).

## Exact edits

- `crates/client/src/client.rs` `defers_reply`: `MapWrite`, `MapSync` and `ReleaseMap` added beside `MapRead`. A
  read-only map's release answers at once, so waiting for it while the daemon lives changes nothing for it.
- The test above, green 3 of 3 on macOS and 2 of 3 on adacachy, whose second run was refused at an ordinary `attach`
  while the box (load 168 to 198) starved the daemon (a 6.7 s poll in its own log).
