# w-perf → main: the push finding is with w-hg; the "idle 34 s" was my harness; the stream profile is running

1. **The push finding is with w-hg**, in inbox/w-hg-from-w-perf-push.md. Base 94c1696bb, M4 Max.
   - The unbundle request takes 94.6 s at the server, with 90.6 s of CPU on one shard. Stock's whole push is 9.7 s.
   - About 57% is `push::intake::Taker::files_present`. For each of the 57k manifests it rebuilds p1's text and its own whole text from the delta chain (`HgIndex::revision_text`, about 85% of the step) and decodes p1 into a map.
   - About 41% is `Taker::take`. Of that, about 68% is `node::check`, the collision-detected SHA-1 over every pushed revision's full text.
   - The store's landing is under 1%.
   - Proposals to w-hg: keep the previous manifest's text across steps (a); or compute the changed entries in `take` (b). I can do the index side (c): `revision_text` starting from a text the caller holds.
   - **A design question for you or Ada:** stock 7.2 checks no node hash at unbundle. Ours is about 26 s of the push. Faster SHA-1 would give up collision detection, so I won't change it without a ruling.
2. **The idle first 34 s was my harness, not slates or the connection.** My profile script gave hg a fresh HOME, so the uvx wrapper rebuilt hg before its first line ("pushing to" at 29.93 s).
   - hg_scale.sh is unaffected: one HOME for the run, first hg call before timing. Its 98 s push = 2.9 s client bundling + 94.6 s server.
   - Stock has no such stretch to compare. The script now runs hg once before its clock.
3. **The stream's 37× gap is being profiled now** (r20, same script with STEP=stream: samples, client timeline, bytes received). Stock's stream bytes on the wire are measured beside it. I'll name its step by phase, then fix it red first.
