# A refused claim dropped the clients its accept round had already accepted

- **Date:** 2026-10-25
- **Status:** fixed 2026-10-25, in the same change as this record (A-176).
- **Subsystem:** the client rendezvous (§4.7): `crates/ipc/src/rendezvous.rs` (`Listener::accept_pending` and each
  platform's `accept_one`) and its caller, the accept loop in `crates/server/src/daemon.rs`.
- **Severity:** a client the daemon had accepted and answered was dropped unserved. Its claim read `READY` (macOS,
  Windows) or its handoff (Linux), so it believed it was connected, but the daemon had dropped its region and, on Linux,
  its end of the control socket. Meanwhile its id stayed reserved in the daemon's live set until a restart. Any claim
  refused for a reason other than the client bound or another uid caused it: a region that could not be created (as
  Windows refused a reconnecting client's region, the sibling below), a peer whose socket refused mid-handoff, or a
  client that hung up before its hello was read.
- **Exposure:** every platform, whenever two or more claims were pending in one round and a later one was refused.
  Present since the round was written (Phase 2); the round's error handling of 2026-09-14 counted and logged the error
  but kept dropping the round.
- **Found by:** reading the round while diagnosing the Windows reconnect
  (`docs/bugs/2026-10-25-a-windows-client-could-not-reconnect-after-a-restart.md`), then a failing test.

## Symptom

`crates/ipc/tests/rendezvous.rs`, `a_refused_claim_does_not_cost_its_round_the_client_it_accepted`, on this Mac before
the fix. Two claims are pending, and the round's second region is refused:

```
a refused claim does not fail the round that refused it: OsRefused { call: "the test refuses every region after the first", code: None }
```

The round returned the second claim's refusal, and the first client, which it had already accepted, went with it.

## Root cause

`accept_pending` collected the accepted clients in a vector and returned `Err` on the first failure other than the
client bound or another uid. A failure there is one claim's alone: the platform's `accept_one` has already answered
that claimant with its refusal. Returning `Err` dropped the vector, and with it every client the round had accepted
before.

## Fix

`accept_one` now reports each claim it serves as `Served::Accepted` or `Served::Refused`. Only a failure of the
rendezvous itself is an `Err`: an `accept` the kernel refused, or a claim table that could not be read. `accept_pending`
hands each refusal to a callback the caller passes and goes on to the next pending claim. It returns a `Round`: every
client it accepted, and the failure that ended it early, if one did. The daemon's accept loop gives a `HandoffLost`'s
id back as before, counts the other refusals in `CLAIMS_REFUSED`, and keeps `ACCEPTS_FAILED` for rounds the rendezvous
ended. Each logs its first occurrence.

## Verification

The test fails before the fix on this Mac and passes after it on this Mac, on fedora and on jupiter. Red-checked on
fedora, whose Linux path through `serve` is the other platform arm: with the old round restored (a refusal ending the
round and dropping what it accepted), the test fails again. The rendezvous suite passes whole on each: 11 tests on this
Mac (one of the 12 is Linux's and Windows' alone), 12 on fedora, 12 on jupiter.
