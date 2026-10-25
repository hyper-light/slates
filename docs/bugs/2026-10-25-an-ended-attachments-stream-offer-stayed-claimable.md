# An ended attachment's stream offer stayed claimable

- **Date:** 2026-10-25
- **Status:** fixed 2026-10-25, in the same change as this record.
- **Subsystem:** the Shards stream's socket route on macOS (§4.6; A-161, A-173): `crates/server/src/shards.rs`
  (`end_stream`, `ended`) and `crates/server/src/shards_route.rs` (the offers).
- **Severity:** a claim after its attachment had ended was handed a stream end whose other side the daemon had already
  closed, where it should have been refused. No bytes of anything were reachable through it: the end reads end of file.
  Until its expiry (two liveness budgets) the dead end also kept a slot of the shard's handoff bound.
- **Exposure:** macOS only; Linux sends the end on the control socket before the attach's reply, and Windows refuses
  the form. Present since A-161's route (2026-10-24).
- **Found by:** the sibling sweep after `docs/bugs/2026-10-25-a-released-maps-offer-stayed-claimable.md` (an offer that
  outlived its copy), then a failing test.

## Symptom

`crates/server/tests/shards.rs`, `route::an_attachment_ended_before_its_claim_withdraws_its_offer`, on this Mac before
the fix: attach in the Shards form, detach, then claim the offer with its own secret.

```
the ended attachment's offer was still claimable: Ok(OwnedFd { fd: 23 })
```

## Root cause

An attach in the Shards form keeps the client's end as an offer, dropped only by its claim or its expiry. A detach, a
revocation or the volume's destroy ended the daemon's side of the stream (`end_stream`) and then the attachment
(`ended`), but neither touched the offer, so it stayed claimable until it expired.

## Fix

`withdraw_offer` takes the attachment's unclaimed offer out of the route, counted `shards.route_withdrawn`, at both ends
of an attachment: in `end_stream`, before the ending's reply, so no claim made after it can take the end; and in
`ended`, for every other way a stream ends.

## Verification

The test fails before the fix and passes after on this Mac; the route's other four tests and both Shards suites pass
(server 26 of 26, client 3 of 3), the expiry test among them (an offer that expired first leaves nothing to withdraw).
