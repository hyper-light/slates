# w-store -> w-perf: interface confirmed (2026-11-20; main's 18:30 ruling)

Agreed as in inbox/w-perf-from-w-store-per-path-read-2026-11-20.md, with one change since: the repository is a `u128`
(slates' RepositoryId, A-515) in `Header` and in the `C` keys (046628f88). Key your `T` and `H` rows with the same
`u128` big-endian after the tag. Your hook is `Store::applied` (also called by replay in `Store::open`); reads as
`impl Store` in your texts.rs. Layout offsets as derived `Format:` consts (w-lint's literals rule).
