# The io_uring zero-timeout harvest slept

Date: 2026-09-26. Contracts: §4.3 "Drivers" (io_uring with `SINGLE_ISSUER | DEFER_TASKRUN`) and "Loop".
Found profiling the NFS bench after
`docs/bugs/2026-09-26-a-spinning-shard-was-blind-to-socket-readiness.md` was fixed: an in-process
GETATTR through the kernel's NFS client took 27 µs on epoll and 999 µs on io_uring.

## Symptom

`strace -f -tt -T` of the shard thread serving the mount showed each request read, then one or two
`io_uring_enter(fd, 0, 1, GETEVENTS|EXT_ARG, …) = -1 ETIME <0.0003–0.0010>`, then the reply written:

```
161 06:27:36.784614 read(32, …) = 108
161 06:27:36.784736 io_uring_enter(10, 0, 1, IORING_ENTER_GETEVENTS|IORING_ENTER_EXT_ARG, …) = -1 ETIME <0.000706>
161 06:27:36.785641 io_uring_enter(10, 0, 1, IORING_ENTER_GETEVENTS|IORING_ENTER_EXT_ARG, …) = -1 ETIME <0.000900>
161 06:27:36.786837 write(32, …) = 116
```

## Root cause

A shard harvests its driver without blocking between tasks and, since the fix above, on each turn of
its idle spin: `Driver::wait(Some(0))`. The io_uring driver turned that into `submit_with_args(want = 1,
timeout = 0)`, which asks the kernel for at least one completion with a zero timeout. With nothing
ready, the kernel takes its sleeping path: it arms an hrtimer, schedules the thread out, and returns
`ETIME` when the timer's wake runs. That took 0.3–1.0 ms per call on the Docker Desktop Linux VM
(kernel 6.12.76-linuxkit, load average about 9), so every request paid it once or twice.

A non-blocking harvest is `io_uring_enter(to_submit, min_complete = 0, GETEVENTS)`, liburing's
`io_uring_get_events`. Under `DEFER_TASKRUN` it runs the ring's deferred task work, so a ready poll is
posted, and it returns without waiting. The io-uring crate's helpers set `GETEVENTS` only when waiting
for at least one completion, so the driver never issued it.

## Fix

`UringDriver::harvest_ready` issues that enter through the crate's raw `enter` (one budgeted `unsafe`
site: no argument pointer, and only this module's buffer-free polls and no-ops are ever queued).
`wait(Some(0))` uses it. Every other timeout is unchanged.

## Evidence

- `crates/rt/src/driver.rs` `a_zero_timeout_wait_delivers_what_is_ready_and_never_sleeps` drives the real
  OS driver. A pipe made readable is delivered by one zero-timeout wait. Then 256 zero-timeout waits
  with nothing ready must cost the thread no voluntary context switch (`getrusage(RUSAGE_THREAD)`).
  Before the fix, on io_uring: `256 zero-timeout waits blocked the thread 257 times` (0.26 s). After:
  0, in under 10 ms. It passes on epoll (Docker's default seccomp) and kqueue (macOS delivers; no
  per-thread count there, which the test says aloud).
- The NFS bench (`docs/wip/BENCHMARKS.md` "NFS transports") improved on both transports; for example,
  256 stats (LOOKUP and GETATTR) went from 782 ms to 6.5 ms over NFSv3 and from 1,313 ms to 10.9 ms
  over NFSv4.2. Those numbers include the spin fix.

## Exact edits

`crates/rt/src/uring.rs` (`ENTER_GETEVENTS`, `harvest_ready`, `wait`), `unsafe-budget.toml` (slates-rt
59 of 59), and the test named above.
