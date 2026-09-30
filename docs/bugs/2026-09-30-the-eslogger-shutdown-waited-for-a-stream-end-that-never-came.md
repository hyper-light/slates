# The eslogger shutdown waited for a stream end that never came (2026-09-30)

Contracts: AC-9.7 (hermeticity evidence), R1/R10. Found in CI run 36655388624 (`c59f3aa`): the macOS
conformance job ran 82 minutes, where 8 to 11 is usual. It was cancelled so that its `always()`
uploads would keep the evidence; a job killed at the six-hour limit keeps no log.

## Symptom

The job's log stops at `hermeticity: eslogger recorded the binary's events` (01:40:43). The next line
is the cancellation, at 02:54:11.

The kept trace (`conformance-anchor-logs-macos-latest`, `trace.log`, 810 slates events) shows the whole
lifecycle finishing by 01:40:46.63:

- the workload over the mount;
- the granted landing's writes into the target;
- the anchor's stop, and the daemon leaving ("the anchor is gone; leaving");
- the drain marker (`slates --help`, pid 84737), whose events are the last lines.

So slates had finished, and the evidence was complete. The xtask then spent 74 minutes in
`EsLogger::stop`.

## Root cause

`EsLogger::stop` did three things in turn:

1. It waited for the marker's event, which arrived.
2. It stopped the tracer. That wait is bounded: 20 s (`MOUNT_WAIT`).
3. It joined the stdout filter thread. That join had no bound, and ran whether or not step 2 had
   succeeded.

The filter read eslogger's stdout line by line until the stream's end. That end comes only when every
process holding the pipe's write end has gone.

- If the tracer did not stop in step 2, eslogger kept streaming the whole machine's events, and the
  join waited forever.
- If the tracer did stop but a process that outlived the stop still held the pipe, the join likewise
  waited forever.

The job's log cannot say which happened: the xtask never returned to print a verdict. That is itself
the defect.

## Fix

`xtask/src/conformance/hermeticity.rs`:

- The filter reads the stream without blocking, and ends when told the tracer has stopped. It drains
  at most one pipe buffer (64 KiB) after that, so a writer that outlived the stop cannot keep the drain
  going.
- `shut_down` stops the tracer first, then tells the filter and joins it. A stop that failed is still
  reported, and this value's drop cancels the tracer's process group.
- A line torn at the stop is past the drain marker, whose own event was already filtered, so it is not
  evidence and is not judged. A line torn at the stream's end still counts, as before.

A tracer that fails to stop now fails the suite, with eslogger's stderr, within the 20 s bound.

## Evidence

- **Red.** `the_tracer_shutdown_returns_while_a_descendant_holds_its_stream_open` uses a stand-in
  tracer that writes one event, leaves a descendant that ignores the stop signal and holds its stdout,
  and exits on the stop signal. Under the old shutdown it timed out at its 40 s bound; the descendant
  (reparented to pid 1) held the stream.
  - Found on the way: the first version of the fixture passed, because the stop signal reached the
    subshell before its ignore trap. The descendant now reports on stderr before the stop is sent.
  - Found on CI (run 36663034331, Linux): the test then dropped its stderr reader, and dash reports its
    foreground child's death there on the stop (`Terminated`), so the stand-in died of SIGPIPE instead of
    exiting on the stop signal — 20 of 20 runs as a non-root user in Docker. The reader now stays open to
    the end, as the real tracer's stderr file does: 20 of 20 as that user, 10 of 10 as root and 10 of 10
    on macOS.
- **Found on CI (run 36663502686, macOS): the first non-blocking filter slept on an empty pipe.** It
  paused 20 ms whenever a read found nothing, and a machine-wide eslogger stream outran it. The kept
  trace shows twelve readiness probes where the old blocking read's run needed two, then nothing more:
  the anchor, the daemon, the landing and the drain marker never reached the log. The suite failed
  "tracer produced no event within 20s" instead of hanging. The idle filter now `poll`s the stream for
  at most one stop-check interval (20 ms), so it wakes the moment data arrives, as a blocking read
  does. The mechanism is inferred from the trace; the fix is proven only by the macOS lane.
- **Green.** The test passes in milliseconds, and the xtask suite is whole (31 and 4).
- **Still to confirm.** The live eslogger path runs only on the macOS CI lane, which needs root; this
  machine does not run `sudo`.

## Open

- A tracer process that outlives a successful stop is not swept. The runner discards its machine, and
  locally the tracer's group is cancelled only when the stop failed.
