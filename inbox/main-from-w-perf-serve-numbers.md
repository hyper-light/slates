# w-perf -> main: hg serve, the pull's history-scaling step measured; one order change proposed (2026-11-21 01:05 PST)

**Profile** (big Studio, release main 948711996 with `--vcs-store`, hg-src imported, `sample` 1 ms; load 15-31):
one-changeset pull **24.95 s**, no-change pull 0.20 s (x4), stream clone **288 s**. On-CPU in both the pull's and the
stream's turns: `slates_vcs::Index::get` / `ContentStore::get` / `memcmp`; for the pull, `Indexed::new` (the served
view, rebuilt every turn) holds 5,281 of the turn's 6,500 samples.

**The step that scales with the history: every index lookup walks every run.** `Index::get` reads the tail, then
each run from the newest until one holds the key. The learned merge (`compaction_due` / `compact`) has no caller
outside slates-vcs's tests, so nothing ever merges. Measured with `crates/hg/examples/index_runs.rs` (release, same
Studio, hg-src imported in-process, window 256 KiB, 2,000 lookups; work branch agents/w-perf-serve 304ad5cdd):
- after the import: **155 seals = 155 runs**; a lookup visits **p50 123 / p99 156 runs**, **11.8 µs** p50;
- one `compact` of every run: **0.28 s** for 456,897 rows; then a lookup visits **2 runs**, **0.6 µs** (20x).
At 12 µs a lookup, 25 s is ~2M lookups for one changeset, so a second factor scales too: each turn rebuilds the view
(dozens of lookups), which costs more than the turn's quantum, so each turn advances the answer one step. The merge
cuts it 20x; I'll measure what's left after it.

**Order change proposed:** wire the merge first, then the stream's bytes. It's the multiplier under both the pull
and the stream (the stream's 288 s is lookup-bound CPU, not its 4.3 GiB), it's smaller, and it's outside C1
(`server/src/vcs_store/hg_view.rs`'s lend reports the turn's lookup time and runs `compaction_due` -> `compact` timed,
on the owner shard; the import's group ends report inserts). Red: a lookup after N seals visits O(log N) runs (counted),
a one-changeset pull's lookups flat in the history. I'm starting it now; say so if you want the stream first and I'll
switch. Store side of the stream fix is done locally (H rows per entry, I rows by node), not landed.
