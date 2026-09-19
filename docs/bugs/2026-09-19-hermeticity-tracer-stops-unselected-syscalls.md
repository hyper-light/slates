# Hermeticity tracing prevented the first heartbeat

## Failure

[Job 105965681080](https://github.com/hyper-light/slates/actions/runs/35468726110/job/105965681080)
at `49597a4` exhausted its 60-second readiness budget on 2026-09-19. The anchor recorded
55 first-heartbeat kills. The previous shared-clock fix did not resolve this failure.

## Diagnosis

The ordinary `strace -f -e trace=...` filter controls output, but still stops tracees at every
syscall. The omitted calls matter: the full trace showed the two shard threads performing
13,756 and 14,351 `mprotect` calls before their first kill. The allocator was extending its
heap while preallocating the lease timer wheel. This machine's derived volume capacity was
1,145,554 per shard; `Wheel::new` reserves all of its 64-entry segments up front. Temporary
boundary logs located the delay in partition construction, before the heartbeat loop starts.
The level-triggered rendezvous doorbell also repeatedly kicks the waiting shards.

Reproduction: a disposable Linux arm64 `rust:1.98` container, default Docker security policy,
strace 6.13, debug CLI, `slates --instance ci-trace-<pid> anchor --quick --shards 2`, with a
supervising Python process trying `volume list` for eight seconds and stopping/reaping its
process group. The trace filter is exactly `STRACE_TRACE` from the conformance harness.
Logs and traces were in `/dev/shm`; no kernel mount or additional container privilege was used.

| Tracer | Ready | First-heartbeat kills |
| --- | --- | --- |
| None | 0.208 s | 0 |
| Original filtered output | 2.282 s | 1 |
| Original filter, diagnostic boundary logs | No, eight-second bound exhausted | 8 |
| Same filter and binary, `--seccomp-bpf` | 0.204 s | 0 |

These are diagnostic samples, not a performance baseline. The full-syscall diagnostic run
also reproduced eight kills; its large trace was reduced to counts and deleted before further
sampling. Diagnostic logs are removed from the implementation.

## Fix and evidence

Use `strace --seccomp-bpf -f` with the **same** `STRACE_TRACE` selection: only selected
syscalls incur tracing stops. Keep explicit `--kill-on-exit` so the harness owning the tracer
also ends its tracees, including when filter setup is unavailable. No daemon deadline, memory
policy, or filesystem-write classification changes.

The [strace 6.13 manual](https://github.com/strace/strace/blob/v6.13/doc/strace.1.in)
documents selective stops and tracee termination. A pre-existing seccomp policy may hide a
denied syscall, which cannot have performed the write; this gate judges completed effects,
not attempted operations. Strace also cannot see writes submitted through io_uring; this is
an existing evidence boundary, not a new guarantee from this repair.

Regression: start the real anchor through the harness's tracer prefix, obtain stable client
responses, and verify no first-heartbeat kill. Independently trace real file mutations in
RAM and verify that the unchanged parser still observes pathname and descriptor writes.

## Siblings

Lease-wheel eager allocation and repeated rendezvous kicks deserve separate bounded-allocation
and wake-coalescing repairs. The tracer fix does not claim those product costs are resolved.

## Validation

On 2026-09-19, with all diagnostic logging removed:

```sh
cargo build -p slates-cli
SLATES_TEST_BINARY=/work/target-linux/debug/slates cargo test -p xtask conformance:: -- --test-threads=1 --nocapture
cargo clippy -p xtask --all-targets -- -D warnings
cargo fmt --all -- --check
cargo xtask check
```

The Linux conformance tests pass, 7/7 in 2.51 s (the unrelated Vim history explicitly skips
without its RAM-directory environment). The startup/write-observation regression also passes
alone in 1.37 s. Clippy passes on Linux and macOS; formatting and the structural, literal,
unsafe-budget and version checks pass. The CI conformance job now runs both targeted regressions
before its mounted suites. Full mounted conformance remains pending; these local tests need
no privileged mount.


## Follow-up: the tracer filter alone did not close CI startup (2026-09-19)

Job 105969406090 still failed after this change. Its 60-second log has 54 first-heartbeat
kills. The local filtered-tracer proof did not establish that the product could start under
CI's scheduling and memory geometry. The remaining eager timer allocations and level-triggered
rendezvous kick loop are now reproduced independently and corrected; see
`2026-09-19-startup-wakes-and-kick-retirement.md`. The unchanged conformance harness regressions
pass as uid 65534 in Linux 6.12.76-linuxkit (seven tests, 0.53 s; unrelated Vim case skipped).
That is local validation, not a fresh CI result or a full mounted conformance run.
