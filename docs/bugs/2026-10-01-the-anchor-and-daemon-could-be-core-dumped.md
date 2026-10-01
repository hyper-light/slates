# The anchor and the daemon could be core-dumped (AUD-29-41, dump exclusion)

**Date:** 2026-10-01. **Audit:** `docs/audit/2026-09-29_audit.md` AUD-29-41 (P1), the dump half. **Design:**
§4.2 "RAM-only guarantees", the RAM-only claims list (claim 2), R1.

## Description

The anchor holds the shared segment and the recovery images; the daemon holds every volume. Neither set
any dump policy. A fault (or a `SIGQUIT`/`SIGABRT`) let the kernel write the process's memory — the heap,
the stacks, the shared segment — to a core file wherever the session's limit allowed, or to a pipe
collector (`apport`, `systemd-coredump`) regardless of that limit. Another process of the same user could
also read their memory through `/proc/<pid>/mem`, `process_vm_readv` or a debugger.

Measured from outside the real processes in the Linux container (`rust:1.98.0`, 2026-10-01), before the
change: core limit `0 unlimited` (soft 0 from the container, hard raisable), core filter `0x23` (anonymous
private, anonymous shared and ELF headers selected), `/proc/<pid>/environ` readable by the same user.

## Root cause

No code set one. The design's claim 2 recorded it as "no dump exclusion is set".

## Impact

Private volume bytes could reach disk through a crash collector or a core file (R1), or another process of
the user through the debugging interfaces. No dump was observed; the audit and this record establish the
absent guarantee, not a leak.

## Exact edits

- `crates/cli/src/dumps.rs` (new): `exclude_from_dumps` — `setrlimit(RLIMIT_CORE, 0/0)` on every Unix;
  on Linux `/proc/self/coredump_filter` = 0 (a procfs control write under a
  reasoned `structural: allow`). A refused setting is a typed start failure.
- `crates/cli/src/anchor.rs`, `crates/cli/src/daemon.rs`: called first in `run`, before any private byte
  is mapped or received.
- `crates/cli/tests/cli.rs`: `the_anchor_and_its_daemon_exclude_themselves_from_core_dumps` (Linux).

## Proof

- Real processes, Linux (Docker, `rust:1.98.0`): red without the calls (`0 unlimited`, `0x23`, readable);
  green with them (anchor and daemon: `0 0`, `0x0`, `environ` closed). The whole CLI suite passes on Linux
  (14 + 34 unit) and macOS (13 + 34 unit): the landing's `linkat` through `/proc/self/fd` still works for a
  non-dumpable process (the kernel exempts the same thread group).
- `dumps::tests::a_process_excludes_itself_from_core_dumps_for_good` (every Unix): the limit reads 0/0
  and cannot be raised by an unprivileged process; on Linux the process reads non-dumpable with filter 0.

## Carried / siblings

- Windows: the daemon builds there (`slates-server`) but has no dump policy; Windows Error Reporting's
  `LocalDumps` can write a full dump. Owed with AUD-29-41.
- Residency (no pageout) of metadata, rings, records, codec and transport buffers: AUD-29-41's other half.
- The client processes (SDK hosts) are the user's; they map the shared rings and are outside this change.

## Correction (2026-10-01, the same day)

The first commit (`ac7f582`) also made both processes not dumpable (`prctl(PR_SET_DUMPABLE, 0)`) and wrote
the core filter after it. Two faults, both found by running the Linux hermeticity suite as a non-root user in
a container (the GitHub runner's case; every earlier Docker run had been as root):

- **Order.** A non-dumpable process's `/proc/self` files belong to root, so an ordinary user's write to the
  filter was refused `EACCES` and the anchor refused to start: `slates: cannot exclude this process from core
  dumps: the core filter was refused (code 13)`. Every non-root Linux start failed.
- **Scope.** Not dumpable also closes the processes' `/proc` to every other process of the user — the path the
  conformance harness reaches the segment's descriptor by to issue a grant on Linux
  (`/proc/<daemon>/environ` refused), where the issuer surface is owed (§4.13, `docs/wip/enrollment.md`). The
  flag adds no dump content the core limit and filter leave; who may reach the segment is the issuer
  surface's decision.

The flag is removed; the core limit and the empty filter remain, and the test observes those two. Measured
again as a non-root user on Linux: the CLI suite 14 + 34 pass, anchor and daemon `0 0` and filter `0x0`. The
lesson is the one already on record: run a Linux process test as a non-root user before calling it verified.

