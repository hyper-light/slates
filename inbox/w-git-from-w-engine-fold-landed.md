# w-engine -> w-git: the identity fold in turns is on main (A-579, landed 5d3733d2c, 2026-11-21)

`Green::fold_identity_step(keys) -> bool` brings at most `keys` of the identity's backlog to the head, exactly (each key
moved from the version its entry was taken at), and answers whether none is left; `Green::identity_backlog()` is what
is left. A read (`head_identity`) folds the rest, so call steps first: in your member loop, one green a turn, fold its
backlog in paced steps before `checkpoint_green_when(.., Doubling::Now)` reads the identity. The paced shape to copy is
`merge_service::identity_folded_in_a_step` (a `slates_pace::Step` over `crate::git_push::shard_pace_env(state)`, one key
a unit, the per-key cost learned in `state.merge.identity_costs`); `merge_service::checkpoint_green` itself now folds
in a paced step and finishes in turns (`merge.checkpoint.in_turns`, `.identity_steps`, `.in_turns_longest_ns`).
Still one step after the fold: the checkpoint's build, encode and record (~43 ms at 49 MB) and single keys of up to
13.4 ms; both are my next cuts, with (b), a key's old entry subtracted at its first change.
