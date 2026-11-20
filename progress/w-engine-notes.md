# Notes for w-engine (S2: a green holds a changed file once), from w-perf's reading, 2026-11-20

Read at main de50bf0f5. File and line numbers are approximate.

## Where a file's bytes live in a green (crates/merge)
- Head files: `Green.content: Files` (`crates/merge/src/dims.rs`): a persistent radix tree (`Dim<Block>`) mapping path to
  `Block`, a slot in `Blocks` (`Vec<Option<(Vec<u8>, holders)>>`, refcounted, freed with the last holder). A green made
  at another's version FORKS the map (`Files::fork`: "the blocks stay with the arena") and the arena is exchanged
  between greens (`swap_arena`, `lend`). So the block arena's lifetime is the fork family's, not one green's.
- History: `Green.content_history: ContentHistory` (per-path `Vec<(version, Option<Value>)>`) plus
  `Green.chunks: history::Chunks` (`crates/merge/src/history.rs`), a per-green arena of immutable refcounted byte
  runs. A `Value` is pieces (chunk, at, len) plus its length (W2, copy-on-write versions).
- The write path, `Green::set_file` (~engine.rs:4576): `seed_content` (records the pre-change bytes whole if the path
  has no history yet), then `content_value(path, ops, bytes)` = `latest.spliced(chunks, ops, bytes)` (shares the
  previous value's runs and copies only the ADDED bytes into one new chunk; stored whole when the splice doesn't
  reproduce `next` or its pieces outweigh its bytes), then `content.insert(path, bytes)` (a NEW block holding the
  bytes), then `push_content`.
- So the duplicate is: every byte a change adds lives twice, in the head's block and in a history chunk. For a
  whole-file replacement (every git/hg push and svn import builds Delete+Insert of the whole file, A-618's
  `merge::push::build`) that's the whole file twice. For an in-place edit it's only the edit's bytes.
- Reads: `Green::content(path) -> Option<&[u8]>` (Files, contiguous), used widely (VFS reads, servers);
  `content_ref_at(path, version)` uses the head (`ContentRead::Head`) while the file is unchanged since `version`, else
  materializes the history `Value`. `content_read` covers copies (A-586) and the one history rule (A-534).
- Folding: checkpoints (`checkpoint.rs`, `Green::checkpoint`/`restore`) drop history below the floor. A checkpoint is
  a durable format with versions and presence bytes (copies section). Restored greens have no history below F.

## Measured cost (BENCHMARKS.md, Mac Studio)
- svn import, 1,000 revisions (223 MiB dump): `Green::commit` live heap 234.8 MiB (history chunks); head blocks in the
  content regions ~585 MB. Slope ~235 KB a revision.
- git push of git/git v1.0.0: `Value::whole < Green::commit` 2.1 MiB (only the head tree's files go through the green).

## Shapes considered by w-perf (not decided)
1. One arena for both: the chunk a whole-file change makes IS the file's block (Files names a chunk handle). It must
   cross the fork/lend arena lifetime (Blocks follow the fork family, Chunks follow one green), so the chunk arena would
   have to move to the family level, with the history's refcounts in the same arena.
2. Keep two arenas, but when a change's new Value is a single piece covering the whole file, let the history name
   the BLOCK (a new piece kind referencing a block, refcounted in Blocks). Spliced, multi-piece values stay chunks.
   Smaller change; whole-file replacements, the servers' case, go to one copy.
3. The head is a Value (pieces), and `content()` materializes or borrows when single-piece. That breaks the
   contiguous `&[u8]` read API used across crates. Rejected for the read path's cost.
- `crates/merge/src/held.rs` (A-716 slice 1a, on main 5efce021a) is the green's identity -> block index. It must keep
  working: a held block outlives its file. In option 2, a history piece that names a block is one more holder,
  exactly as held.rs holds one.

## Tests that guard this
merge `--lib engine::` (copy_tests, restate_tests, node_tests, held_tests), `--test engine`, `--test checkpoint`,
`--test chain_replay`, and the model/proptest suites. `history.rs`'s own tests hold `spliced` exact.
