# The hermeticity judge exempted writes by name and descriptor

**Date:** 2026-10-01. **Area:** `slates-conformance` (`trace.rs`), `xtask` (`conformance/hermeticity.rs`,
`conformance/slates.rs`). **Audit:** AUD-29-42 (P2). **Design:** R1, R10, D-26, AC-4.5, T-9.1.

## Description

The hermeticity tracer's judge placed each traced write by its path and descriptor alone, and four rules
were exemptions rather than proofs:

1. **Any write on descriptor 1 or 2 was a standard stream,** whatever file was behind it. A daemon writing
   its log to a regular file through standard error passed. The policy also exempted the anchor's log file
   by name, so the slates processes really did write a disk file on every run: the harness handed them a
   file as stderr.
2. **Any write inside the landing target was allowed.** The judge took no grant input, so it accepted a
   write before the grant, after the landing, by another process, or in a lifecycle that granted nothing.
3. **Any `.slates-*` name inside the target was accepted** as the landing engine's hidden sibling, and
   nothing checked that none remained.
4. **`shm_open` paths are RAM-only by name** on the fs_usage parser. On Linux a `/dev/shm` path was already a
   violation (A-50).

**What the old judge did with the acceptance's mutations** (each now a test):

| Mutation | Old judge |
|---|---|
| A write inside the target before the grant's landing began | accepted (inside target) |
| A write inside the target after it ended | accepted |
| A write inside the target by another process | accepted |
| A write inside the target with no landing granted | accepted |
| A log file behind standard error | accepted (standard stream) |
| Any `.slates-*` name | accepted as hidden |
| A `/dev/shm` path | violation (A-50) |

## Root cause

The judge modelled where a write went, not who made it under what authority and when. The harness made
the stream exemption necessary by giving the slates processes a disk file as stderr.

## Fix

- **Every event carries its process and time.**
  - strace runs with `-ttt` (wall-clock seconds.microseconds), and the parser keeps the pid and the stamp,
    taking the start of an `<unfinished ...>` call.
  - eslogger events keep `process.audit_token.pid` and their RFC 3339 `time`, converted with the civil-day
    algorithm. The golden vectors are checked against Python's `calendar.timegm`.
  - fs_usage names neither, so its target writes cannot be placed and are refused.
- **The policy carries the granted landing:** the processes that execute it (the daemon) and its interval
  on the tracers' clock (`Landing { writers, from_ns, until_ns }`). A write inside the target is
  `Unauthorized` for a stated reason if any of these hold:
  - no landing was granted;
  - the process is not named or is not a writer;
  - the time is not stamped or falls outside the interval.
- **A standard stream is a write on descriptor 1 or 2 to a pipe, terminal or null device.** A regular file
  behind the descriptor is judged as a file. The path-named `streams` exemption is removed.
- **The harness gives the anchor, and through it the daemon, a pipe as stderr,** and a harness thread
  drains it into the log. The slates processes no longer write a disk file. The thread is joined once
  both have left.
- **Hidden siblings must be this landing's own forms,** `.slates-{id:016x}-{n}` or
  `.slates-{id:016x}-aside-{hash:016x}`, under the presented landing's id. None may remain in the target
  afterwards (`judge_hidden`, with the target walked after the landing).
- **Each violation carries its reason,** which the record notes print with the pid and time.

## Tests

- `each_mutation_of_a_granted_trace_fails_for_its_own_reason`: a clean granted trace passes. Each mutation
  fails with its own reason: before the interval, after it, another process, no grant, a log file behind
  descriptor 2, a `/dev/shm` path, and an unstamped write.
- `only_the_landings_own_hidden_names_pass_and_none_may_remain`: another landing's name, a malformed aside,
  an empty counter, an arbitrary `.slates-` name, and a kept name are refused; a name left on disk is
  refused.
- `tracer_times_parse_to_epoch_nanoseconds`: golden vectors and hostile stamps.
- The existing trace tests, updated:
  - the strace fixture's log file behind descriptor 1 is now a violation;
  - the eslogger fixture's anchor log is now a violation;
  - fs_usage's target writes are refused as unattributed.

## Not shown here

- **The live lanes are not yet shown.** The hermeticity lanes need root (Linux strace with a kernel mount;
  macOS `sudo eslogger`), which this machine's runs do not take. CI runs both on the next push.
- **The eslogger `time` field is cited from memory.** Its name and format are not verified against Apple's
  documentation. If it is absent, the macOS lane fails with "a time the tracer did not stamp", a visible
  refusal and not a silent pass.

## Siblings reported

- **The tracer still models writes only.** Reads, pageout, core dumps and external logging remain outside
  it (AUD-29-41, §9.3).
- **fs_usage cannot support the grant interval.** It is no longer the macOS lane's tracer; any future use
  needs a dated clock and the traced pid.
