# A host's wait on a looping anchor passed its deadline on Linux

- **Date:** found 2026-11-27 by main, reading the Ubuntu gates job of diag CI 10 (run 46033104507, job
  140621619115, at 2541225f5). That job's Test step ran from 16:31 to 22:23 UTC, its job's whole six hours, and was
  cancelled. Diag CI 11 (run 46057905828, at 230d5a1d7) did the same. Fixed the same day.
- **Status:** fixed in `slates-api` (`Instance::wait_for`), with the test bounded so a regression fails in about a
  second instead of hanging the lane.
- **Subsystem:** the host's start wait (§4.12 "Embedding", A-278, A-366), over the rendezvous's claim (§4.7, A-468).
- **Severity:** the Ubuntu gates lane never finished its workspace tests, so no Linux result of any crate after
  `slates-api` was seen. diag 7's lane had stopped earlier, out of disk, so the gap went unseen. A host that started an
  anchor its own way and waited with a deadline could wait forever on Linux while the anchor crash-looped its daemon.

## Symptom

The last test the binary started, and the lines it printed until it was cancelled 5 h 45 min later:

```
16:38:00.66 test an_anchor_asked_until_served_stops_when_none_of_its_daemons_serves ...
16:38:00.97 an anchor asked until_served stopped after 301.074084ms
16:38:01.20 slates anchor: instance api-start-looping-25637-11 up; daemon pid 25726; 1 shards
...
22:23:00.76 slates anchor: daemon exited (None: ended by a signal, which leaves no exit code (Unix)); restarted as pid 31399 (3449 failed starts in a row)
22:23:04.88 ##[error]The operation was canceled.
```

The test's first half passed. Its second half starts an anchor that is not asked to stop (`until_served` unset),
whose every daemon fails before serving. It then waits with a deadline twice as long as the first half took, about
600 ms. That wait never answered.

## Root cause

`Instance::wait_for` looped over `Instance::attach`, which is `Client::connect`: a blocking connect. It looked at its
deadline only after a connect had answered.

A claim waits while its rendezvous's holder lives, never by a clock (A-443, A-447, A-468). On Linux the holder is the
listening socket, which the anchor keeps across its daemon's restarts (A-114). So a claim queued on that socket, while
every daemon exits before it accepts, waits as long as the anchor lives. A looping anchor lives forever. The connect
never returned, and the deadline was never looked at.

macOS does not hang here: the macOS gates jobs of diag 10 and 11 ended within their hours, and the test passes on
adas-macbook-pro-2 with the fix (10 of 10 start tests, 2026-11-27). The reason is inferred from the rendezvous's
design, not logged. On macOS the anchor holds a bootstrap name, not a socket a connect can queue on, so a claim made
before a daemon has published its slots is refused at once, and `wait_for` sees each refusal and its deadline.

## Fix

`wait_for` now makes each claim with `Client::begin_connect` and polls it (`claim_within` in
`crates/api/src/start.rs`). Between polls it looks at the anchor's exit and at the deadline, paced as an event loop's
connect is (`Connecting::next_poll_ns`, doubling to a tenth of the claim wait), each pause clipped to the deadline. A
claim still unanswered at the deadline is refused `Unavailable`, as a connect refused at once is. The claim itself still
waits while its holder lives, as A-468 requires; only the caller's deadline and the anchor's exit can end the wait
sooner.

## Test

`an_anchor_asked_until_served_stops_when_none_of_its_daemons_serves` (`crates/api/tests/start.rs`) now runs its second
wait on a thread of its own, held to the deadline plus one claim wait (`PAST_DEADLINE`, `CLAIM_WAIT_NS`, a second).
A wait still blocked then fails the test, after the anchor is killed, so the kernel resets the queued claim and the
thread ends. Three runs of the start tests on each of `ubuntu-latest` and `ubuntu-24.04-arm`, 2026-11-28 01:33 UTC:

| Branch | Run | Result |
|---|---|---|
| `diag-measurer-apistart-red-1`, the bounded test alone | 46093455640 | red 6 of 6: "the wait answered within 1s of its deadline", the wait answering only once the anchor was killed (`NoDaemon`: "the daemon closed the rendezvous before answering") |
| `diag-measurer-apistart-fix-1`, the test and the fix | 46093461708 | green 6 of 6, 10 of 10 start tests each run |

The red's answer after the kill is the mechanism seen: the wait was held in the claim the kernel reset when the
anchor, its listening socket's holder, ended.

## Siblings, reported, not changed

- `Instance::attach`'s doc says it is refused `Unavailable` "at once when no daemon serves the instance". On Linux,
  under a live anchor whose daemon has not accepted, its claim waits while the anchor lives. `Instance::start` calls it
  first to refuse `AlreadyRunning`; an instance held by a live anchor is running in that sense, so the start's answer
  waits for the daemon. The doc is stale since A-468. The behaviour may be right.
- The gates job's Test step has no timeout of its own, so one hung test spends the job's six hours. With this test
  bounded, a hang of this shape fails in about a second.
</content>
</invoke>
