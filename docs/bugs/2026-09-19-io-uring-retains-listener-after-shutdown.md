# io_uring retained a listener after runtime shutdown

Date: 2026-09-19. Area: runtime driver retirement, §4.3 / §4.7, T-2.14.

## Failure

[Linux CI job 105974090622](https://github.com/hyper-light/slates/actions/runs/35471844854/job/105974090622)
failed on `bf0e690` in
`a_warm_restart_preserves_both_groups_votes_before_their_replies_escape`.
The second daemon's rendezvous bind returned `EADDRINUSE` after `first.stop()`.
The failure preceded the recovered-vote assertions.

The earlier local checks used Docker's default seccomp policy, which refused
`io_uring_setup` with `EPERM`; the runtime selected epoll. They did not cover
the Linux driver that retained this listener.

## Reproduction and root cause

The authorized disposable container allows just `io_uring_setup`,
`io_uring_enter`, and `io_uring_register` in addition to Docker's default
profile. Linux 6.12.76-linuxkit on aarch64 selected
`SINGLE_ISSUER | DEFER_TASKRUN`, confirmed in the runtime's probe notes.

The unchanged warm-restart test passed 100 isolated runs. The smaller regression
`shutdown_releases_a_listener_with_an_armed_readiness_wait` failed with the
same `EADDRINUSE` on the first subsequent trial (its initial isolated run passed).
It arms an idle abstract listener, waits for registration, shuts down and joins
the runtime, then immediately binds the same process-specific address. No
client connects, no other process owns the fixture, and no Raft state is involved.

Closing the ring is not a completion barrier. Linux's
[`io_uring_release` and `io_ring_ctx_wait_and_kill`](https://github.com/torvalds/linux/blob/v6.12/io_uring/io_uring.c)
queue cleanup to a worker. Pending poll requests retain their file descriptions
until that cleanup completes, even after userspace closes the descriptors.
Counting open descriptors cannot detect these remaining kernel references.

## Fix

Before closing a driver, cancel its pending requests and submit a drain no-op.
Wait for that no-op's completion so all earlier requests have completed before
retirement returns. Teardown owns the ring exclusively and waits only for its
already-submitted polls and no-ops; it never waits for a new client or retries a
bind. A teardown refusal remains a typed runtime error and is reported at drop,
whose interface cannot return it.

The boot probe verifies synchronous cancellation support on the empty ring.
An unsupported operation refuses io_uring selection through the existing driver
probe, before any socket is registered. No runtime operation uses a second
implementation. The synchronous cancellation API is available since Linux 6.0;
`SINGLE_ISSUER | DEFER_TASKRUN` still requires 6.1.

Add both a threaded shutdown regression and a calling-thread retirement case.
The latter prevents thread-exit cleanup from hiding delayed driver retirement.
Keep the original warm-vote assertions unchanged.

## Validation

On 2026-09-19 local / 2026-09-20 UTC, the authorized four-CPU, 4 GiB disposable container
(Rust 1.98.0, Debian 13, aarch64 Linux 6.12.76-linuxkit) passes:

- Twenty consecutive `cargo test -p slates-rt --test reclaim -- --nocapture` runs:
  five tests each, zero failures; each verifies `SINGLE_ISSUER | DEFER_TASKRUN`.
- Twenty consecutive runs of the original warm-voter vote-preservation regression:
  zero failures, typically 0.11 s each.
- The same five reclamation tests under Docker's default seccomp policy, with
  `SLATES_TEST_DRIVER=epoll`, pass in 0.07 s; the test proves the selected backend.
- `cargo test --workspace` with `SLATES_TEST_DRIVER=io_uring`: 1,506 passed,
  zero failed, 14 ignored, including all 49 fleet histories.
- Strict Linux workspace/all-target Clippy and `cargo xtask check` pass.

Commands and output: `/private/tmp/slates-run-final-linux-checks.sh`,
`/private/tmp/slates-final-linux-checks.log`, `/private/tmp/slates-epoll-reclaim-final.log`,
`/private/tmp/slates-run-linux-workspace-current.sh`,
`/private/tmp/slates-linux-workspace-current.log`. Each run is serial and time-bounded;
there were no concurrent builds during the repeated histories. The two removed duplicate
SQ-push unsafe sites reduce the runtime's actual count from 59 to 57.
