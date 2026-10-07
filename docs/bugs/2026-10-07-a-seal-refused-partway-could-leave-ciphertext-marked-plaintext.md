# A seal refused partway could leave ciphertext marked plaintext

**Found:** 2026-10-07, in a sweep of discarded results (`let _ =`) after the truncate fix of 2026-10-06.

## Description

`ChunkStore::seal_segments` encrypted a chunk in place, segment by segment (A-99). If the cipher refused a segment,
it opened the already-sealed prefix back and returned "not sealed", leaving the chunk in the clear and counting the
refusal. It discarded each open's own result (`let _ = cipher.open(...)`).

If an open back was refused, that segment stayed ciphertext under a chunk with no seal. Reads then returned the
cipher's bytes as file content: silent corruption of acknowledged data.

The test reproduces it with a cipher that refuses the third segment's seal and every open. Before the fix, the read
returned the first two segments as ciphertext.

The same sweep found three frees on the seal's paths whose refusals were discarded:

- the deferred free of a block moved out of a recovery image;
- the free of a copy that could not be filled;
- the tag run of a refused seal.

Each refusal leaves bytes held, possibly plaintext, with no trace.

## Root cause

The seal mutated the only copy of the bytes before it knew the whole seal would succeed, and its rollback could
itself fail without anyone hearing.

## Fix

- **Copy aside, then seal in place.** The plaintext is first copied into a scratch buffer the store keeps (one chunk
  at most). On any refusal it is copied back, so a refused seal changes nothing whatever the cipher does. The scratch
  is zeroed after every seal, so no plaintext copy outlives it (the rule of
  `2026-10-06-sealed-content-left-its-plaintext-in-ram.md`).
- **Counted frees.** A refused free on a seal's path is counted (`ChunkStore::free_refusals`) and surfaced in status
  as `content.free_refused`.

## Impact

Seal cost per 64 KiB chunk under AES-256-GCM (`cargo run --release -p slates-vfs --example sealed_read_bench`,
which now times its seals), back to back at load average 14–17 on this Mac (M5 Max), best of 3 per round:

| Version | Round 1 | Round 2 |
|---|---|---|
| Before | 7.83 µs | 7.70 µs |
| After | 9.24 µs | 9.26 µs |

One copy plus one zeroing per seal: about 20% more, once per chunk, in the idle sweep. Measured and rejected first:
sealing a copy and copying the ciphertext back. That takes two copies plus the zeroing, and measured 17.7 µs and up
under a busier machine.

## Tests

`content::tests::a_seal_refused_partway_leaves_the_chunk_plain_even_when_opening_back_would_fail` failed before the
fix: the read returned ciphertext.

## Siblings

The rest of the sweep (rollbacks, recovery give-backs, drift re-checks, a reclaim's descriptor) is fixed and recorded
in `2026-10-07-a-reclaimed-base-file-leaked-its-descriptor.md`.
