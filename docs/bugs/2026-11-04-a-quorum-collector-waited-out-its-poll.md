# A quorum collector waited out its poll after its quorum had answered (2026-11-04)

## Description

Every cross-node dispatch in `slates-cluster` (a register record's commit, a ledger's promotion, a council or root
group round's broadcast, a content put's offers and transfers) sends one request per holder from a child task and
collects the replies in the calling task. The collector checked its reply channel, and when it was empty parked a whole
poll interval (`DispatchWait::keep_waiting`: `sleep(poll_interval_ns)`). Nothing woke it when a reply landed. So a
commit cost the poll however fast its holders answered.

Measured on 2026-11-04 (adacachy, AMD Ryzen 9 9950X3D2, 32 threads, 122 GiB; load average 36–42; each run in a
`systemd-run --user` unit capped at a quarter of the cores) by
`cargo run --release -p slates-cluster --example replication_bench 1000 0`: a register commit from an owner to two
holders over real loopback endpoints took 10,125–10,194 µs at the median at the daemon's 10 ms poll, over a transport
round trip of 10.8–14.0 µs. At a 100 µs poll it took 1,107–1,116 µs.

## Root cause

`DispatchWait::keep_waiting` parked on the timer alone, and the dispatch tasks sent on a plain `std::sync::mpsc`
channel, which wakes nobody. The poll was the only thing that ended the park.

## Impact

- Every record commit, merge record, promotion and content put paid up to one poll per round after its quorum had
  answered (10 ms at the daemon's poll, a tenth of its 100 ms heartbeat).
- Any per-write replication built on this path (§4.10 live shipping) could never get under the poll, so a 1 ms goal
  was out of reach whatever the transport did.

## Fix

- `crates/cluster/src/lib.rs`: a dispatch's reply channel is a `ReplyTx`. Its `send` hands the reply over and then
  wakes the task that made the channel, the collector, through that task's own waker word (`slates_rt::waker`, no
  reference count).
- `DispatchWait::keep_waiting` now parks with `within(poll_interval_ns, idle())`, which ends at the poll or at the
  first wake. A wake from anything else only ends the park early: the caller drains, judges and parks again. The
  deadline and the progress-extension rules are unchanged.
- The five dispatch sites use it: `broadcast`, `commit_record_on` and the ledger promotion in `lib.rs`; the put round
  and the chunk transfers in `content.rs`.

The failing test first: `a_commit_returns_at_its_quorum_not_at_its_poll` in `crates/cluster/tests/commit.rs`. It
runs on the simulation's virtual clock, so it cannot flake on load. On origin/main (`76b450a1`) it fails with "the
commit took 10000000 ns of virtual time, at least its 10000000 ns poll". With the fix it passes.

After the fix, measured the same way and interleaved with the old build five times, the commit at the daemon's poll
took 18.9–19.9 µs at the median and 28.8–32.9 µs at p99, with a worst sample of 44.7 µs
(`docs/wip/BENCHMARKS.md`, "Replication latency").

## Siblings found, not changed here

- The daemon still ships a head only on its 100 ms record period (`fleet::run_record_period`), so a head's placement
  waits up to a period before this commit starts.
- The reader's fetch path (`content::fetch`) has its own command and event channels, which this change does not
  touch. Whether it waits out a poll the same way is not yet measured; it serves reads, not replication.
