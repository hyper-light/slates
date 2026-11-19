# live-durability handoff (2026-11-19)

The agent live-durability (01a1af4a) is stopping; its goals go to g227-transport-reports-durability, g240-taken-over-live
and g250-unresolved-object-recovered. Its scratch directory (`~/.cache/slates-agents-live-durability`: scripts, logs,
diag downloads, worktrees) was removed in a disk cleanup on the shared Mac; everything that matters is in the branches
and the `diag-results/live-durability-*` branches named below (`git show origin/<branch>:results.txt`, kept logs beside it).
Policy carried: failing test first (red commit, then fix), area-prefixed commits with measured facts, merge origin/main
(never rebase) before a gate, gates chained in a script with `cargo xtask check` first, docs and GAPS in the same change,
a rejected or dead-even A/B recorded in BENCHMARKS and not landed, CI watched with `git ls-remote`, never gh polling.

## g240 (taken-over live): A-271 and the takeover flows

- **On main already:** the recipient fix `agents/live-durability-recipient-1` 7cce51aa (B10: a replacement's pairs were
  wrapped to its destroyed predecessor's key; bug record docs/bugs/2026-11-09-a-replacements-pairs-were-wrapped-to-its-
  destroyed-predecessors-key.md, with the sweep table) and the flows' recovered-wait `agents/live-durability-seat-1`
  4771336a (B12: mode B was the flow's own wait; GAPS Fixed entry).
- **A-271 (mode A), held, ready to land:** `agents/live-durability-modea-1` at 76394ddb (A-271 + B9, gated: server lib 232,
  seal 3/3, cross-lint). It needs: merge origin/main (merge, not rebase); main's recipient fix conflicts with A-271's own
  copy in two doc hunks of `crates/server/src/fleet.rs` (take main's wording); re-gate (xtask, fmt, clippy -D warnings
  server/transport/db/cluster/cli, server lib, `--test seal`, server cross-lint x86_64 linux+windows).
  `agents/live-durability-modea-1-local` (9009f441) is that branch's local tip with its own copy of the recipient fix
  (red 6828aaf4, fix 9009f441): superseded by main's; kept only because diag-80 was built on it.
- **A-271's flow evidence, owed in its amendment text (the supervisor asked):** diag-live-durability-80 (tree 1e44a57e =
  modea-1 9009f441 + main db9c514e + seat-1 4771336a): 48 of 48 passed; the replacement served 13 of 24 on x86 (a 5, b 8)
  and 13 of 24 on arm (a 7, b 6); every survivor's takeover_waits_key, key_arrived, no_key_exhausted, refused.no_key,
  reclaimed, lost read 0 in all 48: the key always arrived, so A-271's bounded wait never had to act.
- Supporting: diag-79 (seat-1 on main): store-destroyed 12/12 per arch (replacement served 8/12 each), candidate-killed
  6/6 per arch. diag-72/-74 (A-271 before the recipient fix): 48/48 stalled. diag-75: stall gone, mode B. diag-77/-78:
  mode B on A-271's tree and on main alike (11/12 each).

## g250 (unresolved object recovered): A-272 (ii) and the cohort finding

- A-272 is on main (7fa36334 and its amendment). Open in GAPS: A-272 (ii), kept adoption records plus a current-members
  round for unresolved objects.
- **Open, model first (filed with seat-1, GAPS "an object whose line never passed through an unconfirmed predecessor reads
  lost"):** `RegionalConfiguration::recovery_cohorts` recovers every object of a host retired before confirming a
  predecessor's takeover jointly with that predecessor's cohort; diag-77 x86 run 1: old b destroyed, the owner killed
  before confirming old b's takeover, the owner's own volume (its cohort {owner, replacement, c}) joined with old b's
  {old b, owner, c}, only c left, 8 objects counted lost (kept, not served). The supervisor's ruling: model "the joint
  cohort applies only to objects whose line passes through the unconfirmed host" in §4.8's model text (read only; no
  TLC/JDK), with A-272 (ii)'s lineage model, before any change; a wrong narrowing loses data.
