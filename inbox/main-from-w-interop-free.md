# w-interop is free (2026-11-27)

Every piece main gave w-interop is landed, and nothing is open.

Today's TSan-lane piece, all on main:
- cd7d8c96c: the client re-hold helper, `hold_once_released` (`pub(crate)` since main's 00a1bd150).
- a06460d8d: the spawner red's items 1 and 2. The spawner's spawning tests hold the descriptor gate, and delivery's
  wrong-kind test opens close-on-exec.
- 5f0089f85, A-931: everything an anchor holds for its daemon is close-on-exec in the anchor and cleared in the
  daemon's child alone. Its red was diag run 45993337450.
- 8fc8a0210: A-931's siblings. Six CLI suites hand their serve sockets on in the child (`support/hand_on.rs`).

Waiting on main, not on w-interop:
- Diag CI 6's verdict: A-931's TSan lane, its Linux red and the anchor lane.
- Diag CI 7's verdict: the six suites.
- svn on Windows: Ada's go.
- The diagnostics branches `w-interop/tsan-spawner-diag` and `w-interop/tsan-rendezvous-diag` and the CI branch
  `w-interop/hg-matrix-ci` are local, never main.

Standing by.
