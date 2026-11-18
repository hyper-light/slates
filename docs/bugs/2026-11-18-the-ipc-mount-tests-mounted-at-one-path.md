# The two IPC mount tests mounted at one path and tore each other down (2026-11-18, A-294's test)

## Description

`crates/bridge-fuse/tests/ipc_mount.rs` holds two real-kernel FUSE tests:
`fifo_and_socket_communication_is_local_to_each_clone` (A-26, AC-3.10) and
`a_device_node_reaches_the_mount_with_its_number_and_is_never_opened` (A-294). On Linux they failed together or one at a
time, each time at a different line, always on a file that should have been on the mount. Four GitHub Linux runs on
2026-11-18 failed four ways:

- main ecba1435a (before A-314): the device test at line 433 (`stat` of `null`: `ENOENT`); the IPC test passed.
- main 5071934e0: the IPC test at line 258 (`open` of `pipe`); the device test passed.
- main 8c9f05404: the device test at line 433 and the IPC test at line 272 (`unlink` of `socket`).
- 8c9f05404 merged with A-316: the device test at line 433 and the IPC test at line 307 (`connect` to the clone's
  `socket`: `ENOENT` where `ECONNREFUSED` was expected).

Run ids: 44266113327 (the first three), 44262784195 (the fourth).

## Root cause

Both tests take their directories from `MountPaths::new()`, which named them by the process id alone
(`$CARGO_TARGET_TMPDIR/slates-ipc-<pid>/{original,clone}`). libtest runs a file's tests at once, so both tests mounted
their volumes on the same `original` directory. Whichever mount was stacked last served both tests' paths: the device
test then looked for `null` in the IPC test's volume, or the IPC test looked for `pipe` in the device test's. Each
test's `Drop` also unmounts both directories and removes the shared root, so whichever test finished first tore the
other's mount down in the middle of its run. Which failure appeared depended on the order the two threads reached the
kernel. A-294 added the second test on 2026-11-17. Before that the file held one test, so the shared name was harmless.
`coherence_mount.rs` had already met this and gives each test a directory named by a tag (`scratch(tag)`); this file
did not.

## Impact

Only the test file. The bridge and the mounts were not at fault. While the tests shared the path, a Linux run of
`cargo test -p slates-bridge-fuse` failed intermittently. That could hide a real failure of either proof and redden any
lane that runs the file at once (`cargo test`'s default).

## Fix

`MountPaths::new(tag)` names the root `slates-ipc-<tag>-<pid>`: the IPC test passes `"ipc"` and the device test
`"devices"`. The tags are short because the IPC test binds a socket inside the mount, and that path must fit in
`sockaddr_un` (otherwise the test skips loudly, as before). Two agents found and fixed this on the same morning: g251's
fab67dbd9 landed first (from CI run 44260193629) and is the fix main keeps. g268's identical change (d36b9f961, tag
`"dev"`) gave way to it at the merge, and this record keeps g268's control experiment, which main's fix did not run.

Evidence: GitHub diagnostics run 44266547416 (Linux x86_64, 2026-11-18):

- Main 8c9f05404 with the tests at once failed 4 of 5 runs, at lines 298, 433, 258 and 258.
- The same binary one test at a time (`--test-threads=1`) failed 0 of 3 runs. This is the control: the old paths pass
  when the two tests never overlap.
- The fix with the tests at once failed 0 of 5 runs.

Siblings swept: `coherence_mount.rs` already uses a tag. `oci_container.rs` and `owner_turn.rs` hold one test each.
The server's FUSE tests (`fuse_mount.rs`, `fuse_told.rs`, `fuse_idle.rs`) name each daemon instance by its test.
