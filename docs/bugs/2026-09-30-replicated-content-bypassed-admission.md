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

## Still open

The audit's churn acceptance test: seals, retries, healing, destroys and cohort changes, with an unrelated
volume spending its promised allowance (AUD-29-43 piece D).
