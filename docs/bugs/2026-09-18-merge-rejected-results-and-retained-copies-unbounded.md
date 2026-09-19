# The merge engine's rejected-result cache and its retained full-file copies were unbounded and uncharged (AUD-16)

Date: 2026-09-18. Contracts: §4.2 (bounded growth; all-cost admission, A-16: "retention charged by
the retaining operation"), §4.16 (the derived constant "delta retention before folding: measured
base lag × safety, capped by the delta memory budget"); AUD-16 in `docs/bugs/2026-09-14_AUDIT.md`;
GAP-A9-1/-14.

## Symptom

`Green::submit` inserted every unique increment's outcome into one `seen` map — accepted and
conflicting alike — so an idempotent retry was answered from it. An accepted outcome is bounded by
the chain (one per committed version, which the partition caps in bytes and recovery replays); a
**conflict is not in the chain**, so nothing bounded the map: distinct conflicting edits against one
old base grew it, with their windows, without limit, while the head and the chain charge stayed
fixed. Separately, every accepted content change kept a **full copy** of the superseded file in the
content history (`(version, bytes)` per path) for reconstructing older versions — the amplification
the engine's module doc acknowledged as an owed optimization — and nothing charged that resident
growth to the shard's budget: the service checked only the encoded increment against the chain's
byte cap. A small edit to a large file cost one increment's bytes on admission and one whole file in
memory.

## Root cause

Two resident structures fell outside the admission accounting: the rejected-result cache had no bound
of its own, and the retained history was neither charged nor folded when no reader could still name
the versions it reconstructed.

## Fix

Engine (`crates/merge/src/engine.rs`):

- The idempotency record splits into `accepted` (identity → version; bounded by the chain) and a
  **rejected-result cache** bounded in bytes: entries priced as what they hold (`window_bytes`:
  path, range, class, plus the identity), the oldest evicted first when a new one would exceed the
  budget, a result that cannot fit even an empty cache never retained, every eviction counted
  (`rejected_evicted`), one running byte total. A retry of an evicted conflict is judged again — the
  verdict is deterministic in the increment, its base and the head, so it is the same answer while
  the head stands and honestly the newer one once it moved. `set_rejected_budget` is the bound;
  `forget_rejected` drops one result when the shard's budget cannot cover it.
- **Retained history accounting and folding.** `history_bytes` runs with every superseded and
  removed value; `retained_bytes()` reports content, history and rejected bytes;
  `history_bytes_recounted()` is the oracle the running total is checked against.
  `fold_history_before(floor)` drops every history entry below the floor except the one in effect at
  it (all of them when an entry is recorded exactly at the floor), for the content and every
  namespace dimension, releasing the bytes and recording `folded_below`; lookups at or above the
  floor are unchanged, a version below it is no longer reconstructible.
  `fold_history_to_budget(reachable_floor, budget)` is what the service drives: **oldest-first and
  only as far as the retention budget needs** — the floor rises one version at a time while the
  retained history exceeds the budget, never past the oldest version a live reader still names. Under
  an ample budget nothing folds, so a reader may still re-pin any earlier version and a base-seeded
  green's origin (version 0) stays readable after a restart; under pressure the oldest unreachable
  history goes first. (An eager fold to the current readers' floor was tried first and rejected: it
  broke the design's backward `advance` and the origin read after a restart — the retention rule is
  a budget, not a liveness set.)

Service (`crates/server/src/merge_service.rs`, `verbs.rs`):

- The cache budget is derived, not chosen: the partition's green-chain byte cap
  (`rejected_cache_budget`) — the cache of refused verdicts may hold at most what the durable chain
  of accepted ones may. Set on the engine at creation and at rebuild.
- The fold never passes the oldest version a live reader can still name (`reachable_floor`): a
  work's base (it composes and rebases against it) or a pinned attachment (it reads at it), else the
  head. `advance` to a version below the fold floor is refused `UnknownBase` rather than served from
  the value in effect at the floor.
- The retention budget is the same derived cap as the cache's (one merge byte bound per green); a
  claim that would exceed it after folding is refused typed with the room left.
- `settle_green_retention` folds to the budget and trues the green's charge on the shard's budget
  (`ShardBudget::charge_retention` / `credit_retention`, the A-16 retention ledger) up to exactly
  `history + rejected`; `ShardState::green_retention` is the credit authority, so the accounting
  balances by construction. It runs after every submit, a clean rebase, an `advance`, a pin's
  removal, a work's destroy, and at rebuild (the replay rebuilds full histories; the recovered works
  are reset to the head and attachments reconciled out, so everything below the head folds and the
  remainder is re-taken ahead of new claims). A destroyed green credits everything it charged.
