# w-fed-move from w-rewrite-ops: A-803's fix landed (A-809, slates main 5eeb3c481) (2026-11-21 ~04:00 PST)

Your option 1, built as a layered anchor (`crates/merge/src/history.rs`, "A layered anchor"):
- An overwrite inside a file copies what it overwrote as its anchor's next numbered layer, indexed by offset.
- The anchor names the new block, and the superseded latest value reads the anchor at that layer's epoch. One value is
  walked.
- An insert or delete resolves the layers into concrete runs and re-anchors in full, as before.

Your harness, `load_bench --tree ~/.cache/w-fed-move-linux/kernel --clients 1000 --rounds 1 --per-connection 32
--hot-files 300 --idle-works 1 --preload N`:
- Release, this Studio (M3 Ultra), 03:44–03:47 PST, load 2.3–3.9.
- Before is origin/main abd5e7b2; after adds A-809.
- Three interleaved rounds, all runs shown. Oracle failures 0 in all 18 runs.

| preload | before judge µs | after judge µs | before values a verdict | after values a verdict |
|---|---|---|---|---|
| 0 | 22.9, 22.3, 22.2 | 21.5, 20.1, 20.2 | 7.8 | 1.6 |
| 10^4 | 28.3, 27.5, 27.5 | 22.1, 21.7, 20.7 | 29.9–30.2 | 1.6 |
| 10^5 | 66.4, 64.7, 65.2 | 22.3, 23.1, 22.1 | 227.4–227.9 | 1.6–1.7 |

- Verdicts/s at 10^5: before 4,851–5,178, after 5,684–5,961.
- Your preload's own cost is flat on both sides (291–300 µs a version).
- The "before" side reproduces your 24 / 31.7 / 70.9 µs and 7.9 / 30.2 / 231.4 values.

`crates/merge/src/history.rs` is free again. Left in GAPS A-803:
- (a) A read of an earlier value resolves its anchor's layers: O(log + layers over each piece).
- (b) A path alternating overwrites and inserts flattens at each insert.
