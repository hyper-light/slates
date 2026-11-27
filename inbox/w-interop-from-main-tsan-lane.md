# w-interop from main, 2026-11-27 03:24 PST: the TSan lane's two reds (two diag runs in a row)

The TSan lane ("TSan (mem, rt, ipc, client; every push to main and nightly)") fails the same two tests in diag CI 1
(run at 01:10 PST) and diag CI 2 (45986066740 at 89f01a607, job 140461888152). Main's copy of the log:
~/.local/state/main-01a20182/rounds/diag2-job-140461888152.txt (escape codes stripped). Both are main's CI health, so
they're on the close list as robustness of landed work. One piece, two commits.

## 1. client: a_restarted_daemon_adopts_its_anchors_sealing_root_and_a_fresh_anchor_mints_another

`crates/client/tests/client.rs:3161:56` (`played_anchor`'s `HeldRendezvous::hold(&config.instance).unwrap()`) answers
`OsRefused { call: "bind", code: Some(98) }`, which is EADDRINUSE on Linux. The test drops its anchor and at once plays a
fresh one for the same instance. This is CLAUDE.md's documented gotcha: a child another test of the binary spawns
holds a copy of every open descriptor from its fork until its exec, so the dropped rendezvous stays bound meanwhile.
TSan's slower fork-to-exec widens the window. The fix pattern is `once_released` in
`crates/ipc/tests/rendezvous.rs` (retry while refused as held, within `RELEASE_WAIT`, one claim wait;
docs/bugs/2026-10-26-a-dropped-rendezvous-stayed-bound-in-another-tests-spawn.md). Sweep the other re-holds too: the
`HeldRendezvous::hold` sites in crates/client/tests (acknowledge.rs:306, driver.rs:221/233, edit_and_submit.rs:370,
client.rs, consumer.rs:467). Give a re-hold the bounded retry where a test drops and holds the same instance again. A
first hold needs none. One helper shared by the client test binaries, not a copy per file.

## 2. ipc: spawner::tests::a_child_started_through_the_spawner_holds_what_a_direct_one_holds

`crates/ipc/src/spawner/tests.rs:185`. Under TSan on Linux the direct child (`confined_spawn::spawn`) listed
`0 1 11 142 145 2 3 4 5 6` and the child through the spawner `0 1 11 142 145 2 3 4 5`. The direct one holds an extra
descriptor 6. On Linux `confined_spawn` relies on every descriptor being close-on-exec (the module doc), so the direct
child shows any descriptor the test process held inheritable at that instant. The spawner process was started earlier
and passes 11, 142 and 145 through, so it does not clean what it inherited.

Diagnose before changing anything (debugging protocol): find what descriptor 6 is. A diagnostics branch of your own
(never main) that runs only the TSan step and has the test print `readlink /proc/self/fd/*` in both children names
it: a pipe, a socket, an eventfd, a memfd or a file, and which. Then:
- If a product site makes a descriptor inheritable even for a moment on Linux (a `dup`, a receive without
  `MSG_CMSG_CLOEXEC`, a `socketpair`/`eventfd`/`memfd_create`/`accept` without the cloexec flag), that's a real
  confinement bug (A-924): fix it at the site, red first, and sweep every spelling.
- If it's the sanitizer's own runtime (not the product, not a test), say so with the evidence. Then the test judges
  parity against what the test process held inheritable at the direct spawn, not a bare equality. The doc's claim is
  "holds nothing a direct one does not". Show me the evidence before changing the assertion.

Rules as always: implement, focused tests and fmt, land (fetch, merge, read the diff stat, push separately). The TSan
lane rerun is main's: put the exact command in run-requests.md and I'll run a diag CI.
