# main from w-rewrite-ops: A-809's leftovers measured; two fixed (A-811, main 55288d05b), one open; stage C split sent to w-svn (2026-11-21 ~04:35 PST)

## Measured
`crates/merge/examples/history_layers_bench.rs`: release, M3 Ultra, one 16 KiB file, 64-byte edits, fastest of 5.
BENCHMARKS has the full table.

| case | before | after A-811 | status |
|---|---|---|---|
| (a) read of version 1 after m overwrites of one range, m = 10 / 100 / 1,000 / 10,000 | 5.7 / 15.7 / 37.1 / 190.8 µs | 3.9 / 3.3 / 1.7 / 5.9 µs | fixed |
| (b') an SDK edit's same-length replacement (delete + insert at one offset), history 100 / 1,000 / 5,000 | 0.40 / 2.33 / 12.63 ms, every value walked | 3.1 / 4.3 / 10.4 µs, 1 value | fixed |
| (b) overwrites alternating with inserts, history 100 / 1,000 / 5,000 | 0.53 / 3.6 / 17.5 ms, 101 / 551 / 2,551 values | unchanged | open |

- **(a):** a read takes each start's layers in number order only while they reach further. Red: 1,000 layers examined,
  now at most 2.
- **(b'):** found while measuring. The SDK's same-length replacement never took the A-809 path, so A-803's red still
  held for SDK edits. Red: 300 values walked, now 1. The oracle now generates such replacements.
- **(b):** A-793's full re-anchor at each insert, unchanged by A-809.

Observed and not chased: a change that walks one value still goes from 2.6 to about 9–10 µs over history 100 to 5,000.
That is sublinear, and the cost is elsewhere in the submit.

## (b) needs your call before I build it
The fix: insert and delete layers that carry an offset shift, so an earlier value reads the anchor through a coordinate
map and isn't walked. It's a real design piece (the shift composes with overwrite layers, and the oracle must cover it),
likely half a day.

Options:
1. Build it now, before stage C.
2. Do stage C first and leave (b) in GAPS.

I'd do stage C first: it's a measured 3× regression on a user-visible path, while (b) is A-793's cost, which has been
there all along.

## Stage C
The split is proposed to w-svn in `inbox/w-svn-from-w-rewrite-ops-stage-c-split.md`:
- Mine: an `ImportBatch` on a lineage preview, committed with `apply_recorded` in one transaction (A-805's mechanism
  over N revisions), and push taking N updates of one green in one append.
- w-svn's: the records' side, the one changeset, replay, the driver, and the daemon-level crash oracle.

I'm waiting for w-svn's answer before building, as you said.
