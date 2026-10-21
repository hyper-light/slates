# A signalled daemon had not stopped yet, and answered the call the test required to stall

**Status:** fixed in the tests (`_stop_and_wait` / `_stop_group_and_wait` in the Python SDK test, their
twins in the Node SDK test). **No product change: the product was right.**
**Severity:** two intermittent CI failures, both in the SDK acceptance tests. Nothing shipped was wrong.
**Found:** 2026-10-21, chasing the async SDK intermittent recorded in
`docs/bugs/2026-10-21-the-async-sdk-intermittents-and-how-to-reproduce-them-on-linux.md`.

## What happens

`kill` and `killpg` **queue** a signal and return. They do not wait for the target to stop. A daemon that
has been sent `SIGSTOP` but has not yet been scheduled is still a live daemon, and it still answers.

Both SDK acceptance tests stopped a daemon and then immediately required silence:

```python
group = os.getpgid(anchor.pid)
os.killpg(group, signal.SIGSTOP)
with self.assertRaisesRegex(slates.SlatesError, "Stalled"):
    await client.list()
```

When the daemon served the call inside that window the call returned a normal empty list, and the test
failed with `SlatesError not raised`. The daemon answering was correct; the test's premise — "a live but
silent daemon" — had simply not been established yet.

## The evidence

A Linux container, the scenario constructed directly (phase 1 then phase 2, nothing else), 100 trials.
The harness printed each daemon's pid, its process group and its state, read immediately after the group
stop:

```
trial  49 FAIL  reconnects=1 refused=1 failures=0  -> ANSWERED []
        anchor=924 group=924 daemons=[(934, 924)] states=['R']
```

The daemon (934) **was** in the anchor's group (924), the group **was** signalled — and the daemon's
state was **`R`, running**. It then answered.

Rates measured the same afternoon, same container, same build:

| harness | failures / 100 |
|---|---|
| the call made straight after `killpg` | **5** |
| with one `pgrep` (a few ms) inserted before `killpg` | **1** |
| waiting until every group member reads state `T` | **0** |

The middle row is the point: a few milliseconds of unrelated work cut the rate fivefold. That is a
timing window, not a logic error.

The rate change is corroboration, not the proof — see the wrong turn below for why that distinction
matters here. The proof is the state reading above, and the fact that once every member of the group
reads `T` there is nothing left that could answer.

Verified with the real test, not only the harness: `test_sdk_async.SlatesAsyncEveryCallEnds` run 12
times in the same container, **12 passed, 0 failed**.

Commands (arm64 Linux container on an Apple Silicon Mac, 6 CPUs, 8 GB, 2026-10-21; the repo mounted
read-only with the target directory redirected to a host scratch path):

```
cargo build --locked --bin slates
maturin build -m crates/sdk-python/Cargo.toml --out /tmp/dist
pip install /tmp/dist/slates-*.whl
SLATES_DAEMON=/tmp/lintarget/debug/slates python -m unittest test_sdk_async.SlatesAsyncEveryCallEnds
```

## This also explains a second, separately tracked intermittent

`docs/wip/GAPS.md`, "2026-10-04: an intermittent async-SDK overflow assertion": phase 1 stops the daemon,
submits three times the outstanding limit, and asserts `refused > 0`. CI run 37183276944 got
`0 not greater than 0`. Its first listed suspect was "`_daemon_pids` named no live daemon, so nothing was
stopped". The measurement sharpens that: the daemon **was** named and **was** signalled — it had not
stopped, so it drained the calls, freed the slots, and nothing overflowed. One cause, two symptoms.

## The fix

Stopping now means stopped. `_stop_and_wait(pids, what)` signals and then waits until each pid reads
state `T` (or is gone). `_stop_group_and_wait(binary, instance, anchor_pid)` stops the group, confirms
the **anchor** stopped first — only a running anchor can fork a replacement the group signal has already
swept past, so once it is stopped the set of daemons is closed — then stops any daemon still running and
waits for all of them. Both wait through the existing `_wait_for`, so the deadline stays
`STARTUP_SECS`; no new constant was introduced (R3).

State is read with `ps -o state= -p <pid>`, which works on Linux and macOS; these tests are not platform
gated, so `/proc` could not be used.

Applied in the same change to the Node SDK test, which had the identical shape, and to **phase 1** of
both, which claimed "stopped first, so the calls stay outstanding" without waiting.

## What was wrong on the way here, because it cost a day

Three hypotheses were tested and refuted before this one; they are recorded in the companion doc. A
fourth was wrong and nearly shipped, which is the part worth keeping:

**A reconnect was blamed for reusing request words.** The story was that a request is named
`(client_id << 32) | sequence`, that a reconnect attaches a fresh endpoint whose `next_request` starts at
0, and that the first call after a reconnect therefore redraws the first word before it and is answered
from the client's `pending` buffer. A fix was written (`pending.clear()` at both reconnect sites), and
the harness went from 5 failures in 100 to **0 in 100**.

It was wrong on both counts:

- **The mechanism is false.** The word is drawn from `Client::sequence`, a client-level counter that a
  reconnect never resets — the endpoint's `next_request` is a *ring index*, not the word. Printed across
  a reconnect, the words ran 1, 2, 3, 4 straight through.
- **The 0 in 100 was luck.** A detector was added that printed whenever a poll was answered out of a
  buffer entry that had survived a reconnect. With the clearing disabled, the bug fired 5 times in 100
  and the detector printed **zero times**. No buffered reply ever answered a post-reconnect call.

The regression test written for it passed with the fix disabled — a red-check caught that before the
commit, which is the only reason this was looked at again. The fix and its test were removed; the clear
would also have discarded replies the exactly-once path legitimately reuses, since `resend_awaited`
re-sends outstanding calls under their *original* words.

Lesson, in one line: 5-in-100 going to 0-in-100 is not proof of a mechanism. `P(0 of 100 | p = 0.05)` is
about 0.6%, which looks convincing and still lost to a single direct measurement of the claimed cause.

## Exact edits

- `crates/sdk-python/tests/test_sdk_async.py` — `_state_of`, `_stop_and_wait`, `_stop_group_and_wait`;
  phase 1 and phase 2 use them.
- `crates/sdk-node/tests/sdk_async.test.mjs` — `stateOf`, `signalIfAlive`, `stopAndWait`,
  `stopGroupAndWait`; phase 1 and phase 2 use them.
- `docs/wip/GAPS.md` — the 2026-10-04 overflow entry closed; failure B of the intermittents entry closed.
- `docs/bugs/2026-10-21-the-async-sdk-intermittents-and-how-to-reproduce-them-on-linux.md` — B marked
  root-caused, pointing here.
