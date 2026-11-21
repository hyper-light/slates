# w-perf → main: a second stream bug at hg-src's scale, landed 94c1696bb; pull turns named; A/B + push profile running

LANDED 94c1696bb143dfb6bb30f49c1d19e37bf3134138 (on origin/main after your lint round 4e32bd101; one conflict in render.rs, both sides kept).

1. Bug found by the A/B. r13 ran on 8968640c0, on adas-mac-studio-2.local (M4 Max, 14 cores), self-provisioned and placed by needs, per your 03:36 note.
   - Every slates stream round failed: "abort: unexpected response from remote server: empty string", 0.235 s.
   - I reproduced it locally on hg's first 2987 changesets. The renderer refused rev 0 of mercurial/__init__.py, an empty file.
   - 7.2 sends an empty text against the null base as an empty delta with no hunk (hg debugbundle --all: length 0). My 8968640c0 check had refused it.
   - Fixed: an empty delta is sent as it is.
2. A second problem the same repro showed. hg verify on the streamed clone gave 2 warnings ("data/mercurial/__init__.py.d not in fncache").
   - 7.2 keeps a log whose every chunk is empty inline. The client lists in fncache only the files it was sent.
   - Fixed: such a log is now rendered inline, in both renderers.
3. Tests. The stock-hg test now includes an empty file and requires a clean `hg verify` of each clone. It went red on both problems, then green.
   - hg's first 2987 changesets now stream (888 files, 5.43 MB) and verify clean.
4. The pull's turns, from r13's traced pulls (time to response headers, three rounds each; the M4 Max is a different machine from r3's):
   - capabilities: slates 1.7–2.0 ms, stock 1.5–1.7 ms.
   - batch (heads + known): slates **8.7–9.5 ms**, stock 0.4–1.3 ms.
   - getbundle (one new changeset): slates **9.5–10.9 ms**, stock 0.6–0.8 ms.
   - Pull wall time: slates 0.205–0.220 s, stock 0.187–0.194 s.
   - Most of either wall is hg's own client start-up. slates' remaining ~20 ms are the server's time in batch and getbundle, ~9 ms each, against under 1 ms for stock.
   - A no-change pull's getbundle is 0.7–1.2 ms on slates, so the ~9 ms in batch is per request, not per changeset. Most likely the served view built afresh each turn (`Indexed::new`), the per-turn view you put next after the merge. I'll confirm with a profile.
   - r13 timed steps (s): pull slates 0.215 0.208 0.211, stock 0.188 0.195 0.199. Push slates 96.99 97.37 97.49, stock 10.17 9.71 9.51. clone -U slates 14.25 14.31 14.45, stock 7.15 7.00 7.04.
5. Running now as r16 on Studio-2 (M3 Ultra), tree 94c1696bb: the full A/B (stream included, with an `hg verify` of a streamed clone each round), then the push profile by phase.
   - The profile script is crates/cli/bench/hg_push_profile.sh: the daemon sampled in 20 s windows, frames counted by busy samples, Rust names demangled. It lands with the results.
