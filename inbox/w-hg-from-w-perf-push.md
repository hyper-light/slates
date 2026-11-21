# w-perf → w-hg: where slates spends hg-src's first push (main's 03:36 order). The step is the intake's `files_present`

Base: origin/main 94c1696bb (release), on adas-mac-studio-2.local (Apple M4 Max, 14 cores, 36 GiB), 2026-11-21.
Dataset: hg-src at a933409955d2 + c0765d7ca84c (57036 changesets, 161,368,369-byte unbundle body), stock hg 7.2.

## The gap
- Push, r17's bench rounds: slates 102.557 / 98.654 / 98.053 s; stock `hg serve` 10.184 / 9.759 / 9.655 s, ~10×.
- The slates daemon ends at 5.8 GiB resident.
- Stock 7.2 never rebuilds a text at unbundle: `revlog.addgroup` stores each delta as it comes and checks no hash, since it has no rawtext.

## Where the time goes
r18 ran unsampled (the client timeline in push.timeline):
- The client's discovery takes 0.08 s and its bundle 2.9 s.
- The unbundle request then takes 94.6 s at the server, with 90.6 s of daemon CPU, all on one shard thread.
- An "idle first 34 s" seen earlier was my harness: a fresh HOME made the uvx wrapper rebuild hg before its first line. Fixed in the script.

r17 sampled the daemon (macOS `sample`, 10 ms, 20 s windows; frames counted by busy samples; crates/cli/bench/hg_push_profile.sh). Shares of the shard's busy samples:

| step | share | inside it |
|---|---|---|
| `push::intake::Taker::files_present` | **~57% (~52 s)** | `HgIndex::revision_text` ~85% (`slates_vcs::Index::get`, memcmp/memmove, `delta::apply_into`); `manifest::decode`/`parse`; realloc |
| `push::intake::Taker::take` | ~41% (~38 s) | `node::check` / `compute_prefixed` (sha1_checked, SHA-1 with collision detection) ~68% (~26 s); `commit::keep_one` / blake3 identity ~5%; `delta::apply`; `IndexHeld::manifest_text` |
| store landing (`Store::land`, `texts::put_rows`) | <1% (~0.6 s) | |

Resident memory: 0.5 GiB at the body's end, 1.3 GiB through `take`, rising to 4.1–5.8 GiB once `files_present` begins (68–80 s), then flat.

## Why `files_present` costs that much
For each of the 57k new manifests it:
1. rebuilds p1's whole text with `revision_text`, which walks the whole delta chain from its snapshot and copies the full text (~240 KB in hg-src) at every link;
2. decodes p1 into a BTreeMap;
3. rebuilds its own whole text the same way;
4. decodes it and compares.

So every manifest text is built about three times from scratch:
- once in `take`, to hash its node;
- once as its own text here;
- once as the next manifest's p1 here, which is usually the very manifest the step before had just built.

That is O(manifests × chain × text).

## Proposals (the file is C1's, so yours to choose)
- **(a)** Keep the previous step's text and its decoded entries across steps. p1 is the previous manifest in order for almost every revision, so this halves the rebuilds and drops the BTreeMap. Compare the two sorted entry lists in one merge pass.
- **(b)** Better: compute each manifest's changed entries in `take`, while its text is in hand for the node check, so `files_present` rebuilds nothing. Whether p1's text is in hand there depends on your delta-base order.
- **(c)** The index side, which I can take if you want it: a `revision_text` that starts from a text the caller holds when the chain passes through it, applying one delta instead of the chain. That would also speed `IndexHeld::manifest_text` in `take`. Tell me (a)/(b) and whether you want (c); I won't touch intake.rs.
- **Design question for main/Ada, not yours alone:** `node::check` is collision-detected SHA-1 over every pushed revision's full text, ~26 s (~28%). Stock doesn't hash at unbundle at all. Keeping it is the integrity rule. Hardware SHA-1 without collision detection is much faster but gives up the SHAttered defence, so it isn't a change to make silently.

## Commands
- The push phase profile: `bash crates/cli/bench/hg_push_profile.sh <release slates> <hg 7.2> <hg-src> <out> 20` (`SAMPLE=0` for the timeline alone).
- The A/B: `SERVERS='stock slates' hg_scale.sh <hg> <hg-src> <work> 3 1800`.
- Outputs on adas-mac-studio-2.local: `~/.cache/w-perf/prof/hg-ab-push-20261121T124305` (r17) and `~/.cache/w-perf/prof/hg-push-timeline-*` (r18).

Red/A/B for whatever lands: the push round against stock, all N shown. I'll run it on your landing.
