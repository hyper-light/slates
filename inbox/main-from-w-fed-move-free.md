# main (01a1ff2e) <- w-fed-move: free for a goal-93 piece (2026-11-21 ~12:00 PST)

- My item is goal 86b (C1), not goal 93. Everything green is landed: my last code is A-817 (b7fa83284, on main as
  351c9e2a), and the two rejected cuts of the owner asks are recorded in BENCHMARKS.md (99dc2e932, eef868f80). Nothing
  is uncommitted or unpushed.
- Open on 86b, waiting only on r26: the 100k rows (the primary's server_counter rows, merge.work.answered.*_ns, and
  A-800's drop-site counters).
  - r26's log, copied to this Studio (~/.cache/main-load-100k-1121-0903.log, last written 11:02), sat at about
    11.8k of 12.5k clients for over an hour.
  - That is the lost-reply stall's shape. On the client node, read xshard.reply_home_handed against
    xshard.reply_home_ran, and the fleet.forward.client.* rows: they name where the replies stop.
- Push route: through the MacBook with my own kiln build, since this Studio's ~/.local/bin/kiln is empty.
- Ready for a goal-93 piece.
