# g228 -> main: A-826 landed; 85a isn't closable yet -- the Linux floor, and the next mechanism (2026-11-21 ~10:25 PST)

**Landed 5e3ec5bc5: the busy-harvest interval alone (A-826**, renumbered: main took A-823 to A-825 meanwhile).
kiln r21, adas-mac-studio.local-2, 32 cores, 5 rounds per condition, every round in BENCHMARKS. Added p50, base
against interval:
- quiet: 164.5-167.7 against 137.0-141.2 µs (5 of 5), owner CPU equal
- loaded (32 busy loops): 468.3-800.2 against 140.9-447.5 (5 of 5), owner CPU 680-980 against 400-670 ms, p999 2.9-11.7
  against 1.6-2.1 ms
- `-b`: 4 of 5 medians ahead; soft spot on record: round 3 461.6 against 385.6, p999 higher in 3 of 5
- `-b` + loaded: 4 of 5 medians ahead; writes over 50 ms 0-8 against 0-47; no failures in either arm
- The lesson (wake-learned spin bounds defeat themselves under contention) is in the rejection note (a8ee99bf7).

Landing note: main moved faster than my 4-minute focused-test run. I retested three merges (all green), then merged
the last two commits, hg crate only, and pushed without a fourth rerun. Your post-merge suite covers that gap. Also,
main has two amendments that were both numbered A-821 (refining and git); it looks renumbered now, but please check.

**85a: not closable; here is why.** Linux hop medians (ubuntu-latest, cache arm, run 12/13): exchange round trip 57.3 µs,
build 26.6, to control 8-9, collect 6-7, reply 6, to owner 4, lane/borrow/settle < 2. Total added 107-142 µs.
- **The floor:** on the same runner, the bare transport (`replication_bench`, no daemon) ships a 4 KiB live write to two
  holders in 42.2-83.0 µs at p50 (runs 13, 14). The daemon's round trip equals it. So **50 µs added at the median is
  under the transport's own round trip on a 4-vCPU runner**: 85a can't close by daemon work alone there.
  The daemon's own share is about 50 µs (build 27, the hops 22).
- **Next mechanism, if you want it: split the build** (27 µs, the largest daemon hop). Publication to message:
  - the delta's encode;
  - the shipper's append (its hash);
  - the entry's encode;
  - the seal.

  Count each per write, then cut the biggest. The copy cut showed copies aren't it, so expect hashing or the encode.
  The hops to and from the control shard (12-13 µs) are the other candidate (A-200's cross-shard dispatch).
- **The transport round trip itself** (~50-60 µs bare) is the larger lever. It isn't mine (the transport's owner, or a
  goal question): AEAD per packet, the endpoint's turns, the demux.

**Ask:** rule which. (i) I split the build next. (ii) 85a's goal is restated against the transport's floor (e.g.
"daemon share under 50 µs", which is about 50 now). (iii) Both.

**For you to delete:** agents/g228-busy-interval (landed), agents/g228-macos-hold, agents/g228-exchange-split,
agents/g228-land-cache, agents/g228-copy-rejected, diag-g228-live-10..14 and their diag-results branches.
