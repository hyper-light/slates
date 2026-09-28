# A client request could wait for a timer after a lost doorbell

Date: 2026-09-28. Design: §4.7 ("Wake strategy": a client rings the doorbell only while its shard is
parked), §4.3 (the runtime's kick-if-parked rule). Found while investigating a CI stall (run 36404234656:
the first `create` after a daemon's start had no reply within 200 ms, on an x86 Linux runner). The defect
is proven below. Whether it caused that stall is **not** established.

## The defect

The client and the daemon's serve loop run a store-buffering handshake:

- **Client:** it writes its request into the command ring, then reads the shard's idle announcement
  (`daemon_parked`), and rings the doorbell only if it is set.
- **Shard:** it announces idle (`mark_parked(true)`), then idles until the doorbell or its poller wakes it.

Each side writes and then reads, so each needs a `SeqCst` fence between the two, or both reads can miss
(the C++20 fence–fence rule; `slates_rt::parking` states it). The code used `Release`/`Acquire` only.
The shard's only re-check of the client rings after announcing was the runtime poller, which runs before
the runtime's own park fence; the runtime's post-fence re-check sees only its own inboxes. So:

1. the client sees no announcement and does not ring;
2. the shard sees no request and idles;
3. the request waits for whatever wakes that shard next.

On a client's shard, the only periodic timer is the reap loop at the liveness cadence, so the wait could
last up to 1 s. x86 realizes this reordering through its store buffer; arm64 does not.

This was found once already. The 2026-09-13 record's sibling sweep
(`2026-09-13-parked-shard-loses-a-foreign-wake.md`) described this exact request-direction flaw and left
it "reported, not fixed", pending a failing test.

## Evidence

- **loom, the protocol as it stood** (`the_unfenced_protocol_loses_a_wake`, kept as a `should_panic`
  witness): `deadlock; threads = [(Id(0), Blocked), (Id(1), Terminated)]` at interleaving 1. The shard is
  idle with the request in its ring, and the client has exited without ringing.
- **loom, fenced** (`a_request_published_while_the_shard_goes_idle_is_never_lost`): 47 interleavings
  pass at the CI bound and 151 exhaustively. Some interleaving rang, some skipped the ring, and some idled.
- **Not reproduced in practice here**, and why:
  - The first `create`'s latency, alone: p50 0.16 ms, max 0.71 ms (30 runs, arm64).
  - Six concurrent copies of the client binary on arm64: p99 16.4 ms, max 30 ms (355 samples).
  - The same pre-fix binary built for x86_64 and run under Rosetta, whose TSO ordering includes store
    buffering: p99 13.6 ms, max 17.5 ms (360 samples).
  - A 20,000-request harness with jittered pauses, pre-fix and fixed, under Rosetta: p99 250 µs, no
    stall.
  - The practical window is narrow: a shard marked active spins, asking its pollers after it announces,
    which catches a request published during the spin.
- **The CI stall's cause is not established.** Ada's assessment (2026-09-28) is that resource limits on
  the CI runner (4 vCPUs, five daemon-starting tests at once) surface slowness this machine cannot
  reproduce. That fits every measurement above, and the derived 1 s deadline absorbs it.

## Fix

- `slates-ipc` `doorbell.rs` holds the two fences:
  - `after_publishing_idle_announced`: the client's half, called by `ClientEnd::send`;
  - `after_announcing_idle_pending`: the shard's half.
- The serve loop now announces idle, fences, and re-checks its client rings itself (`verbs::announce_idle`
  over `state::ring_ready_in`) before it idles. A request the re-check misses was published after the
  fence, so its client sees the announcement and rings. The ring wakes the shard through the runtime's
  own, already-sound kick protocol.
- The loom model drives those same fence functions, and CI's loom lane now runs `slates-ipc`.

## Found in the sweep

- **`mark_parked` discarded `set_parked` refusals** (`let _`), next to a stray `let _ = Ordering::Relaxed;`.
  A region whose announcement cannot be written is now counted (`ipc.idle_announce`), and the shard keeps
  polling rather than idle, since that client would never ring.
- **The handoff discarded a refused `Control::Active(true)`.** The shard then never spins, and each
  request pays a whole wake. It is now counted (`ACTIVATION_LOST`) and logged once.
