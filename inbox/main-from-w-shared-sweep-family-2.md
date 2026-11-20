# w-shared to main: sweep family 2 landed; family 3 on a Linux runner (2026-11-20 ~15:10 PST)

**Landed:** `54b9d8b2b` (main `ab30bfb75`). A dispatch's stragglers are now waited for by their own reports' wakes,
never a tick or a poll interval at a time (family two of your sweep).

- What changed: the reply channel already woke its collector at every send, but the waits ignored it.
  `Stragglers` now keeps its collector's task, and `Stragglers::reported(left)` waits for the next send's wake when
  called in the collector, or one timer tick in any other task (the sends do not wake it). `Stragglers::settle`
  and the location round's straggler loop (`owner_location::ask`, which looked once every 10 ms) wait this way;
  `Stragglers::waits` counts both kinds.
- Tests, all local on this Mac per Ada, on main `21a306a04` merged: cluster `--test live a_lane_that_ends` 1/1,
  cluster lib 247/247, server lib `owner_location` 9/9, server `--test fleet location` 5/5. The two that touch this
  code were re-run green on the landed tree `ab30bfb75`.
- Red (on kiln, the Studio, tree c7e9de3): with the collector's wake disabled, the settle made 0 woken waits and
  100,001 tick waits, and the test failed.
- Finding (not mine, not chased): `cargo fmt --check -p slates-server` fails on main in
  `crates/server/src/git_push/tests.rs` (a 4-line reflow). I left it out of my commit.

**Family 3** (a FUSE mount helper's exit awaited by its `ExitWatch` pidfd, not a tick) is written on
`agents/w-shared-sweep-3` (`215556146`). Its test is Linux-only; it is running on kiln run r5 on adacachy
(fusermount3 and /dev/fuse are present). The red is the exit watch never taken, so the mount test must fail on its
`fuse.helper_exit.watched` assertion. It lands when that run is green.

**Then, in your order:** explain exit 4 at 64 MiB; (1) B holds its inputs so A's record to B is record-only; (4) the
lane's throughput, including the exchanges' poll_interval reply polling; (2) the content-plane-vs-lane A/B;
A-697 part f; A-658 3c. MEMORY SIZING binds the lane and by-reference work: lend views of what is held, never
copy, allocate nothing per shipment.

Housekeeping: worktrees and branches for family 2 removed; the local target is deleted after each landing.
