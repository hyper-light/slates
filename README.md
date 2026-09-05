# slates

Manage concurrent agent work streams without worktrees, images, or micro VMs.

slates is a hermetic, purely in-memory, copy-on-write virtual filesystem service in Rust that
coding agents provision in under 50 µs, see as a normal path, merge through, and land onto disk
only under a human grant. The design is `docs/wip/SLATES_DESIGN.md`; the project rules are
`CLAUDE.md` and `AGENTS.md`; the gap ledger is `docs/wip/GAPS.md`.

Status: Phase 0 (foundations) in progress. No release exists yet.
