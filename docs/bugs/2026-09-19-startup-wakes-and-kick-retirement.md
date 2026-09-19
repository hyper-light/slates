# Startup wake amplification and kick retirement

## Evidence and impact

The Ubuntu conformance job [105969406090](https://github.com/hyper-light/slates/actions/runs/35470114220/job/105969406090)
at `e882683` failed the traced-startup regression on 2026-09-19. Six tests passed;
startup exhausted its 60-second budget. The captured log contains 54 first-heartbeat
kills and derives 1,617,130 volume slots per shard. The syscall filter emits no
installation warning. The earlier Linux trace recorded thousands of eventfd writes
while a connection waited for acceptance.

## Causes

`doorbell::run` polls the Linux listener with level-triggered readiness. Until the
control shard accepts the connection, every poll returns immediately and writes
every shard's eventfd again. Tracing these writes amplifies the startup delay;
the loop itself also burns CPU without tracing.

Separately, a lease wheel reserves one allocation per 64 possible timers at boot,
even with no leases. The wheel's slot count is its admission bound, not the
number of active leases.

`KickFd::with` checks a closed flag before borrowing an `UnsafeCell` containing
the descriptor. Unregistration can close it after that check and before the
borrow finishes. Joining the shard does not join foreign callers. A copied
reference can also outlive the registry entry on slot reuse.

## Repair and validation

Use the control shard's existing one-shot readiness driver for the Linux
rendezvous. Drain pending connections before rearming; a connection that arrives
between draining and registration remains readable. Other platforms retain
their shared-memory doorbell wait. No Linux listener-watching thread is needed.

The fixed-capacity timer slab reserves one backing segment, sized to its exact
admission bound. Its power-of-two index stride does not round up the backing
allocation; entries are initialized on insertion. The allocator regression
reported 25,271 calls before and four after at CI's 1,617,130-timer bound.
A 65-timer wheel also takes four calls. Filling a non-power-of-two wheel,
renewing every entry, refusing stale cancellations and overflow, and expiring
all entries make zero additional allocator calls. The wheel's admission bound
and cancellation/expiry rules are unchanged.

Unix kicks now carry `SlotHolder`, not references into registry entries.
The entry owns an immutable `OwnedFd`. `KickFd::with` pins the matching
registration using the existing registry reader count. Unregistration removes
the pointer, waits for readers, drops the entry, then publishes a free slot.
A copied kick cannot resolve a reused slot. Both unsupported unsafe `Send`/`Sync`
implementations and the `UnsafeCell` are removed. Simulated kicks use the same
generational discipline; simulation shutdown keeps pair rings until every
context's cancellation has finished. Reader pins release on unwind too.

## Local regressions

On 2026-09-19:

- `cargo test -p slates-rt --test timer_allocations -- --nocapture`: red before,
  green after, the allocation counts above (macOS and Linux).
- `cargo test -p slates-rt --lib retirement_waits_for_a_foreign_kick_borrow`:
  a channel holds a foreign borrow across retirement. The old implementation
  closes during the borrow and fails; the new implementation waits and passes.
- `cargo test -p slates-server --test daemon a_pending_rendezvous_does_not_repeatedly_kick_the_shards`:
  both real Linux shards are held, their admission kick drained, and one real
  connection queued. The old listener watcher wakes an unrelated held shard
  (failure in 1.13 s); the replacement passes in 0.22 s, then accepts clients
  through subsequent readiness registrations and shuts down. This exercises
  the defect without depending on CI's scheduler or tracer slowdown.

The original traced-startup regression and first-heartbeat assertion remain.
The local Linux baseline passed in 1.40 s, so that run does **not** reproduce
CI's 60-second startup failure. The forced delayed-accept test and allocator
counter establish the product defects independently. Full conformance and a
fresh CI result must be reported separately from these local regressions.

Sibling review: Windows still carries a raw IOCP handle in `Kick::Iocp`, while
the driver owns its close; this change establishes the Unix descriptor and
simulation lifetimes only. Windows port ownership needs its own by-use test.


## Final verification

Linux 6.12.76-linuxkit arm64, Rust 1.98.0, 2026-09-19:

```sh
timeout 120s cargo test -p slates-mem -p slates-rt -p slates-db --tests -- --test-threads=1
cargo test -p slates-server --test daemon -- --test-threads=1
```

Together: 180 passed across 25 test binaries, zero failed or ignored. The server's ten
integration cases finish in 3.72 s. The Linux conformance harness's seven cases pass as uid
65534 in 0.53 s, including traced startup and write observation; the unrelated Vim case
prints its RAM-directory skip. The real FUSE coherence mount passes separately as that
ordinary user (0.13 s). On macOS, the two simulation cases in
`cargo +nightly miri test -p slates-rt --test differential` pass (two OS cases intentionally
ignored by that suite). `cargo xtask check` passes, with unsafe ceilings tightened from
67 to 59 in rt and five to four in server.

Strict clippy passes for the affected crates on macOS and Linux, and for rt's
Windows targets with `slates-machine/pure-hash` (compilation only). The full Linux
FUSE suite reports 69 passed and one ignored; the OCI Docker case additionally
prints its missing-runtime skip. That skip is not a container-bind proof.
