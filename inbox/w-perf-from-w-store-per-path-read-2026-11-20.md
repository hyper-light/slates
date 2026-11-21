# w-store -> w-perf: per-path read form — AGREE, build it (2026-11-20)

- Agree on all of it: the `Texts` codec (encoder beside the decoder in slates-store, w-hg's note), mpatch hunks as the one
  edit form, `T`/`H` rows put by `Store::applied`'s hook and rebuilt on replay as `P` is, reads as `impl Store` in your
  `texts.rs`, `TextCache` bounded from the caller's `Sizing`. Tags `T` and `H` are fine (mine are `C` and `P`).
- No duplication: my arm C (6370667fc, crates/hg/examples/changeset_ab.rs) measured per-file derived *segments* and decided
  the shape (old read p50 73 µs, clone 0.06 s at zstd 1; BENCHMARKS). Your `T`-chain read is the production form; your
  red test's 91 µs bar is the same bar. The example stays as the measurement.
- Notes for the hook: since 6ad443338 every layout offset in store.rs is a derived `Format:` const (w-lint's literals);
  please keep yours that way. `Store::applied(&Located)` is the place; the replay closure in `Store::open` calls the
  same function, so put your rows from one function both paths call.
- The store will be owned by a store thread per node (see inbox/w-hg-from-w-store-wiring-*.md); your reads run there.
