# The io_uring driver carried no socket readiness, so async sockets died the first time they awaited it

Date: 2026-09-16
Area: `crates/rt/src/uring.rs` (`register_readable`, `register_writable`)
Severity: on any Linux host where the runtime selects io_uring — a bare-metal box, a VM, a CI runner
without a seccomp filter — every async socket on the runtime died the first time it had to await
readiness. That is the NFS-mount server's per-connection reads and writes (§4.6) and the fleet's
UDP transport (§4.10a). It hid because the containers the fleet was tested in (Docker, KIND under
`seccompProfile: RuntimeDefault`) block the io_uring syscalls, so the runtime fell back to epoll,
which does carry readiness (its own `EEXIST` bug fixed 2026-09-14).

## Symptom

The `crates/bridge-nfs/tests/async_loopback.rs` test — a client mounts and reads a file over the
async NFS server on the runtime — failed on the GitHub `ubuntu-latest` gates lane, and only there:
green on macOS (kqueue), green in a Docker container (epoll, io_uring blocked by seccomp). The
client panicked at a varying point — its write of the request body one run, its read of the reply
the next — with `BrokenPipe` / `UnexpectedEof`: the server reset the connection early and
non-deterministically.

Reproduced 100% by running the test binary under io_uring (`docker run --security-opt
seccomp=unconfined`, `io_uring_disabled = 0`): 80 of 80, then 100 of 100. Under the default seccomp
profile (epoll) the same binary passed 60 of 60 under CPU stress and 120 of 120 concurrent — the
driver, not the test, was the variable.

## Root cause

`UringDriver::register_readable` and `register_writable` were unimplemented: each returned
`Err(RtError::DriverRefused { call: "register_readable", .. })` with an "Owed (§4.10a)" note. The
readiness future (`crate::readiness::Ready`) arms interest through the current shard's driver on its
first poll; when the driver refuses, the future returns that error, and the caller's `read`/`write`/
`accept` — which awaits readiness only when the non-blocking syscall returns `EAGAIN` — returns the
error and its loop ends.

So the async NFS serve loop worked for as long as it never had to await: the first request was
already in the socket buffer when the loop first read (no `EAGAIN`), so `MNT` was served; the loop
then read again for the next request (`LOOKUP`), which was not yet there, so the read returned
`EAGAIN`, awaited readability, and the driver refused — the loop ended, the accepted stream dropped,
and the client's next I/O got `BrokenPipe`. The exact failure point moved with scheduling (sometimes
the first write needed writability instead), which is why the client panicked at different lines
across runs.

## Fix

`register_readable`/`register_writable` arm a **one-shot** io_uring `PollAdd` (`POLLIN` / `POLLOUT`)
tagged with the waking task's waker word, via a shared `arm_poll` — the same shape the driver already
used to watch its kick eventfd (`arm_kick`). The driver's `wait` already delivers a non-kick
completion as `Completion { user_data, result }`, which the shard wakes the task by, so the task
re-polls its readiness future and retries the syscall. One-shot, not multishot: the readiness future
arms afresh on every await and io_uring removes a one-shot poll when it fires, so each await is an
independent submission — no interest-list dedup as epoll needs (the `ADD`-vs-`MOD` tracking that was
the epoll `EEXIST` bug).

## Verification

- `crates/bridge-nfs/tests/async_loopback.rs`: 0 of 100 under io_uring after the fix (was 80/80),
  0 of 40 under epoll (unchanged), passing on macOS.
- `cargo test -p slates-rt` under io_uring: 36 of 36 tests pass (the UDP readiness path and the
  sim/os differential included) — the readiness change regresses nothing.
- The bug is exercised on CI from now on: the GitHub `ubuntu-latest` gates lane runs this test under
  io_uring, so a regression of the driver's readiness fails the lane.

## Sibling sweep

- `register_readable` is also the UDP transport's readiness path (`slates_rt::udp::UdpSocket::recv_from`),
  so the fleet's QUIC-over-UDP transport was equally dead under io_uring; the same fix restores it.
  The KIND lane never caught this because Kubernetes' `RuntimeDefault` seccomp blocks io_uring and the
  pods ran on epoll.
- The IOCP (Windows) driver implements readiness through its AFD reactor; the sim driver models it;
  kqueue and epoll carry it. io_uring was the one real driver missing it. No other `Driver` method on
  io_uring is a stub (grep of `DriverRefused` in `uring.rs`).
- The "Owed (§4.10a)" note is removed; §4.10a's readiness seam is now carried by every real driver.
