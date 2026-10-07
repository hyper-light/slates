# A cross-region read took one WAN round trip per window, and a slow batch could fail a live read

**Found:** 2026-10-07, on the two-network Docker topology (`docs/wip/bench/multiregion/run.sh`: 100 ms ± 40 ms one
way, 3 % loss each way). **Status: fixed for correctness and robustness. Throughput on the jitter link is bound by
the session's congestion control, owed as condition 7's jitter-robust delay signal.**

## Description

Region 1's clients read an 8 MiB file whose owner is in region 0. The read is forwarded page by page to the
owner (§4.8 "Lookup").

- At `2a56388f` the reads failed. Each page was one round trip of 200–450 ms, and any page slowed by a loss
  overran the 1 s reply deadline (`GAPS.md` 2026-10-07, "the two-network rerun").
- Read-ahead windows fixed that (`RequestBody::ReadWindow`: the origin asks for a window of about 64 KiB and
  answers the client's later pages from it), and the forward's deadline gained the path's measured tail. The reads
  completed byte-identical, but took 127–148 s (five rounds, three readers). That is one window per forward, 131
  forwards per read, about 1 s each.

## Root causes

1. **One window per round trip.** Each window was its own request, with its own round trip and its own loss
   recovery. With 3 % loss, most 64 KiB windows lose a packet, and the last packet of a reply can only be
   recovered by a probe timeout, which the jitter makes long.
2. **The session's congestion window collapses under the jitter.** `congestion_bench` and `fetch_bench` show Copa
   holding a 17–24 KB window on this link shape (`BENCHMARKS.md`, "delay jitter collapses Copa", 2026-10-07). At
   200 ms that is about 65–100 KB/s, which is what the reads achieved.
3. **(Found while fixing 1.) A batch deadline estimated from a measured rate refused live transfers.** The first
   batched version gave each batch a deadline of one liveness budget plus its estimated transfer time. When the
   estimate ran fast, the batch was abandoned mid-transfer, and the client's read failed `HomedElsewhere` after
   2.6–13 s on a path that was delivering.

## Fixes

- **Batches that double** (`crates/server/src/verbs.rs`, `next_batch`, `window_for`, `keep_window`, `join_batch`).
  A read that continues where its window ended fetches the next windows as a batch sent at once on the owner's
  session (`slates_cluster::requests_within`), twice the last batch, the read-ahead growth Linux gives a
  sequential reader. A batch is capped by:
  - what is left of the file;
  - the shard's read-ahead allowance (`DaemonConfig::read_ahead_bytes`, one session's receive bytes per peer,
    counted inside the fleet's receive share of the reserve; charged before the fetch, credited when the window
    goes, and credited with the client if a reply is never carried home);
  - once a rate is measured, what the path moves in one liveness budget, so a batch holds the owner's session no
    longer than a forward is already allowed to.

  The joined windows must abut and read one state of the file (one stamp and length). The rest is dropped and
  fetched when the read gets there.
