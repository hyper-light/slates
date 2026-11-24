# The takeover round's test asserted the slices of a set that held no root (2026-11-23)

## Description
`crates/bridge-fuse/tests/takeover_round.rs` failed 0 of 2 on Linux at main 80fbd5249 (run 45324224922). With a zero
slice budget, a restored mount's takeover round came out as 41 slices of one inode each, where both tests asserted
40: the first slice carrying the root and one file, then one file a slice. No main run reached it, since the ubuntu
lane stops at clippy. Main's bisect (run 45325199476) put it between ff16bed94 and 3c3efe4f0.

## Root cause
6f0804bbd ("a takeover names every inode of the head to a mount of the head", 2026-11-18) made the takeover set the
whole head, walked in inode order, and the root inode comes first in that order. The round tells the root as the
kernel's node 1 in its first slice and skips the root inode when the walk reaches it, so it is told once. Under a zero
budget a slice walks one item, and the first slice's one item is now the root: that slice tells the root alone, and the
round takes one slice more than before, 41 for the same 41 inodes. Before 6f0804bbd the set held only the mount's
restored references, which never named the root, so the first walked item was a file that rode with node 1.

The round meets the design's words: a slice takes at most the owner's budget and at least one inode, walked in inode
order, and the first carries the epoch bump and the root (`channel::takeover_told`, §4.6, A-158); every round takes "at
least one item each, so every round owed moves on at every turn" (A-562). The test asserted the old set's shape.

## Impact
None on mounts: every inode is still told once, and the cursor moves at every turn. The cost is one more round per
takeover when the budget lets a slice walk one item, as a zero budget does (41 slices for 40); at the owner's real
budget, half the shard's step quantum, the root's walk visit is one item of many. The Linux tests were red on main.

## Exact edits
- `crates/bridge-fuse/tests/takeover_round.rs`: the slices are asserted by the rule (`slices_hold_the_rule`): every
  slice tells at least one inode; the first tells the root and at most one inode the walk reached; each after it one,
  under a zero budget; and together they tell all 41, each once. The root is asserted first. The module doc states the
  rule and why the test counts no slices.

## Evidence
- Bisect (diag run 45326209301, ubuntu-latest): the test as each commit had it passed 2 of 2 at 6f0804bbd^ (f8ba01f61)
  and failed 0 of 2 at 6f0804bbd: slices [1, 1, ...] (41) against the asserted [2, 1, ...] (40), and 41 slices
  against 40 in the kernel-without-epoch test.
- The rule (the same run, on ac0d9514a): the rewritten test 2 of 2 in each of three rounds; red with the walk made to
  ignore the budget, both tests failing at the first slice's rule (`[41]`, the whole set in one slice); clippy of
  bridge-fuse's tests clean.
