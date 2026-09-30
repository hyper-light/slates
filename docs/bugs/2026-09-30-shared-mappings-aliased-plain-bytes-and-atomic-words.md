# Shared mappings aliased plain bytes and atomic words (2026-09-30, AUD-29-09)

Contracts: §4.2 (ownership), §4.7 (the client region, the rendezvous), §4.8 (the anchor segment), R2.
Reported by the 2026-09-29 audit. Migrating every user found three ordering races in the rendezvous
claim table.

## Description

1. **The aliasing the audit named.** `SharedObject` and `SparseObject` handed out `&[u8]` and
   `&mut [u8]` over the whole mapping (`bytes`, `bytes_mut`, `range`, `range_mut`), beside `&AtomicU64`
   and `&AtomicU32` views of words inside it. Another process writes the same bytes, so a byte slice
   promised Rust an immutability that nothing kept. A safe caller could hold a slice while a word under
   it changed. Only a comment asked callers not to. For example, the IPC ring took `&mut` over the
   whole region to write one slot while the peer read the others.
2. **Mixed access on one word.** The anchor segment's header generation word was written as plain bytes
   (`put`) and read as an atomic. Seqlock payloads (the profile, snapshot, consensus and landing slots,
   and the issuer secret) were copied plainly while a writer could be rewriting them.
3. **Rendezvous claim races** (macOS and Windows), found while migrating:
   - A claim became visible (`CLAIMED`) before the claimant wrote its pid and wanted id, and the daemon
     could read them unwritten.
   - A client that timed out stored `FREE` unconditionally, even while the daemon was writing its
     answer into the slot, which a new client might meanwhile claim.
   - A slot whose client died holding `READY` was never reclaimed.
   - A reader could map the bootstrap object before its header was written, and read it torn.

## Root cause

The storage boundary had no interior-mutability discipline. References into shared memory were the
access path, and which bytes were concurrent was a convention in comments, not a type.

## Impact

- **Undefined behaviour reachable from safe code, by the language's rules.** It was never observed as
  a miscompilation.
- **A handful of cross-process race windows in the rendezvous.** A client could be assigned a stale or
  wrong id, the table could leak a slot, or a client could misread the header of a daemon mid-creation.

## Exact edits

- **`crates/mem/src/words.rs` (new).** A layout declares:
  - atomic **words**, a run of `Width::U32`/`U64` words (one or strided);
  - **racy bytes**, `Width::U8` spans accessed only through `AtomicU8` on both sides, for seqlock
    payloads, so the race they tolerate is between atomics of one width;
  - plain **span runs**, such as a ring's slot bodies.

  `Words::layout` validates the layout into an indexed `Layout`. It refuses words or spans past the
  object, words that are misaligned, any two runs sharing a byte, and any span touching a word. A
  byte-model oracle checks the arithmetic on every range.
- **`crates/mem/src/shared.rs`.** No reference into a mapping is handed out.
  - Plain bytes cross by copy (`read`, `write`), refused if they touch a declared word.
  - Racy bytes cross by `read_racy`/`write_racy`.
  - Words are reached through `atomic_u64`/`atomic_u32` for a declared word of that exact width, or in
    constant time by a `RunId` resolved once (`run_u64`/`run_u32`).
  - A declared span is copied in constant time by a `SpanId` (`read_span`/`write_span`).
  - `declare` sets the layout of an object opened before its header was read.
  - The one reference path left is `ExclusiveObject`, whose `unsafe fn new` carries the
    single-accessor promise `Region::shared` needs.
- **`crates/ipc`.**
  - The ring and the region declare their words and slot bodies and resolve them once.
  - The bulk area is reached by `read_bulk`/`write_bulk`.
  - The rendezvous claim protocol gains `CLAIMING` (fields written, then `CLAIMED` published) and
    `ANSWERING` (the daemon takes a claim by CAS before writing its answer).
  - A client's timeout takes its claim back only by CAS, and reads its answer then CASes it to `DONE`.
  - The daemon reclaims a slot left `CLAIMING`, `READY` or `REFUSED` past twice the claim wait.
  - The start stamp is the header's publication word.
  - The files are panic-free (no indexing).
- **`crates/anchor`.**
  - The segment's layout is derived from its geometry.
  - The header's generation is an atomic word.
  - Payloads and the issuer secret are racy spans, and rings' data is plain.
  - `attach` reads the header with only its seqlock word declared, then reopens with the full layout.
- **`crates/vfs/src/recover.rs`.** The image traits copy. A slot is validated by streaming its payload
  through a bounded buffer for the CRC, and only the chosen image is copied out.
- **`crates/db`, `crates/server`.** The log ring and the content adapter use copies. The tests that
  injected torn publications write through the atomic generation word and racy bytes, not plain bytes
  over the word.

## Evidence

- **Tests** (all by use, through real mappings):
  - `slates-mem` shared tests:
    - copies and declared words cross two mappings;
    - an undeclared or wrong-width atomic view is refused;
    - a copy touching a word is refused and writes nothing;
    - racy spans cross, while a plain copy of them is refused;
    - span copies cross, and an out-of-run span or an overlapping declaration is refused.
  - The `words` oracle, compared byte by byte on every range of a ring layout.
  - The rendezvous claim tests:
    - a claim still being written is not answered, and once published it is answered with its id;
    - a claim abandoned mid-write is reclaimed past the stale bound;
    - a client never reads an unpublished header.
  - mem, ipc (incl. the cross-process rendezvous), anchor (cross-process), db, vfs, client, server
    (lib 129, daemon 17/18, recovery 8) and the CLI flow (13) pass on macOS and Linux (io_uring).
  - Clippy is clean on the Windows cross-lint.
- **Cost** (instruction counts, one push and one pop of a 64-slot ring, callgrind in a Linux container,
  each tree in its own target directory):
  - before this change: 307 instructions;
  - first cut, which searched the layout on every access: 1,186. Rejected.
  - final: 450.

  The words and spans are resolved once, so every hot access is constant-time, validated arithmetic.
  Wall clock, best of six at load 11: 212 ns before and 221 ns after for the spinning round trip,
  within noise; the parked round trip is no worse.
- **Unsafe budget.** `slates-mem` rises 19 → 22: two plain copies, two racy copies, and
  `ExclusiveObject::new`. The two Windows range views are gone.

## Siblings

- **`slates-machine::segment::Segment`** writes its seqlock generation plainly too, but it has no
  second mapper (no `open`) and no user outside its own tests. The anchor's Profile region replaced
  it. It is reported for removal, not changed here.
- **The anchor segment's words** are still reached by searched lookup (`word_at`), off the IPC hot
  path. The db log append makes four lookups per commit. Resolving them once, as the ring does, is
  owed if a measurement shows it matters.
