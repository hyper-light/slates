# The buddy allocator accepted a forged free (2026-09-30, AUD-29-10)

Contract: §4.2 ("Conversion, rounding, counter arithmetic and generation exhaustion are checked and refuse
before mutation"). Reported by the 2026-09-29 audit, which ran it.

## Description

`Buddy::free` (and `ChunkArena::free` over it) accepted a block that no allocation had produced. With
4,096 bytes allocated from an 8,192-byte region, a free of offset 1 / length 4,095 was accepted. It
credited 4,095 free bytes (8,191 in all) and coalesced an 8,192-byte block while the real 4,096-byte
block was still in use. The next 8,192-byte allocation handed out memory someone owned.

Nothing refused:
- a stale copy of an extent after its place was reused (it freed the new owner's block);
- an extent handed to another arena of the same geometry.

## Root cause

1. **No validation.** `free` floor-divided the offset by the granule, rounded the length up to an order,
   and checked only that a head of that order was allocated there. It never checked that the offset lay
   on a granule and on its block's boundary, or that the length was exactly one block's. It credited
   the caller's length, not the block's.
2. **Forgeable extents.** `Block` and `Extent` had public fields, so any caller could construct one.
   Neither carried an allocation's identity, so a copy kept past its free named the next allocation there
   exactly.

## Impact

- **Silent corruption of the content plane's accounting and ownership.** A wrong free handed live memory
  to a second owner. In-tree callers free what they allocated, and no in-tree path was shown to forge.
  The API still made a forged or stale free indistinguishable from a real one.
- **Refusals mislabelled.** The refusals it did give read as `TooLarge`.

## Exact edits

- **`crates/mem/src/buddy.rs`**
  - `Block` fields are private: a `compile_fail,E0451` doctest proves outside construction fails.
  - Each block carries its allocation's incarnation; one `u64` per granule is allocated once with the
    region.
  - `free` validates, before anything changes, each of: the offset on a granule and on its size's
    boundary, the length exactly one block, a head allocated at that order, and the current incarnation.
  - A refusal is `MemError::ForeignExtent { reason }` naming `Misaligned`, `WrongLength`, `OutOfRange`,
    `NotAllocated` (duplicate or interior) or `Stale`.
  - An incarnation is checked and never wrapped: a spent head refuses `GenerationExhausted` (AUD-29-11)
    with the allocator intact.
  - The granule must be a power of two (a page size). Anything else is refused `BadCapacity`, and every
    offset conversion and alignment test is a shift or a mask.
  - The file is panic-free: no indexing, and checked arithmetic.
- **`crates/mem/src/arena.rs`.** `Extent` fields are private, with accessors. Each extent carries its
  arena's identity from a checked process counter; an arena whose identity space is spent refuses to
  allocate. `free` refuses `OtherArena` and `NoSuchRegion`.
- **`crates/mem/src/error.rs`.** New `MemError::ForeignExtent`, `ExtentRefusal` and
  `MemError::GenerationExhausted`.
- **Callers** (`crates/vfs/src/{content,volume}.rs`, mem tests and benches) use the accessors.

## Evidence

- **Red.** The audit's run as a unit test
  (`buddy::tests::a_forged_offset_and_length_are_refused_with_the_totals_unchanged`) on `5922adf` failed:
  "a forged block is refused".
- **Green.**
  - Every forged shape is refused by name with the totals unchanged.
  - A stale copy after reuse is refused, and the new owner's bytes stand.
  - A duplicate free is refused.
  - A cross-arena free is refused (`crates/mem/tests/extent_ownership.rs`).
  - A spent incarnation refuses rather than wraps.
  - mem, vfs, bridge-core, land and server daemon/recovery suites pass on macOS and Linux.
- **Cost** (instruction counts, iai-callgrind in a Linux container, the CI lane's own benches, each tree
  in its own target directory):
  - alloc+free of one page: 1,295 → 1,218 instructions (−5.9%);
  - 64 pages split and coalesced: 1,507 → 1,479 (−1.9%).
  - The shifts pay for the validation.
  - The bench had been counting the allocator's own destruction inside each alloc+free (763 of 2,077
    instructions), which read an added table as a +142 regression. It now borrows the allocator.

## Open

- A caller can still keep a copy of an extent (`Extent` is `Copy`, because copy-on-write bodies clone it).
  The incarnation and arena identity make every misuse of such a copy a typed refusal, not memory
  safety by type.
- Other handle representations' exhaustion (the slab's generation wrap, the runtime's 24-bit waker alias)
  is AUD-29-11, next.
