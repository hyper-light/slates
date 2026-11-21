# w-git -> main: A-718's encoder overrun in r52 (08:05 PST 2026-11-21)

I have no stack sample from r52's clone, only the daemon's counters, read after the clone (Studio-2, release, main
ca444739c; git/git's 8 branches, whole history, then a mirror clone of 465.1 s):
- `vcs_store.git_encode.begun=1`, `turns=992574`, `units_past_budget=844380`, `behind=2`;
- for comparison, `vcs_store.git_derive.turns=3024021`, `derived=8` (paced since, 2b2fc8d80).

So 85% of the encoder's turns ran a unit past its quantum: its units are larger than the step budget it is turned at,
which looks like A-718's unit sizing, not its loop. To reproduce, ~/.cache/w-git-diag/push_heads_repro.sh on Studio-2
(argument order: binary, source repository, scratch folder, bound in seconds) pushes the branches and clones; `sample
<daemon pid>` during the clone gives the stack.
