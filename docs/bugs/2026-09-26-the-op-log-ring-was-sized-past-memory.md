# The op-log ring was sized past memory

Date: 2026-09-26. Contracts: D-12 (RAM-only; the daemon is sized by the memory it may use), §4.8 (the
op log in the anchor segment), R3. Found while fixing Windows' anchor-segment commit
(`2026-09-26-windows-committed-the-whole-anchor-segment.md`); Ada: "why would we allot 16.4 GB?"

## Root cause

`log_bytes_per_partition` was `recovery_budget_us × one page per µs`, which amounts to a guessed replay
rate of one page per microsecond (16 GB/s with 16 KiB pages). The recovery budget is 1 s, so:

| Host | Log ring per partition |
|---|---|
| This Mac (Apple silicon, 128 GB) | 16.4 GB |
| KIND pod (1 GiB limit; its boot log) | 4.1 GB |

The size does not depend on memory. The ring wraps and `trim` only moves its head, so over a long run
every page of it is touched and becomes resident: 4.1 GB of log per partition in a 1 GiB pod.

The ring never needed that size. The log holds only what the next snapshot absorbs:
- the cadence snapshots every `recovery budget × measured replay rate` bytes (`SnapshotPolicy`);
- a full ring snapshots, trims and retries the append (`Db::mutate`, `LogFull`).

## Fix

The ring is sized to the tables' share (`table_bytes`), the memory-derived quantity (a ratified share of
the shard's reserve) the snapshot slots are already sized from.

| Host | Log ring per partition, before | After |
|---|---|---|
| This Mac, one shard | 16.4 GB | 11.4 GB |
| KIND pod, 1 GiB | 4.1 GB | 89 MB |

## Tests

- `the_rings_that_wrap_fit_in_the_memory_the_daemon_may_use`, for one to four shards, unbounded and
  under a 1 GiB bound: the logs and the audit, which wrap through their whole capacity, fit in the
  effective capacity. Payload regions (snapshot and consensus slots) hold RAM only for what is
  published in them, since the segment is sparse. The old formula fails this under any bound.
- The db (with its LogFull and torn-tail histories), server lib, daemon, recovery and client suites pass.

## Not changed

The consensus regions are sized `2 × log + snapshot`, so they shrink with the log. They are payload
regions, so their resident RAM is what the configuration groups publish.

## Edits

- `crates/server/src/config.rs`, `docs/wip/TBD_FIXES.md`.
