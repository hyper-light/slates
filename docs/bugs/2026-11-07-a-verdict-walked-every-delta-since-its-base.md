# A verdict walked every delta since its base (2026-11-07, §4.16 "Position mapping")

Found by agents/loadtest-scale. The 10,000-client rung still stalled after the kept-forward fix
(`2026-11-07-a-kept-forward-waited-for-another-request.md`), so I sampled the daemon on this Mac during the stall.

## Description

The verdict maps an increment's ranges through the deltas that changed its paths since its base. `Green::intervening`
found them by walking every delta from the base to the head and looking the path up in each one's map: one string
comparison per version since the base, whatever that version changed. The fast path skips a path unchanged since the
base. A path changed even once since then paid for every version.

At 10,000 clients over one green, a work's base lags the head by thousands of versions, and every verdict runs on the
green's one owner shard. So each verdict cost thousands of map lookups, the total grew roughly with clients squared,
and the owner shard was saturated while the others waited.

## Measured

- `sample` of the daemon in the stall (this Mac, W2 plus the kept-forward fix, release without LTO, kernel/, 10,000
  clients):
  - the owner shard was idle in 5 of 1,201 samples, against 1,055 to 1,183 of 1,201 for the other four;
  - in it, `Green::rebase → decide → intervening` took about 700 samples, about 530 of them in `memcmp`.
- `crates/merge/tests/engine.rs` (`a_verdict_consults_only_the_deltas_that_changed_its_paths`): 2,000 versions changing
  `b` with one change to `a` among them, then an overwrite of `a` based before them all.
  - Red, the walk restored in place on CI (run 42376010762): 2,000 deltas consulted.
  - Green: 1.
  - The new `Green::deltas_visited` counter is the non-vacuity count.
- The local 10k rung on the fix, at about 500 s: 2,047 of 10,000 clients done, 40,639 submits and climbing. Before it,
  the run stalled for minutes near 50,000 submits as counted then. The owner shard's samples moved to the partition's
  snapshot (below).

## Fix

`intervening` reads the versions `path`'s content history names after the base. A delta records a path's ops exactly
when the commit set its content, which pushes that history entry (the one insertion site, `Green::commit`). So the
walk costs the path's own changes since the base. A base below the fold, whose history entries may be gone, walks
every delta as before. The merge crate's verdict proptests (engine, rebase, base_for) and the server and client merge
suites pass unchanged on both Linux runners (run 42376010762).

## Impact

A verdict's cost grew with its work's lag, not with what changed. At the scale where works lag, one owner shard's
verdicts became the throughput of the whole green.

## Siblings, reported, not fixed here

- **The next wall**, from the same rung's sample after this fix: the owner shard spends about 1,800 of 2,241 samples
  in the database's snapshot.
  - `Db::commit` re-encodes the whole partition (`Partition::to_snapshot`, the codec's `memmove`) and publishes it to
    the anchor segment each time the log fills.
  - The partition holds every work's log (A-207), every client's completion records and every volume. At 10,000 works
    that is large, and the owner's log fills constantly.
  - It needs §4.8's snapshot made incremental or its cadence derived from the partition's size. That is database
    design, raised to the supervisor.
- The other dimensions' histories (modes, symlinks, xattrs) are read by their own lookups. Only content had the walk.
