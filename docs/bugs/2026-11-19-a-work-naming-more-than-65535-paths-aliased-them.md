# A work naming more than 65,535 paths aliased them (2026-11-19, found by g225 on the push path; fixed by A-621; g267)

## Description

An increment's ops document names each operation's path by an index into its path table. The index was 16 bits.
`PathTable::intern` answered `u16::try_from(i).unwrap_or(u16::MAX)` for a new name's position, so every name past the
65,535th took the index `u16::MAX`, the 65,536th name's. All of them aliased one path, and their edits would land on
that file.

g225 met it pushing a kernel tree (about 97,000 files) as one work (A-618) and refused such works typed (`TooManyNames`)
until the fix.

The same narrowing stood in four other places:
- the deriver's `index_of` (`crates/merge/src/increment.rs`), the same saturation;
- the verdict's reading of a rename's source and of an attribute's name (`Green::resolve`), the same saturation;
- the merge plane's reservations (`changes_reservation`, `history_reservation`), which dropped an index past 16 bits;
- the server's laying-in of an attribute's value (`assemble_post_state`), which skipped a name past 16 bits, leaving
  that attribute's value zero.

Interning also scanned every name it held (`position`), so a table of n names cost O(n²) comparisons. That was about
4.7 × 10⁹ for a kernel push, made again by every decode of the document.

## Root cause

The format's path index was sized for a typical work's names, and every conversion into it saturated rather than
refusing. The saturated value was itself a valid index, so the defect was silent.

## Impact

Before g225's interim refusal, no work of more than 65,535 paths could be merged correctly. Its later names' operations
were judged and spliced on one path. No such work had reached the merge plane: the push path refused it first.

## Edits (A-621)

- `crates/merge/src/ops_doc.rs`:
  - `Op::path` is `u64`;
  - `PathTable` interns through an ordered map (`BTreeMap`), which `canonicalize` reads the canonical sort off in one
    pass;
  - the document's version is 2: path and op counts `u64`, the op record 34 bytes (kind, flags, path, at, len, src);
  - decode refuses a table naming a path twice (`DuplicatePath`) and a version 1 document (`BadVersion`).
- `crates/merge/src/chain_entry.rs`: version 5, an op's record the document's (`ops_doc::OP_BYTES`), its path read as
  `u64`.
- `crates/merge/src/increment.rs` and `crates/merge/src/engine.rs`: the index conversions widen. A name not found is
  `u64::MAX`, which names no path, so it never aliases another as `u16::MAX` did.
- `crates/server/src/verbs.rs`: an attribute's name index read whole.

## Tests

`crates/merge/src/ops_doc/tests.rs` (moved from `tests/ops_doc.rs` so the crate's own run holds them) and
`crates/merge/src/chain_entry.rs`:
- 65,537 distinct names, each its own;
- the comparison counter;
- the determinism gate at that size;
- version 2's golden vectors;
- `DuplicatePath` and `BadVersion`;
- the chain entry past 16 bits.

Green on the Mac Studio (kiln r692): `cargo test -p slates-merge --lib`, 60 of 60; `cargo test -p slates-server --lib --
green merge_origin`, 31 of 31; fmt clean.

Red with the old scan and the 16-bit index restored inside the new types (r705): all four new tests failed.
- The 65,537th name took index 65,535.
- Doubling the names took 2,149,245,934 comparisons against 537,633,163, four times.
- The determinism gate's two orders differed.
- The chain entry clipped the index.
