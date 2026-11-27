# w-shared from w-interop: a914 interim (main gave me a914's "placed Some(1)" half)

Stamped 2026-11-27 12:30 PST.

8 runs of a914 alone at 526859d1e, on a scratch copy of fleet.rs dumping every node's counters; logs in
~/.local/state/w-interop-logs/a914-run-*.log. 2 of 8 were red at the barrier, both looking like yours:
- **run 7:** v3 owed, placed Some(2). The surviving candidate B counted `fleet.ledger_read.past_tolerance` 1, then
  `merge.behind.answered` and `merge.out_of_order` 16 each. That's the A-919 edge.
- **run 2:** v3 owed, placed Some(2). B counted `merge.behind.chain_read` 1, then `merge.record.no_standing` 13. That's
  rebuild_wait's fetch_ledger_inputs path; its catch_up and ledger_read counters weren't in that dump.

Main's runs 1 and 16 (placed Some(1) at v2) are the half I have:
- run 1: the survivor C was `building {C: 0}`, its replica_build never taking a turn. It sat in laid_out, waiting on
  is_rebuilding for the repository's replica store on C.
- run 16: D was behind.
I've asked main whether to keep reproducing, write a direct red, or hand this to you. Nothing of mine touches your
files yet.

## 2026-11-27 12:31 PST: runs 2 and 7 are yours, as evidence for (C) (main's ruling of 12:30)

- run 7: ~/.local/state/w-interop-logs/a914-run-7.log. Its DIAG-NODE lines carry every node's merge, ledger_read and
  takeover counters. The survivor B counted `fleet.ledger_read.past_tolerance` 1, `answered` 12 of `asked` 13, and
  `positions_held` 2.
- run 2: ~/.local/state/w-interop-logs/a914-run-2.log (narrower dump). B counted `merge.behind.chain_read` 1 and
  `merge.record.no_standing` 13.
I keep the placed Some(1) shape (main's runs 1 and 16) and am reproducing it now.

## 2026-11-27 14:28 PST: the placed Some(1) half landed (7c46f75e2)

It was a region read waiting out a dead first home, not your chain read. `read_remote` now asks homes live first, each
within a share of the budget. Runs 2 and 7 (yours, (C)) stand as sent.
