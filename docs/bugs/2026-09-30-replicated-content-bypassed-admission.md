# Replicated content bypassed byte admission

**Date:** 2026-09-30. **Area:** `slates-cluster` (content hold), `slates-mem` (budget, arena),
`slates-archive` (verification), `slates-server` (holder path, status). **Audit:** AUD-29-43 (the admission
half). **Design:** §4.2 atomic admission ("a remote holder makes the same admission against its own machine
before acknowledging placement"), §4.10 placement closure; `docs/wip/admission.md` §4f.

## Description

A candidate holder kept other owners' content in heap maps that no ledger charged. Any put that verified
was held and acknowledged, whatever the shard had promised to its own volumes, so a bounded volume did not
bound its holder.

Red test, `crates/server/tests/recovery.rs`:
`a_replica_is_admitted_only_from_the_holders_unpromised_capacity`. With every shard's admittable capacity
withheld (the memory-pressure hold), a put was still acknowledged: "no acknowledgement without unpromised
capacity" failed.

## Root cause

`ContentHold` stored `Chunk` payloads in a heap `ContentStore` and manifests as heap trees. It had no
reference to the shard's budget, arena or metadata ledger, and no refusal for capacity. Verification
decoded every shipped chunk into its own heap buffer.

## Fix

- **Arena storage.** The hold stores chunk payloads and manifest encodings as blocks in the shard's
  arena. The shard lends the arena, its byte budget and its metadata ledger for each operation
  (`HoldSpace`), so the shard stays the one capacity owner.
- **Charging.** Blocks are charged at their length (`ChunkArena::block_len` names the block before
  allocation) under `ShardBudget::charge_replicated`, from unpromised capacity only. The index is charged
  to the metadata ledger at a derived B-tree entry cost.
- **Whole or nothing.** Charges come before verification or storage. New encoded chunks are verified in
  one charged arena scratch block (`Archive::verify_into`, which decodes into a caller's buffer). A
  refusal at any step returns every charge and block (`ContentRefusal::NoCapacity`). Releases free
  exactly what their hold took.
- **Reporting.** `slates status` prints `replicated=` beside `retained=`.

## Tests

- The red test above passes.
- `a_put_past_the_unpromised_capacity_is_refused_whole` (cluster).
- `block_len_is_the_block_alloc_takes` (mem).
- `replicated_content_takes_only_unpromised_capacity_and_is_refused_whole_past_it` (mem).
- `a_chunk_verifies_into_scratch_as_its_content_does_and_refuses_the_same_faults` (archive).
- The ownership oracle checks two things over every generated history:
  - after every step, the budget and ledger charges equal the hold's account;
  - releasing everything returns them and the arena to zero.

## The audit's acceptance (2026-10-01)

`a_holders_replicas_cap_at_its_unpromised_capacity_through_churn_and_retire_to_the_survivors_baseline`, in
`crates/server/tests/fleet.rs`, runs in 3 s:

- **Setup.** Three daemons. One refuses content puts, so a single holder carries the churn. An unrelated
  bounded volume on that holder promises all its admittable capacity but room for two more seals.
- **Churn.** The owner seals four volumes with distinct bytes and destroys them while puts retry.
- **Measured.** One seal costs 32,768 bytes, and the cap is 98,304. The holder's charge peaked at exactly
  98,304 and always equalled its hold's own account.
- **Outcomes.**
  - Puts past the cap were refused `NoCapacity` and counted.
  - The unrelated volume wrote within its promise while the holder was full.
  - Destroying the first volume freed room that a waiting seal took.
  - Destroying everything returned charge, index and manifests to zero.

A first version capped the holder with `Daemon::inject_pressure_hold` and never bound: the peak was 81,920
against a 65,536 cap, with no refusal. `refresh_pressure_hold` re-samples host memory at the liveness cadence
and overwrites any hold set on the budget. The test now caps through a real promise, which is what the audit
names.

## Sibling reported

`refresh_pressure_hold` overwrites `Daemon::inject_pressure_hold` at the next liveness cadence on any host
that reports available memory. A test that relies on an injected hold for longer than one cadence is
timing-dependent, including
`nfs_mount::a_memory_pressure_hold_refuses_new_admission_but_not_an_admitted_volumes_writes`, which acts
immediately after injecting.