- `submit` **secures** the retention an accept can add — at most the sealed post-state the
  increment carries — before the verdict (refused typed `BudgetExceeded` when the budget cannot cover
  it; nothing changes), and the settle after the verdict credits the surplus. A conflict credits the
  secured bytes and charges the cache's growth in their place; a budget that cannot cover even that
  drops the result from the cache and counts it — the verdict stands.
- `Daemon::merge_retention(green)` reports content, history (running and recounted), rejected bytes,
  entries and evictions, the charge and the fold floor.

## Failing test first, and regression

- `crates/merge/tests/engine.rs::a_conflict_flood_reaches_the_rejected_result_bound_with_balanced_accounting`
  — 40 distinct conflicting increments against an old base under a 256-byte cache budget: the byte
  total never exceeds the budget, retained + evicted = 40 with evictions ≥ 1, the head does not move;
  an evicted result re-judged on retry reaches the same verdict, a retained one is served from the
  cache with the cache unchanged. Before the fix the map held all 40.
- `…::small_edits_to_a_large_file_are_charged_as_retained_history_and_fold_below_the_floor` — eight
  four-byte overwrites of a 64 KiB file retain exactly eight 64 KiB copies (the measured
  amplification: 512 KiB of history for 32 bytes of edits), running total = recount; under an ample
  budget nothing folds; under a budget of five copies with a reader at version 3 the two copies
  below the reader go and the floor then binds (six remain, balanced, versions 3 and the head
  reconstruct); with no reader and no budget everything goes (history 0), the current file
  untouched; idempotent.
- `crates/server/tests/daemon.rs::a_greens_retained_history_is_charged_folded_and_its_rejected_cache_is_bounded`
  — by use: a writer's six small edits to a 2 KiB file (the ring's bulk payload is 4 KiB), with a
  lagging work based on version 1, retain six copies (12 KiB) charged to the shard's budget with
  `charged == history + rejected` and running == recount; the lagging work's conflicting submit is
  cached and charged; destroying it leaves the copies (the daemon's budget is ample, so nothing
  folds and an earlier version can still be re-pinned), still charged and balanced.

## Validation (this box, 18 cores, 2026-09-18)

The merge crate's suites: 132 passed (engine 43, derive 41, increment 9, map 9, ops_doc 9, origin 7,
splice 5, verdict 9). The server's daemon suite 9 (13.96 s; the merge-service roles/pins/barrier
history and the base-seeded origin history among them — both broke under the first, eager fold and
pass under the budget-driven one), recovery 4, the client suite 4 (the green-chain and origin restart
histories). The in-process fleet suite, run alone: 45 passed in 293.60 s. Clippy `-D warnings` for
the merge, IPC and server crates clean; fmt clean; `cargo xtask check` ok.

## Siblings reviewed

- `deltas` (one ops map per version) grows with the chain, but each delta is a subset of its
  increment's operations, which the chain's byte cap already bounds — not a sibling.
- The holder replicas (`MergeShardState::replicas`) are engines too, rebuilt by recomputation; they
  take the same engine bounds (the rejected cache defaults to zero there — a holder judges no
  retries) and their histories fold under the same rule when the holder's readers are wired
  (GAP-A9-14, owed with mounted greens).
- The full copy per change remains the representation; the design's copy-on-write sharing of the
  chain is the measured optimization still owed — what this change guarantees is that the copies
  are bounded by what a reader can reach and charged for what they take.
