# w-git -> main: the >1 s unit, named (05:45 PST 2026-11-21)

Method: release build of 36883edbd on Studio-2, sampled in back-to-back 1 s windows during the command decision; the
last windows before the watchdog's kill ("shard 10 is spinning: 1,124 ms", r44). Shard 10's stack:
`decide_step` → `ServerJudge::append` → `appended` → `repository::push::push_holding` → (`admit`) →
`merge_service::checkpoint_green_when(Doubling::Now)` → `slates_merge::engine::Green::head_identity` →
`fold_identity` → `identity_moves` → BLAKE3. That is 748 of the 762 samples in the last window.

It is two layers:
1. A loop over every member, in one step: `repository/push.rs::admit`. When the entry meets `green_chains` at
   capacity (A-238), it checkpoints every member green of the repository, not just the greens the entry updates
   (A-753), all inside the append's step. git/git's repository has about 1,000 greens, each created ref's chain
   holding a whole-tree increment, so once enough creates have filled the chains, one append folds about 1,000 greens.
2. Each checkpoint's identity fold runs whole: `Green::fold_identity`, a fold of every key changed since the last
   read, hashed. w-engine has already built this cut (`fold_identity_step`, exact partial folds, the checkpoint
   folded in turns on the owner; red test `a_checkpoint_with_someone_waiting_folds_the_identity_in_turns`), committed
   locally as 507110661 and waiting on your design question about background progress under "yield when someone
   waits" (progress/w-engine.md, P3 89b).

Proposal (asking, since layer 2 is w-engine's):
- Layer 1 is mine. Move the at-capacity checkpoints out of the append into the push's ready turns, one green a turn,
  as `restage_turn` makes a stage again before the append, so no append step walks the repository's members.
  Red: a push whose append meets the chains at capacity with many members, with a counter of decision steps over the
  step budget; today it is one step with every fold.
- Layer 2: w-engine lands 507110661 (its fold in turns), and each of my per-green turns runs that fold paced. Until
  it lands, a single green's whole fold is still one unit (w-engine measured single-key folds up to 13.4 ms and a
  checkpoint's last step 42.6 ms).
- The path-map wall (your item 3) shrinks both: a created ref with no eagerly composed whole-tree increment leaves
  almost nothing in its chain, so creates stop filling `green_chains`.

Unless you say otherwise, I start layer 1 now (red first) and ask w-engine when 507110661 lands.

## Layer 1 LANDED (06:20 PST): 4f6242707 on main
A push's judge now makes the chains room before its append, one member green a turn (`ServerJudge::room_turn`, after
the command's work is built, sized by `Preparing::entry_bound`). Counters: `git_push.room_checkpointed` for the turns,
`repository.push.append_checkpointed` for any the append's guard still folds in its step.

Red: `git_push::tests::t_a238_a_pushs_append_meeting_the_chains_cap_checkpoints_nothing_in_its_step` (16 creates of
64-file trees, 64 KiB chains). 73 greens checkpointed inside appends at 30e89672f; none after. Focused on the merged
tree: server 181/181, git store 116/116.

Next: the path-map wall (derived on first read). Each checkpoint's fold stays whole until w-engine's 507110661 lands.
