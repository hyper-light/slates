# Retention decoded every merge record per put

Found 2026-11-07 measuring single-submit latency at `f = 1` against goal 4's 1 ms bound. With the one-exchange commit
(`2026-11-07-a-versions-inputs-and-record-took-five-hops.md`), the first submits were answered in 106 to 300 µs, but
the answer grew by about 40 µs a version. Median per 50 submits, release, adacachy at load 22: 247 µs, 887 µs, 1.9 ms,
3.5 ms, 5.0 ms, 7.3 ms, 10.1 ms, 13.1 ms, 16.2 ms, then 20.3 ms by the 500th. A-235's 500-submit p50 of 6.7 ms had the
same shape.

## Cause, from a profile

`perf record -g` of the 500-submit run: `MergeRecordValue::decode` 46.2%, `MergeRecordValue::from_record_bytes`
12.6%, `content_retention::named` 5.7%. A holder's retention rule (`crate::content_retention`) runs at the door of
every put and after every accepted one. For each manifest it held for the green, it asked whether any position of the
green's merge ledger named it, and decoded every position to find out. Each put therefore cost O(versions × held
manifests), quadratic over a green's history.

## Failing test

`a_submit_late_in_a_greens_history_is_answered_as_fast_as_an_early_one` (`crates/server/tests/fleet.rs`). Its setup is
a settled three-node fleet at `f = 1` and 300 works over one green, submitted one after another. It expects every submit
accepted, and the median of the last 50 answers at most four times the first 50's plus 2 ms. Before the fix (the old
scan put back, otherwise the same tree), the early median was 193 µs and the late one 6.83 ms, so it failed. After,
three runs gave early medians of 97 to 101 µs and late ones of 239 to 245 µs.

## Exact edits

- `crates/db/src/register.rs`: `Acceptor::value_at(object, sequence)`, one lookup of an accepted position.
- `crates/server/src/content_retention.rs`: `named` asks only the ledger position at the sequence the manifest was
  placed for. A green's inputs are placed at their version's sequence (`merge_service::ship_record`), so that position
  is the only one that can name them. The module doc says so.

## Verification

Release, adacachy at load about 10, 2026-11-07, 500 sequential submits at `f = 1`, with the one-exchange commit:
p50 265 µs, p99 731 µs, p999 and max 1.13 ms. Median per 50 submits ran from 116 µs to 473 µs, so a smaller
per-version cost remains, and that is owed.
