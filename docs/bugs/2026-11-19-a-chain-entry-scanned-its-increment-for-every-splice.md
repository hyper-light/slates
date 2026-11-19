# A chain entry scanned its whole increment for every splice (2026-11-19, found by A-621's reading; fixed by A-632; g267)

## Description

Encoding or decoding a version's chain entry cost the number of its effects times the number of its increment's ops.
The bytes were right; the cost grew with the square of an edit's size. A kernel-size push splices tens of thousands of
existing files from one increment, so it pays that square on the owner's commit and on every holder's replay.

Measured on the Mac Studio (kiln r786, red, 2026-11-19), with the index rebuilt for every effect: an entry splicing
1,000 files, each as its increment declared it, visited 1,000,000 of the increment's ops on encode and 1,000,000 on
decode. Fixed, the same entry visits 1,000 each way.

## Root cause

A splice whose ops are the ones its increment declared for that path (the common case, an increment based on the head)
is recorded by its path alone. `put_applied` decided that by comparing the splice's ops with `declared_ops(path)`, and
`take_applied` restored the ops on decode the same way. `declared_ops` filtered every op of the increment by its path's
name, once for each effect. A-586's `declared_copy` did the same for each copy's source version.

## Impact

O(effects x ops) path lookups and name compares on the owner's commit path (encode), on every holder's replay, and on
every recovery (decode). A version splicing 50,000 files of a 100,000-op increment is 5 x 10^9 of them. Nothing wrong
was written.

## Edits (A-632, `crates/merge/src/chain_entry.rs`)

- `Declared`, new: an index of what the increment declares, built in one pass over its ops. It holds each path's
  content ops in order (`ops_for`), and each copy's source version by destination, then source, the first one a pair
  declares (`copy`).
- `encode` and `decode` build it once, before their loops; `put_applied` and `take_applied` read it.
  `declared_ops` and `declared_copy` are gone.
- The entry's bytes are unchanged; it stays version 5.

## Tests

`chain_entry::tests::an_entry_finds_each_splices_declared_ops_in_one_pass`: an entry splicing 1,000 files, each as
declared, counting the ops the encode and the decode visit (a counter in test builds). Expect 1,000 each way, and the
entry to decode to what was recorded. Red on r786 with the index rebuilt for every effect: 1,000,000 each way. Green on
r786: `cargo test -p slates-merge --lib` 79 of 79.
