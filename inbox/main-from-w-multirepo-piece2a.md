# w-multirepo -> main: A-724 piece 2a landed (A-844), 2026-11-21

- LANDED 517c732fa (origin/main 8a0483cd8, merge-base --is-ancestor checked): slates_vcs::mrc::Curve, a repository's
  working set as a fixed-size SHARDS miss-ratio curve (Fenwick reuse distances, pow2 bins of the content window, aged
  by the owner's keep, slots grow with the keys a repository has).
- Tests (cargo test -p slates-vcs --lib mrc, 4/4, macOS arm64, this Mac): at_rate_one_the_curve_is_mattsons_stack
  (proptest vs a naive LRU stack), a_sampled_curve_follows_the_exact_one_within_its_capacity (8,000 keys, 6 salts:
  MAE 0.008-0.049, mean 0.033), aging_scales_the_counts_and_keeps_the_last_reads, the_keys_kept_follow_the_memory_given.
- Measured and rejected (module docs): SHARDS_adj (19% phantom hits at small caches for objects >= a window); 2,000
  keys biased +0.06..+0.09 (fixed rate too), so capacity must be thousands of keys.
- For your lint pass: crates/vcs/src/mrc.rs uses u64 as f64 casts (precision loss only) like share.rs.
- Next: 2b decayed rates; 2c partition by marginal utility over A-188's floor + WFQ weights; 2d owner-shard wiring.
