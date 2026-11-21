# w-perf → main: the stream's step is named, a fix is landed (bbcf0f72f), and the A/B is running

1. **The step (r20, base 94c1696bb, hg-src, M4 Max).**
   - slates streamed in 15.66 s; stock in 0.474 / 0.436 / 0.492 s.
   - Bytes received: slates 151,736,084 (8452 files, uncompressed); stock 77,885,593 (stream v2, 4282 files, compressed chunks).
   - The serving shard was idle about half the stream; the store thread was busy only 61 samples.
   - The renderer asked the store once per log, in both passes, plus chunk asks: about 12,000 awaits. Each one cost a turn and a served view made afresh.
   - Of the busy half: `Indexed::new` (the per-turn view) 380 of 766 samples; `HgIndex::len` 504 inclusive, a 32-lookup search called on every turn and for every entry written; `Streaming::step` 168; TCP writes 64.
   - So: not the framing and not the store reads per chunk. It is the per-log round trips and the per-entry length search.
2. **LANDED bbcf0f72f.**
   - A rows ask now holds as many logs as the step's room does. A chunk batch runs on into the logs held after the current one. A log's length is its held rows' count.
   - `StoreHandle::texts_by_tool` now takes many logs in one request. It replaces the old one-log request, no second path.
   - Red first: a stream of 65 logs makes 2 row asks and 1 chunk ask. With batches capped at one log it goes red. It gives the same bytes as the stream made a log at a time.
   - The stock-hg stream test and a local clone of hg's first 2987 changesets stream and verify clean.
   - The profile script (crates/cli/bench/hg_push_profile.sh, STEP=stream) landed with it.
3. **The A/B is running as r23** on adas-mac-studio.local (the M4 Max had no memory left; the balancer had put r22 on the MacBook, which I stopped per your disk rule). It times stock three rounds, then slates three unsampled rounds with bytes received and an `hg verify`, then one sampled round.
4. **Still open after that:**
   - **The bytes:** about 2× stock's, because our chunks go uncompressed against stream v2's compressed files. The store keeps changegroup deltas raw; matching stock would mean compressing at send time, or keeping a compressed form, which runs into the "content kept once" storage rule. I'll bring numbers before proposing anything.
   - **The per-turn view (`Indexed::new`).** Your queued item, also behind the pull's ~9 ms batch/getbundle.
