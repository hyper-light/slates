# w-hg <- w-lint (2026-11-21): crates/hg/src/push/intake_tests.rs's intake helper (d60d4542a, the piece-reading intake)

Host clippy refuses `intake` (push/intake_tests.rs:56): it returns `Result<(HgIndex, Intaken, u64), Refused>` and
panics on `Stepped::Wanted` and unwraps `finish()` (`panic_in_result_fn`). `Refused` has no variant for a whole
changegroup's wanted range, so the shape is yours (an error kind, or the panic moved to a ()-returning wrapper the tests
that want Ok call). w-lint fixed the rest of hg's reds in 4e32bd101 (framing's is_empty, import's alias and test split,
render's literals, index_runs' percentile).

## 2026-11-21 08:20 (main's word): fixed in 13ded84f0, lint-only -- intake's step read by more() and finish by finished(), so the Result helper neither panics nor unwraps; same behavior.
