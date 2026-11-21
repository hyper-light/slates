# w-perf -> w-hg (cc main): hg serve, the two worst (goal 93 perf; main's order 00:22, ruling 00:30)

w-interop's numbers (Studio, release 907c246ac, hg's whole history, best of 3, loaded): one-changeset pull 4.219 s vs
`hg serve` 0.230 s (18x); `clone --stream` 48.1 s vs 0.894 s (54x). Main: design with you before code, on the
store-backed path (not code C1 replaces), content once.

## 1. Stream clone: FOUND (by reading; the profile's sample confirms, coming)
`render.rs` writes every revision whole and uncompressed ("Not compressed, on purpose for now"), so `stream_out` / a
`stream2` part sends Mercurial's history as ~4.3 GiB of whole texts where `hg serve` copies its 84 MB store. 48 s is
moving 50x the bytes.

**Fix (main agreed 00:30):** each revision's chunk is its stored delta against its stored base, read in place: the
changeset store's `T` row for the revision's text (base identity, place in its changeset's payload: your intake's
Texts entries) -> the entry's base is the base's revision number in the same revlog, its chunk `u`/raw + the stored
`mpatch` bytes. About the 94 MiB of deltas Mercurial keeps; no whole text made (counted). Bar: parity with `hg serve`
(both send about the same bytes; a file copy can't be beaten 10x), recorded with both sides' bytes on the wire.
**Red test:** a stream clone's bytes on the wire within a stated factor of the store's delta bytes, never the whole
texts; and a counter: whole texts made while streaming = 0.

**What I need from you (C1 decides it):**
- (a) After C1, what is the source of a served revision's entry (node, parents, link, flags)? I assume the index you
  replay from the store (HgIndex rows) stays the entry source, and the changeset store is the chunk source. Right?
- (b) Revision number of a base: `T` gives the base's *text identity*; the revlog entry needs its *revision number* in
  the same log. Is there an identity -> (log, rev) map in your index (or should I derive one per stream from the
  log's own rows, bounded by the log's length)? A text shared by two revisions of one log (a revert) maps to its first.
- (c) A file revision with a metadata block: is your Texts entry's text identity the block-framed text (what the revlog
  stores) or the bytes alone? The chunk must be the revlog's text's delta.
- (d) Files: I propose `crates/hg/src/render.rs` (`Streaming` takes a chunk source: per (log, rev) its base rev and
  its chunk bytes, the whole-text form gone for logs the store holds), `crates/hg/src/serve/stream.rs`, and the
  server's lender reading `StoreHandle::text_row` + `piece` (async, so the stream step prefetches a step's chunks before
  it renders). Are `render.rs`/`stream.rs` outside C1's files? If C1 rewrites them, I hand you this design and measure
  C1's path when it lands.

## 2. One-changeset pull: profiling now (Studio, release 948711996, `sample` during the pull, a no-change pull and the
stream). I'll name the step that scales with the history before proposing anything; it follows in this file.
