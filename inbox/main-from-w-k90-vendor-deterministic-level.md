# w-k90-vendor -> main: the deterministic zlib level is on main (2026-11-20 ~12:10 PST)

LANDED: `7c37b6fee` via `800818993`, tested on base `159f1c873`.

What changed:
- The zlib level and the delta window are now chosen from the bytes each saved plus the machine's rates, measured once
  per process (`slates_git::store::learn::Rates`).
- Those rates are nanoseconds per KiB for each level and for the delta search, measured over a seeded text corpus of two
  deflate windows. They are kept as `DaemonConfig::git_rates` and logged with the boot's derivations.
- `Learning` no longer has a clock.
- Each kind's levels are sampled once, on the first object of that kind a repository encodes. Before, every fragment
  sampled again, so each object was compressed several ways.

Tests:
- `a_versions_fragments_are_the_same_bytes_whatever_the_owners_clock_reads` encodes the same versions under a ticking
  clock and under one that jumps by random amounts. It was red on origin/main, where the commit's entry deflated
  differently, and is green now.
- A-745 rule 4's held-answer test now compares the answers byte for byte; it passed 3 of 3.
- slates-git: 570 passed. Server vcs, git_push and config: 352 passed.
- The shard's clock did not need to become replaceable: the bytes no longer depend on any clock.

Please mark complete in the UI: A-745's deterministic level / determinism gate step.

Owed:
- RUN REQUEST (yours, per CLAUDE.md §8): a release A/B on git/git, comparing CPU per encoded MiB and bytes out. A is
  main before `7c37b6fee`, B is `800818993`.
- Each node measures its own rates, so two machines can still pick different levels for the same data. One deployment's
  rates shared by its nodes is owed.
- A sibling of the SSH patience fix, not mine to fix unless routed: the HTTP daemon_edge long-history tests
  (`a_long_history_pushed_...`, `a_partial_clones_lazy_fetch_...`, `a_push_of_a_long_history_...`) failed in the full
  vcs run at load 54, each with a client read `WouldBlock` against their fixed 10 s `PATIENCE_NS`. They pass when run
  alone (8.16 s, load 44).

Next for me: item 3 (protocol v0's fetch in turns), then item 4 (bounded scattered plans).
