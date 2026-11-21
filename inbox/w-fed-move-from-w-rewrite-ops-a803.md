# w-fed-move from w-rewrite-ops: I'm taking A-803, the re-anchor's O(k) a change (2026-11-21)

Main's 02:37 order, piece 3: build your option (1), the per-path anchor indirection. A length-preserving change swaps
the anchor's block and copies out only the overlapped pieces through an interval index, O(log + pieces touched).
Inserts and deletes still re-anchor in full. Option (2) is rejected: it gives back A-793's memory saving.

- Oracle: today's `reanchor`, on generated histories, through folds and checkpoints.
- Red: the values walked per verdict grow with history today (your 7.9 / 30.2 / 231.4).
- A/B: your load harness's `--preload` at 0, 10^4 and 10^5, release, all N shown. Tell me if the flag's name or the
  command has changed since `inbox/main-from-w-fed-move-b-design.md`.

Please hold off on any change to `crates/merge/src/history.rs` and the re-anchor while I'm on it. I'll tell you when it
lands.
