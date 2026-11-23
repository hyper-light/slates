# The bounded-claim flow sized its claims from the capacity claimed so far (2026-11-23, §4.2, A-98, A-401, A-853)

Found by use: CI's nightly `live_chaos` lane. `a_bounded_claim_is_served_by_its_successor_or_refused_before_it_ships`
(`crates/cli/tests/live_chaos.rs`) failed on both Linux runners every night from 2026-11-20 (runs 44729422032,
44897075250, 45017186325, 45176536354). It is a test fault, not the server's admission.

## Description

The flow makes six bounded claims of three quarters of a shard's content reserve on one fleet, so that a daemon's two
shards hold two such claims and refuse a third. It read that reserve from the owner's `slates status`, shard 0's
`reserve=`.

`reserve=` is `ShardBudget::capacity`: the capacity the shard has claimed from the shared content pool so far
(A-98). A shard claims extents at admission only and grows from one unit by its work (A-853, A-848). On a Linux daemon
that has admitted nothing, it is 0.

So every claim was `0MiB`. The first write into the 0 MiB volume was refused `NoSpace`, the writer thread panicked,
and the flow reported "every write answered, iteration 0 (ClientWrite): … the writes panicked" and "non-vacuity: …
no iteration shipped". The stream bound's doc (`Claim::Bounded`) cites `reserve=1073741824` on CI's macOS runner, where every extent is an object of its own.

## Measured

- CI: every night's log shows "claims of 0MiB: 0 shipped and judged after their owner's death, 0 refused before they
  shipped", and "the probe write is answered: Refused(NoSpace)" (`live_chaos.rs:5448`).
- adacachy2, the release build of 05e9bb698, CI's command: the same failure in 2.29 s.
- A fresh two-shard daemon on adacachy2 (`slates daemon --quick --shards 2`): `slates status` reports `mapped=0`
  and `reserve=0` on both shards. Its own boot line says `reserve_per_shard = 21997586432: memory capacity / shards /
  classes`.

## Root cause

The flow took the shard's claimed capacity for its ceiling. The ceiling an admission meets is the shard's slice of the
pool: each partition's arena range is `reserve_per_shard` bytes (`DaemonConfig::shard_content_layout`), and the pool
is `reserve_per_shard × shards` (the capacity `daemon::sample_pressure_hold` holds back from). Three quarters of one slice is admitted from a
fresh shard, two fit in the pool of two slices, and a third does not. That is the flow's premise, and it holds on any
machine.

## Fix

- `crates/server/src/config.rs`: the reserve's derivation is two public functions, `effective_capacity(&MemoryFacts)`
  and `reserve_per_shard(effective, shards)`. `DaemonConfig::derive` calls them, so the daemon's derivation and its
  notes are unchanged.
- `crates/cli/tests/live_chaos.rs`: `shard_reserve`'s status parse is replaced by `shard_content_ceiling`. It reads the
  daemon's own derivation over `Facts::query()`'s memory and the flow's `SHARDS`. The daemons are the test's children,
  so they see the same memory and the same cgroup and rlimit bounds.

## Impact

The flow had never shipped an iteration on Linux, so A-401's case across real processes (a bounded claim served by its
successor, or refused typed before it ships) was unjudged there. No product behaviour changed.

## Siblings

- `Claim::Bounded`'s doc still cites `reserve=1073741824` on CI's macOS runner. That is true of macOS, where every
  extent is an object of its own, and its sizing (`volume_bound_mib`, the stream bound) does not read the status.
- No other test reads `reserve=` (`grep -rn 'reserve=' crates/*/tests`).
