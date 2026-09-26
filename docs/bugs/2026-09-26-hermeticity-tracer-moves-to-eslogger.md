# The macOS hermeticity tracer moves from fs_usage to eslogger

Date: 2026-09-26. Contracts: R1, Part 6 example 8, AC-4.5, AC-9.4. Found by every macOS conformance run
(for example 36264748298): `hermeticity: 93 write-capable calls … 92 unresolved`.

## Why fs_usage could not judge

fs_usage prints an `openat` family path as `[dirfd]/name` and suppresses `dup`/`dup2` rows, so a
descriptor the daemon duplicated or opened before tracing started cannot be named. 92 of 93 calls were
unresolved (`2026-09-22-fs-usage-startup-and-ownership.md`).

## What was tried

- **DTrace, 2026-09-26, run by Ada with sudo on this Mac.** Refused outright:
  ```
  dtrace: system integrity protection is on, some features will not be available
  probe description syscall::open*:entry does not match any probes. System Integrity Protection is on
  ```
  The syscall provider does not exist under SIP; GitHub's macOS runners also have SIP on.
- **eslogger (`/usr/bin/eslogger`, Endpoint Security, macOS 13+), run by Ada with sudo and Full Disk
  Access for Terminal.**
  - A throwaway daemon ran a volume, a kernel mount, files created, renamed, tagged, removed and
    snapshotted.
  - The daemon's events, filtered by pid, were three `close` events at exit: its stderr log (modified),
    the tty and `/dev/null`. Each came with its full kernel path. The daemon made no file event during
    the session.
  - Found: eslogger ignores `SIGINT` and ends on `SIGTERM`; the probe hung on `SIGINT`.

## Change

- `slates_conformance::trace::parse_eslogger` places Endpoint Security events (JSON lines).
  - Write-capable kinds: `open` with a write flag (`FWRITE|O_APPEND|O_CREAT|O_TRUNC`), `close` of a
    modified file, `create`, `write`, `truncate`, `rename` (both names), `unlink`, `link`, `clone`,
    `copyfile`, `exchangedata`, and the attribute, mode, owner, flags, times and ACL setters.
  - A missing or truncated path, or a torn line, is unresolved.
- `Policy::streams` names regular files that are the processes' own standard streams (the anchor's log,
  which the harness gives the anchor and the daemon as stderr). ES events carry no descriptor number.
- The harness (`xtask/src/conformance/hermeticity.rs`) runs `sudo eslogger` before the anchor starts
  until after it stops.
  - The events are filtered live, on a joined thread, to those whose `process.executable.path` is the
    built slates binary: anchor, daemon and every CLI call. The old tracer covered the daemon alone.
  - It is ready once the binary's own events arrive, and stops on `SIGTERM` (`stop_accepting`).

## Tests and limits

- `eslogger_events_are_placed_by_their_kernel_paths`: every placement class, the stream log, read-only
  opens and clean closes ignored, a truncated path and a torn line unresolved. The trace-process
  tests (6) and the conformance suite (47) pass.
- The full traced lifecycle has not run yet. It needs root and Full Disk Access: here from Ada's
  Terminal, and on CI only if the runner grants Full Disk Access to its shell (not yet known).

## First full run (Ada, 2026-09-26): the trace stopped early

- The run completed: the landing reported 12 of 12 written and the disk verified. The judgement read
  137 events: 94 standard streams, 0 unresolved, 43 outside.
- The 43 were all `open /dev/dtracehelper`. macOS's dynamic loader opens that DTrace helper device
  read-write in every process it starts. It is now in the character-device class.
- The trace was not complete. Every kept event fell between 21:56:28.0 and 21:56:29.9, while the run
  went on for seconds more. No event named the target, and no `create`/`rename`/`unlink` event appeared.
  Endpoint Security had delivered 16,705 machine-wide events to the client in those 1.9 s (by
  `global_seq_num`), and then the filter kept nothing.
- The likely cause is backpressure: the harness parsed every machine-wide line as JSON in an
  unoptimized build. This is not confirmed.
- Changes:
  - The filter tests a line for the binary's path as a substring before parsing it.
  - It checks every line's `global_seq_num` for continuity; a gap fails the run with the count of
    dropped events.
  - Before stopping eslogger, the harness runs the binary once more and waits for that run's events, so
    the stream is proven caught up past the daemon's teardown.
- That run's record (written into `docs/wip/conformance/records` by `conformance run`) described a
  partial trace and was not committed.
