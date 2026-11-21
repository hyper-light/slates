# w-perf → main: the stream capability per repository (your 02:31), plus a real stream bug it found

LANDED 8968640c054f7b0bc2b837a6bde58d595cd9ac72 (merged on origin/main c6f71e8e4).

1. The fix you asked for. `stream`/`streamreqs` are now advertised per repository, only when the store keeps its logs.
   - `State::stored_logs` comes from `HgStore::unstored`. That flag is set when an import is placed, an inline record is replayed (an import, a made changeset, or a pre-C4 push), or a made changeset lands.
   - A push replayed from the changeset log leaves the flag clear, so a rebuilt pushed repository still streams.
2. The red test, run with stock hg 7.2 (uvx mercurial==7.2, on this MacBook). The new test is `stock_hg_clone_stream_streams_a_pushed_repository_and_clones_an_imported_one_plainly` (cli/tests/hg_interop.rs).
   - With the gate disabled, the imported leg aborts exactly as you saw: "streaming all changes / abort: operation forbidden by server", exit 255.
   - With the fix it clones plainly with hg's own warning ("stream clone requested but server has them disabled"), holding every changeset.
   - Unit tests: `a_stream_clone_is_advertised_only_where_the_store_keeps_the_repositorys_logs` (imported no, pushed yes; lent and held). `a_landed_push_is_replayed_into_the_store_from_its_entry` now also asserts the stream is still advertised after a rebuild. Both were shown red with the fix removed.
3. A bug the stock test found, now fixed. The pushed leg's stream clone failed: "integrity check failed on 00changelog:0". So my earlier stream landing was broken against real hg.
   - Cause: a revision deltaed against the null base was sent with the changegroup's 12-byte trivial hunk in front of its text.
   - It is now sent as the text alone. A place that isn't exactly that hunk is refused as Unreadable.
   - The unit test missed it because its store doubles kept such revisions bare. They now hold the hunk the way a changegroup does, and they went red before the fix.
4. GAPS: an OPEN entry under "Goal 93 perf, hg serve", to stay until w-hg's C1 step 3 lands.

Next: the stream A/B on 8968640c0 (all three rounds, stock beside it) on the big Studio. Then I'll name what remains in the pull's turns. r3 on e3a253e96 was lost when its runner restarted; it would have measured the broken stream anyway.
