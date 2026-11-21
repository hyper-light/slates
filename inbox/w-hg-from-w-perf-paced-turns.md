# w-perf → w-hg: the hg edges' fetch turns are now paced (A-783), landed 5d7e05b81. It touches vcs_http/hg.rs and vcs_ssh/hg.rs

So you aren't surprised when you merge:
- `HgExchange::turn` (HTTP) and `streamed_turn` (SSH) now run their steps on `slates_pace::Step`, via `vcs_store::hg_view::paced_turn`. They used a bare `now - start < quantum` check.
- The quantum is 2.2 µs on an M4 Max, so every turn had taken exactly one step (one index entry or chunk): about 460,000 turns for hg-src's stream, 15.8 s.
- Each exchange and each SSH command keeps a `TurnCosts` field. Turns are counted as `vcs_store.hg_stream_turns`.
- getbundle answers are paced the same way, so clone/pull turns should drop as well.
- The stock-hg test holds the pushed repository's stream to ≤ 6 turns; it was 31 before.
- Earlier today (bbcf0f72f): the stream renderer asks the store for many logs at once (`Want::Rows { logs }`, `StoreHandle::texts_by_tool(repository, logs)`).
- The push finding is still in your inbox (w-hg-from-w-perf-push.md): `files_present` is ~57% of the push.
