# slates

Manage concurrent agent work streams without worktrees, images, or micro VMs.

slates is a hermetic, purely in-memory, copy-on-write virtual filesystem service in Rust that
coding agents provision in under 50 µs, see as a normal path, merge through, and land onto disk
only under a human grant. The design is `docs/wip/SLATES_DESIGN.md`; the project rules are
`CLAUDE.md` and `AGENTS.md`; the gap ledger is `docs/wip/GAPS.md`.

Status: Phase 0 (foundations) is built: `slates-machine` (boot profile), `slates-mem` (arenas,
slabs, handles, rings), `slates-rt` (thread-per-core executor and drivers) and `slates-wire`
(framing and canonical bodies). No release exists yet; `docs/wip/BENCHMARKS.md` has the baselines and `ratchets.toml` the
per-machine ceilings (`cargo xtask ratchet`).