- **The rate is timed between deliveries.** The session's consumed bytes are counted between the first turn
  that consumed any and the last (`Endpoint::bytes_consumed`; BBR's delivery-rate sample). Two earlier cuts were
  measured and rejected, both on the jitter link:
  - The origin's congestion window. It is the wrong direction: the origin sends only requests, so its window
    stays near the initial 12 KB and capped every batch at one window (0 windows joined in 25 forwards).
  - The whole fetch's bytes over its elapsed time, with and without the round trip subtracted. A one-window
    fetch is mostly fixed cost (round trip, first-byte wait, loss recovery), so the rate read low and capped the
    next batch at one window again (2 joined in 70 forwards).
- **A batch is given up only when it stalls.** No byte consumed and no reply completed for one budget (the
  liveness budget plus the path's tail). It is never cut at a deadline estimated from its size. This is the
  record dispatch's progress-aware rule, applied at the byte.

## Tests

- `a_cross_region_read_is_served_from_read_ahead_windows_and_byte_identical` (fleet suite): a 4 MiB read across
  regions is byte-identical, windows are joined (the batch path's non-vacuity counter), and the read takes at
  most `log2(pages) + 1` forwards. Measured: 7 forwards for 1,041 pages, where one window per forward took 66.
- Unit tests in `verbs.rs`:
  - `a_batch_joins_in_order_only_while_each_window_abuts_and_reads_one_state`;
  - `a_continuing_read_doubles_its_batch_within_the_file_and_the_measured_rate`;
  - `a_batch_delivery_rate_is_its_streamed_bytes_over_their_interval`;
  - `the_read_ahead_ledger_refuses_past_its_bound_and_never_credits_below_zero`.

## Measured on the two networks (2026-10-07, this Mac, Docker Desktop, 8 MiB, three readers in region 1)

Command: `docker build -t slates:mr .`, then `sh docs/wip/bench/multiregion/reads.sh SCRATCH PAYLOAD 3`.

- The script waits until cross-region membership has formed, writes the 8 MiB payload on `a1`, and times three
  whole reads on each node.
- The jitter-free rows add `JITTER=0ms`.
- The one-window arm is the same tree with `next_batch` pinned to one window, built from a scratch worktree as
  `slates:mr-single` (`IMAGE=slates:mr-single`).

Load average 2–7 throughout. All N shown.

| Link | Arm | Reads (all byte-identical) | Forwards per read |
|---|---|---|---|
| 100 ms ± 40 ms, 3 % loss | before batching (one window per forward) | 127.5–148.1 s (15 of 15) | 131 |
| 100 ms ± 40 ms, 3 % loss | batches | 124.9, 130.6, 132.5, 134.3, 135.2, 142.2, 142.9, 143.1, 144.9 s (9 of 9) | 127 (11 windows joined in all) |
| 100 ms, 3 % loss, no jitter | one window per forward | 82.3, 83.5, 84.0, 84.9, 85.0, 85.9, 86.6, 86.6, 87.1 s (9 of 9) | 131 |
| 100 ms, 3 % loss, no jitter | batches | 28.8, 29.0, 29.1, 29.3, 29.6, 30.0, 32.3, 33.7, 33.9 s (9 of 9) | 29 (307 windows joined) |

- **Without jitter, batches read 8 MiB 2.6–2.9× faster than one window per forward.** The batches settle at 4–7
  windows, about 1 s each, which is the cap holding a batch to one liveness budget at the measured rate. That is
  about 300 KB/s.
- **With jitter the two are even.** The measured rate is 65–100 KB/s, a window per second or less, so the cap
  keeps every batch at one window. Here the session's congestion window, not the read path, sets the time.
- Every batch ended: begins equal ends on every reader (86–393 each). In an earlier jitter-free run of this image,
  before the traces were added, `b2`'s record session to the owner stayed borrowed and its reads failed
  `HomedElsewhere` after 48–69 s (`fleet.forward.session_never_returned`). That run's `b2` replaced its sessions 111
  times (its peers about 20) and had three region-0 detector pairs unconfigured. The traced run shows the batch path
  always returns its session, so that borrow came from somewhere else; it is recorded as open below.

## Owed

- **Open: a record session to the owner stayed borrowed on one node** (the jitter-free run above, `b2`), alongside
  cross-region session churn. Evidence is kept in the scratch run directory `mr-j0-fail/` (every node's log and
  status). It needs a trace of who borrows a record session, and for how long, before any diagnosis.
- **Open: cross-region membership sometimes never forms.** In 2 of 12 bring-ups this day, every node's detector held
  only its own region 45 s after `up`, and stayed so after bootstrap (`mr-noform/`, `mr-noform2/`). `reads.sh`
  waits for formation, so the timings above are from formed topologies.
- Condition 7: a jitter-robust delay signal for the session's congestion control. On the jitter link it, not
  the read path, bounds a cross-region read.
- Reads of a snapshot (immutable) could be served from the reader's own region's mirror holders and never cross
  the WAN. Reads of the head must reach the owner.
