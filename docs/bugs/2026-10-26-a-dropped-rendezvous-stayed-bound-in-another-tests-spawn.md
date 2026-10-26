# A dropped rendezvous stayed bound in another test's spawn

- **Date:** found 2026-10-26 (CI run 40262946431, commit 5a6d9e18), fixed 2026-10-26.
- **Status:** fixed 2026-10-26 in the tests, decided under the standing goal, not in plan mode. No product code changes.
- **Subsystem:** the rendezvous tests (§4.7): `crates/ipc/tests/rendezvous.rs`.
- **Severity:** a red Linux gate lane. Three tests failed at random on a busy machine. The product is not affected
  (see Impact).
- **Exposure:** Linux, in any run of the rendezvous suite with more than one test thread. The race is older than
  A-178. A-178 (657c1c80) made it more frequent: it added a fourth test that spawns a child, and it enabled
  `a_second_owner_of_a_live_instances_rendezvous_is_refused` on Linux, which has the same exposure.
- **Found by:** CI's "fmt + clippy + structural + literals + test (ubuntu-latest)" job on 5a6d9e18, in one of the
  seven runs that contained A-178.

## Symptom

```
---- a_fresh_rendezvous_opens_while_a_client_of_a_dead_daemon_lives stdout ----
thread 'a_fresh_rendezvous_opens_while_a_client_of_a_dead_daemon_lives' (17303) panicked at crates/ipc/tests/rendezvous.rs:551:6:
a fresh rendezvous opens while a client of the dead daemon still maps the old one: OsRefused { call: "bind", code: Some(98) }
```

Errno 98 is `EADDRINUSE`: the dropped daemon's listener was still bound at the instance's abstract address.

## Root cause

Every child process a test in this binary spawns gets a copy of each descriptor the process holds at the fork. The
copy lasts until the child's `exec` closes the close-on-exec descriptors. A Linux socket keeps its abstract address
until its last copy closes.

So when a test drops its listener and at once binds the same address again, the bind is refused if another test,
on another thread, is spawning a child at that moment. Four tests in the binary spawn children:
- `a_client_process_connects_and_completes_a_round_trip`;
- `a_client_process_refused_at_the_bound_is_told_so_typed`;
- `a_claim_begun_before_a_restart_judges_the_daemon_that_answered_it_alive`;
- `a_second_process_is_refused_a_live_instances_rendezvous` (A-178).

Three tests drop an owner and own its address again right away. Each is exposed:
1. `a_fresh_rendezvous_opens_while_a_client_of_a_dead_daemon_lives`: drops `dead`, then calls `Listener::open`.
2. `a_second_owner_of_a_live_instances_rendezvous_is_refused`: drops `first`, then calls `HeldRendezvous::hold`.
3. `a_second_process_is_refused_a_live_instances_rendezvous`: drops `first`. Then its child process must own the
   address, as a daemon run alone and as an anchor.

### The evidence

All runs were on adacachy (32 cores, Linux, shared with other work, load 44 to 80), in a scratch copy with an
instrumented test, 16 test threads, capped with `systemd-run --user -p CPUWeight=1 -p Nice=19`. The binary was run
directly:
`rendezvous-583db6b4556e43a3 --test-threads=16 [filters]`.

**The instrumented loop: 5 failures in 33 runs.**
- Four were test 1. Each refused bind cleared by the first retry, 13.6 to 36.4 µs after the instrumentation's own
  read of `/proc/net/unix`:

  ```
  RACE-REPORT refusal=OsRefused { call: "bind", code: Some(98) } me=3246650 name=@slates-rv-test-fresh-3246650/1000 inodes=[] freed_after=36.328µs attempts=1 freed=true
  RACE-REPORT refusal=OsRefused { call: "bind", code: Some(98) } me=3263888 name=@slates-rv-test-fresh-3263888/1000 inodes=["2856570391"] freed_after=36.448µs attempts=1 freed=true
  ```

- One was test 3, whose child found the address still held after its owner was gone.
- The holds cleared faster than a scan of `/proc/*/fd` could find the holder, so the holder was not named this way.

**An interleaved A/B: 100 runs per arm.** Each arm ran once per round, so all four saw the same load.

| arm | tests run | runs failed |
|---|---|---|
| A | the whole suite | 44 (34 test 1, 10 test 2) |
| B | the suite without the four tests that spawn children | **0** |
| C | test 1 with test 3 and its child | 25 |
| D | the suite as before A-178 (without tests 2 and 3) | 16 |

**What it shows.**
- Arm B: no test fails when no test spawns a child. So a concurrent spawn is the cause, not the listener's own close.
- Arm D: the race was there before A-178.

## Impact

Tests only. The product never drops a listener and binds the same address again while a thread of the same process
spawns children:
- **A daemon run alone** binds close-on-exec. Its children hold a copy only between their fork and their exec. Its
  successor is a new process, which starts after the old one has died.
- **An anchored daemon** adopts the anchor's socket (`adopt`). The anchor holds the address for its whole life and
  never binds it again.

**Sibling, reported and not changed.** A daemon keeps the anchor's original descriptor number open, and inheritable,
for its whole life ("nothing here closes the number", `adopt`). So the `fusermount3` helpers it spawns also hold the
rendezvous for their lives. Those lives are short (no `auto_unmount`, `BASE_OPTIONS`), and the anchor holds the
address anyway, so no bind is refused because of them.

**Other suites, checked.** The client and server suites hold the rendezvous across their daemon restarts, as an anchor
does: `HeldRendezvous::hold` once per test, under a name of its own, and daemons adopt it. They never bind a dropped
address again, so they are not exposed.

**A sibling from before, now explained (probable).** GAPS kept one CI failure open as unexplained:
`a_green_chain_survives_a_daemon_restart` (`crates/client/tests/client.rs`), run 36554081271 on `b036a0e`
(2026-09-29). Its second daemon's bind was refused `EADDRINUSE` right after the first stopped.
- At `b036a0e` that test's daemons bound the address themselves, and three tests of the same binary spawn children:
  the conditions measured here.
- Since `af985556` (2026-10-25) the test holds the rendezvous and its daemons adopt it, so it is no longer exposed.
- The entry is closed in GAPS as probable, not reproduced at `b036a0e`.

## Fix

The three tests own the address again through `once_released`. It retries the owning while it is refused as held:
- Linux: `bind` refused with `EADDRINUSE`;
- Windows and macOS: `InstanceHeld`.

It stops at `RELEASE_WAIT`, one claim wait (1 s), far past any exec. A hold that never ends still fails the test,
after one second. Each retry yields the thread rather than sleeping, so no interval constant is needed.
`once_released` prints how many refusals it waited out, so a CI log shows when the race happened.

This follows the CLI tests' precedent for the same harness property: `unmount_through_the_cli` retries a busy answer
within `WAIT_FOR` (CLAUDE.md, "A real-process CLI test with a guard").

The test-3 child now tries without retrying when it expects the address held, since a live owner must refuse it. It
retries only when it expects to own the address.

## Verification

- **Red, before the fix:** arm A of the A/B above, 44 of 100 runs failed (adacachy, load 44 to 80).
- **Green, with the fix:** the whole suite, 100 runs on the same box, at load 60 to 94, with 16 test threads and
  `--nocapture`: **0 failures**.
  - The race still happened. `once_released` waited it out 25 times: 8 after one refusal, 3 after two, and the
    longest after 598, all far inside `RELEASE_WAIT`. So the retry path is exercised, not dead code.
- **macOS:** the suite passes, 14 of 14. `cargo clippy -p slates-ipc --all-targets -- -D warnings` is clean.
