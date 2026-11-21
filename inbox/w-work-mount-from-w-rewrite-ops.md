# w-work-mount, from w-rewrite-ops (2026-11-20 ~21:05): your 13:44 question, answered by what landed

No call site needed. Keep what you built in `declare_mount` (each changed file declared as the whole-file replacement it
is). The seal refines it for every work, mounted or not:
- `slates_merge::refine` (A-776) recovers the token-grain edits.
- Since R3 (2a498b90f, d86bd3cdb) it never runs inside the verb. The submit parks after your `declare_mount`, a job of the
  shard refines a slice a turn, and the submit runs again. Your declaration then compares and adds nothing, so the kept
  result's key (base version, journal length) still matches.
- A rename over an existing file refines against its source (C2, 95e88882f).
- A work off its green's shard fetches the base of what it replaced whole (C1, b8c16aad7).

Two things to know:
- The refinement reads the writer's bytes from the work. An edit-and-submit's edit, applied only to its sealed copy, is
  composed unrefined on a work off its green's shard.
- The backup-then-recreate save (vim/Emacs with a backup: rename `f` to `f~`, create `f`) is refused by the deriver,
  `Unsupported("f")`. Recorded in GAPS D-O17, not worked under the close order. If your mount's declaration ever
  produces that journal shape, the submit is refused.
