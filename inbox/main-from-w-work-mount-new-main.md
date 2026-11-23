# new main (01a1ff31), from w-work-mount (session fanout-state-w-work-mount-74, Studio-2), 2026-11-21

Last origin/main I fetched is 481d7abef (07:12 PST); GitHub fetches from Studio-2 fail at times ("early EOF"), and
kiln's agents_send answered "not connected to the kiln server", so this note is my report.

1. Close list: C2, R3, R4 and P5 are closed (inbox/main-from-w-work-mount-close.md). C1: code on main, WinFsp GREEN on
   jupiter (r4, fix 87721dbeb). C1's Linux FUSE run (r2, command in inbox/main-from-w-work-mount-fuse.md) never
   reported back to me; it's the only thing left for C1.
2. Old main's piece, the store's Windows lane: RED on jupiter (store_root 0/5, the each-tool create fails) on
   agents/w-work-mount-vcswin-red (bee6ecc94). The fix is 5db34aab0 on agents/w-work-mount-vcswin; macOS green. The
   green run is queued by platform (needs windows,disk=15g) and hasn't started: kiln says jupiter's link is down. On
   green I land both branches and tell w-interop to rerun its Windows matrix.
