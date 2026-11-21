# g228 -- progress (close-list item 85a: a live write adds < 50 us at the median and < 1 ms at p999 under load)

## State (2026-11-20 ~17:20 PST)
- My first piece (each shard on a live write's path opens its idle window) was already on main as A-396 ("the live
  holds", landed 2026-11-18, before its A/B). Dropped: local branch agents/g228-live-write-adds-idle-window, never
  pushed (for main to delete: nothing remote).
- Working branch agents/g228-live-write-adds-holds-ab in ~/slates-work/g228-live-write-adds, base origin/main
  b402afc7c. Only change: crates/cli/tests/live_fleet.rs, `measure_what_live_shipping_adds_to_a_stable_nfs_write`
  now reads each daemon's CPU (process; Linux also the control shard's thread) over the stream, the 500 ms after it
  and the second after that.
- Running: A-396's owed A/B, holds (main) vs no holds (live_hold::hold asks nothing), release, 5 interleaved rounds,
  alternating order. Linux: kiln r17 on adacachy. macOS: kiln r24 on adas-mac-studio.local-2 (CARGO_BUILD_JOBS=4).
  Script: ~/.cache/g228/ab.sh (passed inline; refuses outside a slates checkout and under 60 GiB free).

## A-396 A/B, macOS (adas-mac-studio.local-2, 32 cores, load 32-34, base b402afc7c, release, 5 rounds interleaved)
Added at p50 / live p999, us:
- holds (main):  511.9/4645.0, 516.1/1045.0, 596.0/1859.8, 477.9/1171.2, 454.0/859.8
- no holds:      548.1/1615.1, 466.0/1408.9, 551.0/1721.2, 467.7/886.5, 455.1/956.8
Every hold declined on macOS (hold.declined ~7,900 a run, A-387's predicate), so the arms are the same; idle CPU after
the stream the same (10-60 ms a second a daemon, ps's 10 ms grain). The goal is ~10x off on this Mac: owner's round
~433 us = exchange 297 + to_control 65 + build 38 + to_owner 28 + lane 4; the reply's wake 68 on top.
Next A/B running (kiln r31): holds vs no holds vs forced (A-387's decline bypassed), each candidate's counters printed
(hop.holder). Linux (r17, adacachy) is frozen by its cgroup freezer at load 80-89 (wchan do_freezer_trap).

## Three-arm A/B, macOS (same Studio, load 31->16, b402afc7c + c1aabba): added p50 / live p999, us
- holds:   234.2/707.7, 177.3/393.5, 179.9/402.0, 182.8/453.5, 167.7/400.3
- noholds: 169.2/434.4, 184.4/416.4, 169.8/447.7, 187.3/491.9, 205.0/423.0
- forced (A-387's decline bypassed): 297.7/534.1, 223.9/447.2, 236.6/502.3, 224.4/514.3, 294.2/585.5 -- rejected:
  ~50 us worse, owner CPU in the stream 620 vs 380 ms. A-387's decline is right on macOS. p999 < 1 ms holds here.
- Holder's serve ~10 us an entry (hop.holder): the 100 us exchange is transport and the two control shards' wakes.
- hop.reply (the held NFS reply's wake after the fold, same shard) 65-116 us.

## Reply-first fold (2fbc20496 on agents/g228-live-write-adds-holds-ab): the fold's follow-on work queued behind
the replies it woke. Focused tests green locally (debug, base 56f33c6f6): a_fold_answers_... (16 deferred, 0 inline),
a_live_writes_round_is_counted_hop_by_hop... A/B on 56f33c6f6 (Studio, load 9-11): hop.reply 88 vs 92 us a write --
the hypothesis (the fold's tail held the reply) is NOT confirmed; added median 2,368-2,676 vs 1,312-2,967. Not landed.

## NEW FINDING (blocks 85a): main regressed 13-16x between b402afc7c and 56f33c6f6 on the same Studio
- Added at p50 2,305-2,967 us (one round 1,312) against 168-205 us; every daemon burns ~5.5 s CPU over the stream
  (was 0.1-0.5 s); in rounds 3-5 the daemons spun 220-600 ms in the 500 ms after the stream and 760-900 ms in the
  second after that (round 4): holds now KEPT on macOS at load 9-11 (asked ~8,000, declined 1,800-4,100; at load 15-33
  all declined). Hold code unchanged in the range; live_ship.rs +2,072 lines (A-768 capture turns, A-763, the holder's
  task 2d4bb1285, ...). Running (kiln r36): main56 vs noholds56 vs old (b402afc7c) to split holds from the rest.

## Incidents (recorded honestly)
- 2026-11-20 ~09:57-10:40 PST: two A/B runs started without `cwd` ran in the runners' home folders; the script's
  `rsync -a ./` copied adacachy's home (73 GB) into ~/.cache/g228-ab-noholds-src before it was stopped; removed by exact
  path on both machines. Fixed: runs pass cwd=<clone>; the script refuses outside a slates checkout.
- r7's orphaned build on adacachy (after a runner restart) wedged with its output pipe unread; stopped by PID, its
  leftovers deleted. Builds now log to a file only.

## Next
1. Read both A/Bs: added median, live p999, idle CPU after the stream (holds vs none).
2. If the holds cost idle CPU or latency: fix in this piece (Ada: performance gaps fixed up front).
3. Land the measurement change; BENCHMARKS table; GAPS line under A-396; inbox/main-from-g228-close.md when 85a holds.
