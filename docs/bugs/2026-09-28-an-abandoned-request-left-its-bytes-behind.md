# An abandoned request left its bytes behind

Date: 2026-09-28. Contracts: §4.10a (request/reply exchanges); RFC 9000 §3.1, §19.4 (`RESET_STREAM`);
CLAUDE.md "unbounded growth" ban. Found test-first by
`crates/transport/tests/exchanges.rs::an_abandoned_exchange_frees_everything_on_both_ends`.

## Symptom

A client abandoned a transfer partway through. The server's connection processed the reset and freed the
stream. The server's endpoint still held the partial request in its `arriving` map, and would forever:

```
5% random loss, seed 1: the server leaked: EndpointCensus { connection: ConnectionCensus { ... all zero ... },
exchanges: 0, arriving: 1, ready: 0 }
```

On a long-lived fleet session, that is one leaked entry per abandoned exchange.

## Root cause

`Endpoint::drain` moves arriving bytes out of the connection into `arriving`, and removes an entry only
when its request completes. A reset request never completes, so its entry was never removed.

## Fix

At the end of every drain, `arriving` keeps only streams whose receiving half the connection still has open
(`Connection::receiving`). The test passes for every hostile path and seed, with both ends quiescent.
