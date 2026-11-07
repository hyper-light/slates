# A refused live takeover refetched its whole log every record period (2026-11-07)

## Description

When a successor's rebuild of a live-shipped volume from the adopted log was refused, the next record period ran
phase one again, fetched the same holder's whole log again, and was refused again, without end. In CI's five-node
fresh-member flow (diag-14 run 3, 2026-11-07), a successor outside the log's holders could not open the log
(`fleet.live.takeover_refused.open` 814). The holder it fetched from answered 389 fetches of its whole log in 40 s, and
the successor's session to it stayed lent. On an earlier tree the same loop moved about 4.2 GB (live-durability's
reading of the 1 MiB seal defect's runs). Banned item 8: an unbounded retry loop.

## Root cause

`fleet::materialize` retries a pending materialization every record period. A refusal at the rebuild
(`live_ship::archive_from_live`) left nothing behind, so the next attempt had no way to know that the same head would be
refused by the same bytes.

## Impact

Bandwidth and the holder's session, every period, for as long as the volume stayed unserved. Correctness was not
affected; the refusal stood each time.

## The fix

- **The control shard remembers the refusal.** It keeps the head each taken-over volume's rebuild was refused at, and
  why (`LiveState::refused_rebuilds`, recorded by `live_ship::note_rebuild` from `materialize`, forgotten when the volume
  is served).
- **The log is not refetched at that head.** Phase one still runs each period: it reports heads, a few bytes each, and
  raises fences. `live_ship::owes_refetch` fetches the log again only when phase one adopts another head, counting each
  skip as `fleet.live.takeover_waits_new_head`.
- **Which refusals count as final.** Those the log's bytes decide are final for a head: `open`, `record`, `apply`,
  `not_whole_first` and `layout`. The first payload's sealing, the records and the deltas are fixed by the chain
  identity. The lineage refusal (this node's own key record) is retried.

## Test

`live_ship::tests::a_rebuild_refused_at_a_head_is_not_fetched_again_until_the_head_moves`. It checks every refusal kind,
at the refused head and at a later one. Red-checked: with `owes_refetch` always true (the old behaviour), it fails on
`open` at the same head.