- Also Open: the regional configuration's encoding has no version or schema check (GAPS).

## g227: a resumed holder's catch-up (live shipping) -- two fixes that must land together

- **Cause 1, the stale retained tail** (diag-82, owner hooks): `Shipper::retain_up_to` was set only at a whole record or
  install image; a whole record comes only once the log reaches the volume's quota, so the tail stayed at the first whole
  record's size (215,997 bytes at head 16,526) and a holder 12 entries behind was installed whole (195-219 MB images on
  the owner's shard, 2-6 per catch-up). Fix: `agents/live-durability-retain-1` 5c981ef6 off main 29ef4521: red adb5e708
  (`the_retained_tail_follows_the_volume_as_it_is_now`), fix f7f486ea (`retain_bound`: the larger of the volume's
  referenced bytes and its last whole/image), GAPS Fixed 5c981ef6. Gated: xtask, fmt, clippy, server lib 237 (CI 245,
  diag-83), cross-lint (zig needs a private cache: `ZIG_GLOBAL_CACHE_DIR`, the shared ~/.cache/zig lost objects).
- **Cause 2, the reply fold's clone:** every reply of a lagging holder cloned every retained entry it lacked
  (`Shipper::on_reply` from `fold_into`): 1.4 ms a reply 1,000 behind, 7.1 ms 5,000 behind. Fix:
  `agents/live-durability-catchup-1` off 1c49c33f: red 3f131747 (crates/db/tests/live_fold_cost.rs, allocator calls
  counted: 21 at 16 behind, 1,035 at 1,024), fix 52374783 (`Shipper::fold_reply`, a private `Folded`). Gated: db lib 65,
  live 27, server lib 228, cross-lint. Lost with the worktree, uncommitted: the test's named bound
  (`RETAINED_BYTES = (LARGE + 2) * PAYLOAD` instead of `retain_up_to(usize::MAX)`, which the supervisor asked for).
- **retain-1 must NOT land alone.** diag-live-durability-84 (20,000 behind, a neighbour volume on the owner's shard
  written through the catch-up, both arches, 4 runs a leg, 24/24 passed):
  - main: catch-up 2.8-3.4 s x86 / 2.2-2.4 s arm, 1-5 installs, neighbour max 1.56-1.85 s / 1.16-1.47 s.
  - retain-1 alone: never caught up in 90 s; neighbour p50 200-280 ms x86 / 32-44 ms arm (the backlog now goes as
    entries, where each reply cloned O(backlog)).
  - retain-1 + the fold fix (tree c7a98c50 was a diag-only merge): 0 installs, catch-up 4.0-6.1 s / 9.4-13.3 s,
    neighbour max 0.26-0.40 s / 0.16-0.22 s, p99 1.6-3.3 / 3.9-5.3 ms, neighbour wrote 9-12k (main 0.7-1.6k).
  Next: merge catchup-1 into retain-1 (merge, not rebase), add back the test's named bound, re-gate, BENCHMARKS for
  diag-73/-76/-81/-82/-84, the GAPS catch-up Open line updated, send the combined tip.
- **Withdraw** `agents/live-durability-catchup-record` (8e22fd84): its "fold fix rejected" verdict is superseded by
  diag-84, and its "~100% of a CPU" reading is wrong (the tick window began before the resume and included a status
  call to the stopped holder; only the 90 s runs' ticks read cleanly, ~105% of one core).
- Earlier A/Bs: diag-76 (flow, main vs fold fix: not separating), diag-81 (neighbour, main vs fold fix, 5,000 and 20,000:
  not separating, because the stale tail sent most of the backlog as installs).
- The install watch (`agents/live-durability-watch-3`, 012769c6/2f9e79ee) stays held; its arm question (why arm's
  watch-off runs stalled) is likely answered by the catch-up pair above. BENCHMARKS "The install watch, measured".
