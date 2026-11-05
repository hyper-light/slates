# The Unix device hold logged every refusal, and no operator could see the count

Found 2026-11-03 by the audit of `2dc9f3eb..98000d56` (kiln plan 01a1a34bf7e973069e94f773afdea44a, step 3), listed as
siblings in `docs/bugs/2026-11-03-one-malformed-hold-message-ended-the-windows-anchor.md`.
Design: A-213 (amends A-61 and A-198), §2.5.

## Description

1. The Unix device hold (`DeviceHold::refuse` in `crates/anchor/src/supervise.rs`) wrote one line on the anchor's
   log for every message it refused. A daemon sending bad device messages, or an OS call refusing at every spawn, grew
   the log without bound for the anchor's life. The Windows hold had been given a bounded log (`RefusalLog`) in the
   audit's step 3; the Unix hold had not.
2. Each hold counted its refusals (`Supervisor::device_refusals`, `Supervisor::handle_refusals`), but only the tests
   read them. With the Windows log bounded, a refusal after its kind's first line could not be seen anywhere in
   production.

## Root cause

The two holds grew separately. The Windows fix bounded its own log and left the Unix one as it was, and neither count
was given a reader outside the anchor's process. The anchor has no surface of its own: `slates status` asks the daemon,
and the daemon learns about its anchor only through the segment's supervision block, which carried restarts but no
refusals.

## Impact

Linux and macOS anchors. A daemon bug that writes bad device messages would fill the log, one line a message. Nothing
grows the count in normal use: the real daemon sends only well-formed messages. On Windows, refusals past each kind's
first line were invisible.

## Fix

- `RefusalLog` is generic over the refusal type, and the Unix device hold logs through it: each distinct refusal once,
  in a table of 64; every refusal is still counted.
- The supervision block gains a word, `SUP_HOLD_REFUSALS` (offset 192, the cache line after the sealing root). Each
  supervision step writes `Supervisor::hold_refusals` there when it has moved. The daemon reads it into
  `DaemonReport::anchor_hold_refusals` (appended), and `slates status` prints `anchor_hold_refusals: N`; `--json` and
  the MCP daemon status carry it too.

## Failing tests first

- `every_bad_device_message_is_counted_and_the_log_says_each_kind_once` (`crates/anchor/tests/anchor.rs`, Linux and
  macOS): a real supervisor runs as a process of its own over a daemon sending 20 bad device messages, a short one and
  one of an unknown kind in turn. The test reads the anchor's stderr: 20 counted, 2 lines.
- `a_real_anchors_hold_refusals_show_in_slates_status` (`crates/cli/tests/cli.rs`, the CLI flow lane on Linux and
  macOS): the test plays the anchor with the same supervisor `slates anchor` runs, over a process that sends the same
  20 bad messages on its inherited channel and then execs the real `slates daemon` with the same channel, segment and
  rendezvous; `slates status` must say `anchor_hold_refusals: 20`. No daemon code garbles.

## Evidence

Commands, 2026-11-04, adacachy (Linux x86_64, inside a capped `systemd-run --user` unit):

| Sources | Command | Result |
|---|---|---|
| `16510b30` with the anchor test | `cargo +1.98.1 test -p slates-anchor --test anchor -- every_bad_device_message --test-threads=1` | fails: 20 counted, `left: 20, right: 2` log lines |
| The fix | the same, then `SLATES_TEST_CLI=1 cargo +1.98.1 test -p slates-cli --test cli -- a_real_anchors_hold_refusals --test-threads=1 --nocapture` | both pass; the anchor logged 2 lines, `wrong length` and `unknown kind` |

CI run ids for Linux and macOS, red and green, are in the GAPS entry.

## Siblings

- The Windows anchor looks at its hold channel once a supervision step: A-213's third part.
