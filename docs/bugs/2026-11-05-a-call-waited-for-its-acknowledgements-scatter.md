# A call waited for its acknowledgement's scatter (2026-11-05, §4.9, R9; the first fix reverted, fixed by A-216)

## Description

Every `ack_every` replies (half the ring's slots), the synchronous `Client::call` (`crates/client/src/client.rs`)
acknowledged them before sending its own request, and waited for that acknowledgement's reply. The daemon answers an
acknowledgement only after every shard has released the client's records (`verbs::scatter_acknowledge`, a scatter and
a gather), so the call that carried it paid every shard's turn first. The async path had always sent the same
acknowledgement without waiting (`Client::begin_ack_if_due`).

## Measurement

- A variant of `provision_bench` (not kept) split the single-client creates by whether the client acknowledged just
  before them (macOS arm64, 2026-11-05 07:25 local, after a load of 30–46; the daemon's boot probe measured a 2.5 ms
  wake that run, so the absolutes are inflated): **133 of 2,000 creates (6.7%) carried an acknowledgement, median
  2,497 µs, against 169 µs for the other 1,867**. At 6.7% of the samples, they set the p99.
- `crates/client/tests/acknowledge.rs` (`a_call_is_never_held_behind_the_periodic_acknowledgement`): with the other
  shard of a two-shard daemon held for one liveness budget, the status call that carried the acknowledgement **failed
  `Stalled { after_ns: 1000000000 }`**, though everything it asked for was on the client's own shard.

## Root cause

The synchronous path waited for a reply that carries no information the caller needs. The acknowledgement's watermark
covers every earlier one, so a lost or late reply only delays the daemon's release of records until the next.

## Impact

Every synchronous client (the CLI, the MCP server's client, the SDKs' sync calls and `provision_bench`): one call in
every `ack_every` waited for the slowest shard. On an idle daemon whose other shards park, that was a wake on each
of them, and so the single-client provisioning p99 sat at many times its p50 (jupiter: p50 20 µs, p99 80–650 µs). A
shard busy past the reply deadline made an unrelated call fail.

## Correction (2026-11-05, later the same day)

The macOS arms below were one binary: built from `git archive` copies into one shared target dir, cargo rebuilt nothing
after the first (main c0c8d28c), since the archives' files carry their commits' old times. So the macOS stall counted
against a6c3bc16 here was main's, and so was the instrumented bench's caught pause. a6c3bc16's own record is 1 stall in
9 runs on jupiter, where each arm has its own target dir. Main stalled 1 of its 4 parked forms on macOS. The
`Stalled` under load fits a defect of main's found later: a scatter whose part's spawn a full control channel refuses
never answers (recorded with A-216).

## Edits, then reverted

a6c3bc16 made `Client::call` send a due acknowledgement with `begin_ack_if_due`, as the async path does. Its reply
was dropped on arrival and counted (`unawaited_dropped`), and the watermark rule that holds an unpublished id's record
back was unchanged.

`a_call_is_never_held_behind_the_periodic_acknowledgement` (`crates/client/tests/acknowledge.rs`) failed before it
(`Stalled` after 1 s) and passed after (3.62 s, macOS arm64), and the whole `slates-client` package passed.

**Then the interleaved `provision_bench` A/B found a stall.** With a6c3bc16, a parked-form create (one idle client,
paced a millisecond apart) went unanswered past the 1 s reply deadline:
- on jupiter, in 1 of 3 runs (15:45 UTC, the processor 9% busy);
- on macOS, in 1 of 3 runs (15:51 UTC, load 8–14).

Without it, every parked form that ran completed: 12 on jupiter and 3 on macOS that day. The change was taken back
out by a new commit, and the test with it, since the test asserts the change's behaviour. Its source is kept to
return failing-first with the real fix.

What is known of the stall: the daemon serves every ready request of a client in a round (`serve_client`, no
in-flight limit), and it publishes replies in completion order, which the client takes in that order. So the
unawaited acknowledgement's reply cannot hold the create's reply back by position. The difference the change makes is
two of the client's requests in flight at once, the acknowledgement's scatter spawned on every other shard, all parked
in this form, while the create (or its forward) runs. Not found yet.

What later runs showed (macOS arm64, 2026-11-05 16:00–16:57 UTC):
- **It reproduces on main's own client code.** A test (kept out of the tree until its fix, since it fails only
  intermittently) put two replies in flight on the sync client without a6c3bc16. It begun an acknowledgement and
  abandoned it, then called a create, paced 1 ms apart, on a daemon derived for the machine.
  - Alone, it never stalled: 2,000 creates, twice.
  - After a prelude of 64 concurrent clients, each dropped with an acknowledgement still in flight, it stalled in 1
    run of 10: 3 creates, numbers 1,713, 1,715 and 1,716.
- **The one stall caught with the bench instrumented** (a stall reported instead of aborted; its client library was
  main's, as the correction above says) was
  not in the parked form. All 64 clients of the 64-client form's second run stalled at once, at their 7th to 10th
  create, each after about 1.767 s. The daemon answered nobody for that long, right after 64 clients were dropped
  and 64 others connected. So the stall is a whole-daemon pause after client churn, and the parked form only met it.
- It came at load 8–14 and did not come in 10 instrumented runs at load 4–8. The pulse dump that would name the
  shard and its longest step has not caught one yet.

## Also found

At load 23–36 on macOS the bench's 8- and 64-client forms aborted `Stalled` in every arm, main included: a live
daemon slower than the reply deadline. The client returns `Stalled` for a live daemon once the deadline passes
(`Client::round_trip`). That is a separate robustness gap.

## Fixed by A-216

The supervisor chose to answer the acknowledgement once the client's own shard has released its records, with the
other shards' releases run as tracked tasks (A-216, §4.9's status). The client keeps one reply in flight.

Exactly-once holds:
- a client's own shard advances its window before the reply, and every request passes that window first;
- a client's sequences only grow and never wrap;
- a new client never takes a used id;
- a later request to another shard queues behind the release on that shard's control channel.

Tests, each failing first:
- `a_call_is_never_held_behind_the_periodic_acknowledgement` (`crates/client/tests/acknowledge.rs`): `Stalled` before;
  after, the call returns in under half a 1 s hold, with no second reply in flight.
- `a_lagging_release_is_never_met_by_a_request_as_new` (`crates/server/tests/ack_release.rs`): the acknowledgement
  took 1.197 s before. After, it and a retry inside the hold are answered at once (the retry `DuplicateRequest` by the
  client's own shard), and a new create makes a new volume. Red-checked: with the own shard's release skipped, the
  retry waited 1.19 s.
- `a_refused_release_is_counted_and_the_next_watermark_takes_it_over`: never answered before (a sibling defect,
  `docs/bugs/2026-11-05-a-scatter-whose-part-could-not-be-sent-never-answered.md`). After, it is answered, the refusal is
  counted, `AcknowledgementOwed` is met at the bound, and the next watermark serves again. Red-checked: with the count
  removed, 0 before and 0 after.
