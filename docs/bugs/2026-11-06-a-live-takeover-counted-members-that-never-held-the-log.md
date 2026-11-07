# A live takeover counted members that never held the log (2026-11-06, A-240)

## Description

The phase one of a live-shipped volume's takeover (A-200) needs `f + 1` reports. It counted them among the hosts it
asked, which were the old placement's candidates plus the current placement's. A member placed there after the writes
(a replacement that joined, or a member the new placement ranks in) holds none of the log. Counted in place of a holder
of the log that could not answer, it let the successor adopt a head the log had passed:

- Acknowledged writes were lost (NoLoss).
- A superseded owner could make an entry durable outside the adopted head (StaleNeverCommits).
- A holder of the log was never fenced at the takeover's epoch.

Seen first on KIND. Run 42326682216 (`wan-loss` and `ceiling`) and run 42331191530 (`ceiling`) failed on "slates-1's
copy is fenced at 1 below the takeover's epoch 2". The replacement pod for the deleted owner had been counted, and a
holder of the log was never asked.

## Root cause

`slates_db::live::adopt` took any `f + 1` reports. The A-200 argument holds only when the `f + 1` reports come from the
hosts that hold the durable entries: an entry is durable on the owner and `f` of its `2f` holders, so `f + 1` of those
holders meet it. Nothing recorded which hosts those were. `fleet::materialize` passed the current placement, which
changes with membership.

## Impact

Safety. A takeover after a membership change could serve a volume without acknowledged writes. Agreement held (one
identity at each position), so the loss showed only as missing writes and as the low fence.

## The fix (A-240, option C)

- **The shipper records its holders.** Each shipper, a successor's too, records its epoch and holder set in the
  volume's catalog (`Op::LiveHoldersCommitted`, `PolicyRecord::live_holders`, `CatalogValue::live_holders`). It ships
  nothing until that catalog version is placed at `f + 1` (`live_ship::holders_placed`, gating `pending_sends`; at
  `f = 0` the owner's own commit places it, as for a head). A placement of the catalog ships at once
  (`live_ship::catalog_placed` from `fleet::record_acks_in`).
- **Phase one counts only recorded holders.** It counts its `f + 1` among the holders the adopted catalog records
  (`live_ship::fetch`'s `holders`, `slates_db::live::adopt`'s `holders`). Any member asked still reports, and the
  greatest head among all reports is adopted.
- **No fallback to the current placement.** When too few holders answer, phase one waits, counted as
  `fleet.live.takeover_waits_quorum`. A catalog that records no holders had a shipper that never shipped, so no log
  exists and the volume is served from its placed seal.
- **A successor keeps the recorded holders.** It copies the recorded holders into its own record
  (`verbs::materialize_taken_over`). A catalog it ships before its own shipper records still names the hosts that hold
  the log.

## Tests

- Model: `phase_one_meets_the_logs_holders_when_a_fresh_member_has_joined` in `crates/db/tests/live.rs`. The lost
  owner's replacement joins under a fresh identity before phase one, in half the takeovers, at f = 1 and f = 2, over
  200 seeds × 3,000 steps each.
  - Red before the fix: 10 of 400 seeds broke StaleNeverCommits.
  - After the fix: 0 broken seeds, with 218 members joined in the passing seeds.
  - The mutation stays as a test: `counting_any_candidate_lets_a_fresh_member_stand_in_for_a_holder` counts the
    first `f + 1` reachable candidates and must break (10 seeds).
- Processes: `a_member_that_never_held_the_log_never_stands_in_for_its_holder` in `crates/cli/tests/live_chaos.rs`,
  on a five-node fleet. Results below.

## Sibling sweep

The register takeover (`takeover.rs`, heads and catalogs) keeps each record's original candidate set and quorum
(the "Takeover placement retention" note of 2026-09-17, under §4.8), so it never had this defect. The live log was the
one place that counted against the current placement.
