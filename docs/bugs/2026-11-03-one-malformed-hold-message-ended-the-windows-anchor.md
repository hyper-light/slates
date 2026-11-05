# One malformed hold message ended the Windows anchor

Found 2026-11-03 by the audit of `2dc9f3eb..98000d56` (kiln plan 01a1a34bf7e973069e94f773afdea44a, step 3).
Design: A-198 (the anchor's Windows hold channel), §2.5.

## Description

The anchor drains its Windows hold channel at every supervision step (`Supervisor::step`, `crates/anchor/src/supervise.rs`).
The Windows hold (`crates/anchor/src/supervise_windows.rs`, `Hold::drain`) passed any refusal of the channel straight up
with `?`:

- a message shorter than `MESSAGE_BYTES` (`held_windows.rs`: "a short read of a hold message");
- a message whose checksum or shape does not decode (`handles::decode`);
- a reply the channel could not write.

`step` returned the refusal, and the anchor's run loop (`crates/cli/src/anchor.rs`) returns on a step error. The anchor
then exits. Its kill-on-close job ends the daemon, and every volume handle and drive link it held closes with it: every
WinFsp drive the anchor was keeping across daemon deaths disappears. The Unix device hold counts the same refusals and
goes on (`Supervisor::drain_devices`).

## Root cause

`2dc9f3eb` and its successors gave the Windows hold a fallible drain and propagated its error through `step`, where the
Unix hold's drain counts refusals and cannot fail. Nothing a daemon sends should be able to end its supervisor.

## Impact

Windows. A daemon bug that writes a malformed hold message, or a write the pipe delivers short, takes down the anchor,
the daemon and every held drive at once: the opposite of what A-198 is for. No run is known to have hit it; the live
tests send only well-formed messages.

## Fix

- `Hold::drain` cannot fail. A message the channel refuses is counted and dropped, and the drain reads on, since the
  next message may be whole. Any other refusal of a read is counted and ends that turn's drain. A reply the channel
  cannot write is counted and dropped, and its daemon's own wait ends at its deadline as a typed refusal.
- `Supervisor::drain_handles` returns the number applied, and `step` no longer fails on it.
- `Supervisor::handle_refusals` counts the channel's refusals with the holdings' own.
- `Channel::recv` tells a timeout from a message of no bytes. A message-mode pipe delivers a zero-length write as a read
  of zero bytes, which was taken for "nothing waited": uncounted, and the end of that turn's drain. It is now a refused
  short message.
- A message longer than a hold message is malformed, not an OS refusal (`ERROR_MORE_DATA`, error 234). The pipe keeps its
  remainder, which the next read refuses as a short message, so one such message counts two refusals.
- The log is bounded. The draft of this fix logged every refusal, so a read the OS kept refusing wrote a line per
  supervision step for the anchor's life. `RefusalLog` (`crates/anchor/src/handles.rs`) now logs each distinct refusal
  once, in a table of 64; every refusal is still counted. Logging on a change of kind was rejected on a measurement: a
  child alternating two kinds logged 20 lines for 20 messages, and the unit test 1,000 for 1,000.

## Failing test first

`crates/anchor/tests/handles_windows.rs`:

- `a_malformed_hold_message_is_refused_and_counted_and_supervision_goes_on`: a real supervised child (`garbling_role`)
  holds one event, writes 10 short messages (0 bytes up to nine tenths of a message) and 10 with a corrupted checksum,
  holds a second event, commits and exits. The test requires every step of the real supervisor to succeed, exactly 20
  refusals, both holds kept, and the successor to inherit both live objects.
- `an_oversized_hold_message_is_refused_and_counted_and_supervision_goes_on`: the same with one message half as long
  again as a hold message; exactly 2 refusals.
- `a_read_tells_no_message_from_a_message_of_no_bytes_and_refuses_a_long_one`: the channel's reads over the real pipe.
- `every_refusal_is_counted_and_the_log_says_each_kind_once`: the anchor as a process of its own, whose log the test
  reads, over 20 bad messages in two runs and then taking turns: 20 counted and 2 lines both times.
- `a_refusal_is_logged_once_and_the_lines_stay_bounded` (`handles.rs`): 1,000 repeats of one OS refusal log 1 line, two
  kinds alternating 1,000 times log 2, and 128 distinct kinds log 64.

The supervisor here is the real state machine in the test's process and the daemon is a real child process. A failing
`step` is what ends the anchor: `crates/cli/src/anchor.rs` returns on it.

## Evidence

Command: `cargo +1.98.1 test -p slates-anchor --test handles_windows -- --test-threads=1`, 2026-11-04.

| Sources | Where | Result |
|---|---|---|
| `crates/anchor/src` of `76b450a1` with the new tests | jupiter, Windows x86_64 | 3 of 6 fail: "a bad hold message ended supervision: anchor hold: malformed hold message: a short read of a hold message"; the oversized test with "ReadFile of a hold message refused: error 234"; the read test with "no bytes: Ok(None)" |
| The same | CI run 41974533851, windows-latest and windows-11-arm, 3 runs each | the same 3 of 6 fail, with the same messages. Annotations show runs 1 and 2 whole on each lane; the 10-annotation cap cut off run 3's result line, and the step exited 1 |
| The drain fix with the old `held_windows.rs` | jupiter, Windows x86_64 | 2 of 6 fail: 19 refusals counted of 20, and "no bytes: Ok(None)" |
| The whole fix, logging every refusal | jupiter, Windows x86_64 | 2 fail: the anchor process logs 20 lines, not 2; the unit test 1,000, not 1 |
| The whole fix, logging on each change of kind | jupiter, Windows x86_64 | 2 fail: the anchor process logs 20 lines, not 2; the unit test 1,000, not 2 |
| The whole fix | jupiter, Windows x86_64, 3 runs of `-p slates-anchor` | 7 of 7 in `handles_windows` each run, 14 of 14 in `anchor`, 16 of 16 in the unit tests |
| The whole fix, logging every refusal | CI run 41976042399, windows-latest and windows-11-arm | both jobs fail; the windows-11-arm log shows the anchor process logging 20 lines, not 2. The windows-latest job's log was not read |
| The whole fix (`fd56be35`) | CI run 41976042550 (`diag-step3-anchor-4`), windows-latest and windows-11-arm | both jobs pass: anchor clippy, 3 runs of `handles_windows`, the whole anchor crate, and the live WinFsp lane (`SLATES_TEST_CLI=1 WINFSP_TEST_MOUNT=1 cargo test -p slates-cli --test winfsp`). Read from the job results; the live lane's test count was not read |
| The whole fix (`fd56be35`) | The same run, ubuntu-latest and macos-latest | both jobs pass: `cargo fmt --all --check`, `cargo xtask check`, anchor clippy and anchor docs |

So on both Windows architectures, 20 bad messages caused 0 anchor exits: each run's test requires every `step` to succeed
and exactly 20 refusals, both holds kept and both inherited live. The 20 messages are asserted in that test, and 3 runs
each make 60 of 60 per lane.

Not shown: a live WinFsp drive receiving bad messages. The channel's daemon end reaches only the daemon, by an explicit
handle list, and the real daemon sends only well-formed messages, so a live test would need a test-only way to make it
garble, which the project's rules ban. What is shown is that the holdings are untouched by a refusal and the successor
inherits them live, and the live WinFsp lane passes over the changed drain with only well-formed messages.

## Siblings

- The server's side of the channel (`crates/server/src/anchor_hold_windows.rs`) already turns a refused exchange into a
  typed mount refusal.
- A hold whose reply is dropped leaves its daemon's mount refused typed; the holding itself, if applied, is released
  with the next successor's handoff or the anchor's exit.
- The Unix device hold (`DeviceHold::refuse` in `crates/anchor/src/supervise.rs`) still logs a line per refusal, the
  unbounded log this change removes from the Windows hold. `RefusalLog` is cfg-free and could serve it; not changed here.
- `Supervisor::handle_refusals` has no reader outside the tests, so past each kind's first log line a refusal shows only
  in a count nothing prints. Not changed here.
- `cargo xtask check` does not compile on Windows: recorded in GAPS.
