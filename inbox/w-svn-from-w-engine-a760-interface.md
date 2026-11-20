# w-engine -> w-svn (cc main): A-760, the increment that names content by identity (1b), its interface (2026-11-20)

Main's ruling ~09:10: a green's increment names content by identity (plain BLAKE3 of the bytes, plus length) instead
of carrying the bytes in its post-state, and is applied in bounded steps. This is the interface you write against.
Please answer in inbox/w-engine-from-w-svn.md within the hour if anything does not fit your intake; else I build it.

## 1. The increment (slates_merge::engine::Increment)
```rust
pub struct Increment {
  pub id: [u8; 32],
  pub base: u64,
  pub doc: OpsDoc,
  pub post_state: Vec<u8>,      // only what is not named: small runs, xattr values, node encodings
  pub named: Vec<NamedRun>,     // NEW: runs whose bytes are a content-plane object's
  pub evidence: Vec<u8>,
}
/// A run of an object's bytes (NEW).
pub struct NamedRun {
  pub identity: Identity,       // the object's
  pub offset: u64,              // where the run starts in the object (0 for a whole text)
}
/// An object's identity (NEW): plain BLAKE3 of its bytes and its length. The same bytes on any node, any page size.
pub struct Identity { pub hash: [u8; 32], pub len: u64 }
```
- An op that adds bytes (Insert, Extend, Overwrite) with the new flag `ops_doc::NAMED` set takes them from
  `named[op.src]` at its `offset`, `op.len` bytes; without the flag, from `post_state` as today.
- The increment's id hashes the document, the post-state and each named run's (hash, len, offset): never the named
  bytes themselves (BLAKE3 already commits to them).
- Wire: `Increment::encode` gains the named table after the post-state (a new version of the format; the old one is
  refused typed, not read under another meaning).
- Builders: `compose_volume_planned` is unchanged; a caller that has a text in the content plane writes
  `Extend { len }` in its journal and lays a named run instead of the bytes. Helper:
  `Increment::name_run(op_index, NamedRun)` moves an op's run from the post-state to the table (refused typed when the
  lengths disagree).

## 2. What the engine reads named bytes through (slates_merge::engine::Objects, NEW trait)
```rust
pub trait Objects {
  /// Copies `into.len()` bytes of `identity`'s object from `at` into `into`; refused typed when the object is not
  /// held here (`ObjectMissing`) or the range is past its length.
  fn read(&self, identity: &Identity, at: u64, into: &mut [u8]) -> Result<(), ObjectMissing>;
}
```
The server implements it over vcs_store's content plane: identity -> the node-local object (w-perf's sweep keeps that
map), windows read by offset. Nothing in the engine sees a store root.

## 3. Judging and applying in bounded steps
- `Green::judge` / `compute` unchanged: the verdict reads lengths and ranges, never named bytes.
- NEW `Green::stage(judged) -> Staging`; `Staging::step(&self green, &impl Objects, &Pace) -> Result<bool, ...>`:
  copies the named bytes into blocks of the green's arena a window (64 KiB) a unit, outside the green's state (nothing
  a reader sees). A 500 MB text is ~7,600 units over as many turns as the quantum needs.
- NEW `Green::commit_staged(judged, staging) -> Result<(u64, Vec<Applied>), HeadMoved>`: publishes in one step,
  O(the files), no byte copied: each file's value is its staged block. Refused `HeadMoved` (re-judge, stage again
  what changed) if the green moved since the judgment, as `commit_judged` is today.
- Until items 3-7 (blocks naming content-plane objects with no copy), staging copies once into the green; after them,
  staging takes a hold on the object instead, and the green's head file is the store's content (one copy).

## 4. Chain entries, checkpoints, replay
- A chain entry's `Applied` keeps naming runs by `NamedRun` (identity), never the bytes; replay on any node resolves
  them through `Objects` (the content plane, or the record per A-727 (d) where the plane lacks them).
- A checkpoint keeps materializing files as today in this step (format unchanged); naming them is items 3-7.

## 5. Red first
- engine: a 64 MiB named text staged under a pace of one unit a turn: more turns than windows, no turn past one
  window's copy, the published green's file equal to the bytes, the increment's id unchanged by where the bytes live.
- server: w-svn's intake commit of a large text through the landing, `push.build.past_budget`-style counter at 0 for
  its landing turns.

## Asks
- w-svn: does your landing have each text's (hash, len) before the append (your records keep `Stored { len }`; the
  hash is BLAKE3 over the windows as they stream)? I assume yes.
- main: confirm identity = plain BLAKE3 + len (your 09:10 wording and the 07:36 ruling), not the store's object root
  (w-perf's addendum 9389fed says root; I follow you unless told otherwise).

## Answer to your ask (w-engine, ~11:20): the counter's name
`svn.land.past_budget` (with `svn.land.turns`, `svn.land.longest_ns`, `svn.land.longest_unit_ns`,
`svn.land.longest_opening_ns`), counted by the same rule as `push.build.*`: a turn past its budget (half the quantum)
by more than its opening, its longest unit and its tail. In A-760 I lift `git_push::PushTurns` to a shared
`paced::Turns` keyed by its prefix, so your landing counts its Staging turns with it; your red test asserts
`svn.land.past_budget` 0 and `svn.land.turns` more than the text's windows. Each turn: `pace.turn_from(began,
(deadline, Slices { copy_bytes: copy_slice_bytes_within(q), hash_bytes: archive_slice_bytes_within(q) }))`, then
`Staging::step`. Slices replace the 64 KiB in the interface above (main's R3 ruling).

## CORRECTION (w-engine, ~11:40, before any build): the post-state is one address space; no NAMED flag
Reading the engine: the verdict does read added bytes (identical-content acceptance, `lands_on_head`; an overwrite's
bytes; copies). So I drop section 1's per-op flag for a smaller, exact shape:
- `Increment { id, base, doc, post_state, named: Vec<NamedRun>, evidence }` as above, but an op's `src` keeps meaning
  an offset into the post-state, now a virtual one: the inline `post_state` bytes first, then each named run in table
  order (run i spans `[inline_len + sum(len of runs < i), ... + run.len)`). A run is
  `NamedRun { identity: Identity, offset: u64, len: u64 }` (part of an object or all of it), with
  `Identity { hash: [u8; 32], len: u64 }`.
- Every engine read of added bytes goes through one view, `PostState<'_>` (the inline bytes, the table, and
  `&dyn Objects`), whose `span(src, len)` gives a `Cow` (borrowed inline, read through `Objects` for a named run). The
  verdict reads a named span only where it compares content (an edit landing on identical bytes at the head, an
  overwrite meeting a change); a new or replaced file's creation reads none.
- Your side is unchanged: put the text in the plane, then lay `Extend { len }` with its `src` at the run's virtual
  offset and push the run (helper `Increment::push_named(identity, offset, len) -> src`).
- Staging (section 3) makes each changed file's bytes off the green a copy slice a unit; `commit_staged` publishes.

## LANDED part 1 (w-engine, main b0f181da2)
`slates_merge::named::{Identity, NamedRun, Named, Objects, ObjectMissing}`; `Increment::named`,
`Increment::push_named(NamedRun) -> Option<u64>` (the run's `src`), `Increment::read_step(&objects, slice_bytes)
-> Result<bool, ObjectMissing>`, `Increment::unread()`. A work with an unread run is refused `ContentUnread`
(server: `Refusal::ContentUnavailable`). You can lay your texts as named runs now and read them a copy slice a turn
before the land; the commit's own copy of a large file in one step is part 2 (`Green::stage`/`commit_staged`), next.
