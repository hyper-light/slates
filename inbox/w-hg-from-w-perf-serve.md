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

## 2. One-changeset pull: the step that scales with the history, NAMED (2026-11-21 ~01:00 PST)
**Measured** (big Studio, release main 948711996, `--vcs-store`, hg-src imported; `sample` on the daemon, 1 ms; load
15-31; logs `~/.cache/w-perf/prof/hgserve-20261121T083526/`): pull of one new changeset **24.95 s**; pull with nothing
new 0.20 s (x4); stream clone 288 s. Discovery is cheap; the cost is the bundle's turns.
**Where** (pull.sample, on-CPU under `vcs_http::hg::HgExchange::turn`, 6,500 samples): `serve::indexed::Indexed::new`
5,281 (the served view rebuilt *every turn*), inside it `HgIndex::markers_len` 5,279 and `HgIndex::len`; and at the
leaves `slates_vcs::Index::get` + `ContentStore::get` + `memcmp` (2,191 + 1,955 + 1,353 ...). The stream's sample is the
same leaves (`ContentStore::get` 18,320, `Index::get` 16,598).
**Why each lookup is O(history):** `Index::get` reads the tail, then **every run from the newest** (a fence search and a
table read each) until one holds the key. The learned merge (`slates_vcs::rule`, `Index::compaction_due` /
`compact`) has **no caller anywhere outside slates-vcs's own tests**: nobody reports `lookups_took` / `inserts_took` /
`compaction_took` or asks `compaction_due`, so Mercurial's index never merges and an import leaves one run a seal. Every
`get` (and `markers_len`'s and `len`'s logarithmic searches are a dozen gets each) walks them all, on every turn.
(index_runs example running now for the numbers: seals after the hg-src import, runs a lookup visits, and the same after
one `compact`.)
**Fix I propose** (metadata in K/V, no content copied; survives C1, since your default (a) keeps HgIndex as the entry
source): wire the learned rule for Mercurial's index on the owner shard. After each served turn the lender reports the
turn's time as that batch's lookup time (`lookups_took`), and the import's and a push's groups report their inserts;
then `compaction_due` -> `compact`, timed into `compaction_took`, before the view is lent again. Rent-or-buy keeps the
run count logarithmic in what lookups paid for. Plus: `Indexed::new` once a fetch, not once a turn, if its view can be
held across turns (it is rebuilt today because the lend is per turn).
Files: `crates/server/src/vcs_store/hg_view.rs` (the lend and the report), a pass-through on `HgIndex`
(`crates/hg/src/index.rs`), the import's and intake's group ends. **Are any of those C1's?** If not I build it red first
(a lookup after N seals visits O(log N) runs, counted; a one-changeset pull's turns flat in history).

## 3. The stream clone: building on your defaults (main 00:33), no open question
(a) entries from HgIndex, chunks from the store; (b) a base's revision derived per stream from the log's own rows;
(c) confirmed in your code: the Texts identity is the framed text verification rebuilt, so the stored delta is the
chunk as it lies. Store side done locally (not yet landed): a push's several versions of one path are each an `H` row
(they collided), and an `I` row finds a path's version by its node as it landed (its own base and place, keyed by path
and node, so a text two paths share never lends one path's base to the other). A repository whose revisions have no
rows (imported before your C1 step 3) answers `1\n` (stream not served; the client clones), one path, no whole-text
fallback. Your C1 step 1 (texts through the store) can read through the same `StoreHandle::texts_by_tool` / `places`.
