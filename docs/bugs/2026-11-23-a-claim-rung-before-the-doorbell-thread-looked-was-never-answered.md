# A claim rung before the doorbell thread looked was never answered (2026-11-23)

## Description
CI's step "The Shards hand-off through the CLI (Windows, every push to main)" (`cargo test -p slates-cli --test shards`)
ran past its 20 minutes in run 45163517212 (4c8b391ec) and passed in 1.12 s in run 45176536354. A diagnostics branch
ran the file as CI does, each run bounded at 90 s: it hung in run 3 of 3 (diag-measurer-w-win-1), run 1
(diag-measurer-w-win-2) and run 2 (diag-measurer-w-win-3), each time in a different test. In the last two, a
`slates --instance cd… volume list` the test runs while its anchor's daemon warms up (`start_anchor`) had not returned
90 s after it started, and its daemon had printed `serving`.

Every thread's stack at the hang (cdb, attached without stopping anything, diag-measurer-w-win-3):
- the client: `Event::wait_or_exit` < `Claim::wait_on_holder` < `Claim::wait` < `connect_as` < `Client::connect`, a
  claim waiting for its answer, or for its holder's exit, while the anchor lived;
- the daemon: both shards parked in their completion port (`IocpDriver::dequeue`), and the doorbell thread waiting
  for a ring (`Event::wait` < `Bell::wait` < `doorbell::run`).

A claim made, and everything waiting for something that would never come.

## Root cause
A claim marks the control shard pending in the bootstrap object and rings the daemon-wide doorbell (A-381). The
doorbell thread (`crates/server/src/doorbell.rs`, `run`) read the doorbell word once as it started (`seen`) and took
the pending marks only when a later wait returned a word other than `seen`. A claim rung before that first read had
already moved the word, so no wait ever differed from `seen` on its account, its mark was never taken, and the
control shard was never told. Its claimant waits while the rendezvous's holder lives (A-468), with no bound, so it
waited for good.

The window is the daemon's own start: it publishes its rendezvous (the start stamp) before its doorbell thread runs
its first line, and a client that sees the stamp claims at once. Under an anchor the window is wider. The anchor's
object outlives its daemons, so a claim rung while a killed daemon restarts lands before the next daemon's doorbell
thread exists, which is what the Shards resync and DAX tests do. macOS runs the same thread, so the race is there too;
only Windows CI's Shards tests met it.

## Impact
A client that claimed in the window between a daemon's publishing and its doorbell thread's first look waited for
good under an anchor: CI's Shards CLI step hung for 20 minutes, and a CLI verb or SDK connect could do the same to a
user at a daemon's start or restart.

## Exact edits
- `crates/server/src/doorbell.rs`: `run` takes the pending marks once after its first read of the word, before its
  first wait, so a claim or a ring that landed before it looked is acted on.
- `crates/server/src/doorbell.rs` tests: `a_claim_rung_before_the_doorbell_thread_starts_is_taken` (macOS and
  Windows): a claim made, then the thread started; its claim flag must be set within the wait's bound.
