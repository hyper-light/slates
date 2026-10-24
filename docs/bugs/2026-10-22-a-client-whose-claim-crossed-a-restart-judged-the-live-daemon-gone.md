# A client whose claim crossed a restart judged the live daemon gone

**Status:** fixed 2026-10-22 (A-143, approved by Ada in plan mode). Root cause confirmed by log lines.
**Severity:** macOS and Windows; Linux is not affected.
- After a daemon restart that a client's reconnect claim crossed, the client misjudges its live daemon. A silent
  daemon is reported `DaemonGone` after the reconnect budget instead of `Stalled` at the reply deadline, and the
  client spends that budget reconnecting to a daemon that is already its own.
- The mirror race, fixed with it, could leave a client watching the wrong process for good: every call `Stalled`,
  and no reconnect.

**Found:** 2026-10-22, while verifying the §8a-sdk-loop fix on macOS at load 14.5. The async SDK test's phase (2)
ended `DaemonGone { after_ns: 2000000000 }`, once in 15 runs.

## What happens

On macOS and Windows a client tells a dead daemon from a slow one in two ways (`Liveness::daemon_gone`,
`crates/ipc/src/rendezvous.rs`):
- the daemon's process exited, which its exit watch reports;
- the start stamp in the bootstrap object's header is no longer the one the client holds.

The client took those two things at two different moments:
- the stamp when its claim **began** (`Claim::begin` → `open_bootstrap`);
- the process from the header's daemon pid when the claim **completed**.

A-114 (2026-10-06) made the anchor hold the rendezvous so that a claim survives a restart: "with a daemon restarting,
its claim waits and the next daemon answers it". Here is what follows:
1. A claim begins while the killed daemon's stamp is still published.
2. The next daemon answers it, so the client holds the next daemon's process and the dead daemon's stamp.
3. The first reply that stalls afterwards compares that stamp with the header's, which is the live daemon's, finds
   them different, and reads "gone".

The window opens when the client's reconnect begins after the old daemon's death and before the new daemon publishes.
A restart under load widens it.

## The evidence

### By use, macOS

The Python SDK over a real anchor and daemon (`/tmp/slates-repro/sdk_stamp_repro.py`, scratch). The held arm:
1. stop the anchor and kill its daemon;
2. call;
3. after 1.3 s, past the 1 s reply deadline, so the client has found the daemon gone and begun its claim, resume the
   anchor;
4. once the call has crossed the restart, stop the new daemon and call again.

The plain arm does the same without the anchor stop.

| | held arm, call to the stopped daemon | plain arm, call to the stopped daemon |
|---|---|---|
| before | **`DaemonGone` 3 of 3** (after 3.06–3.16 s: the 1 s reply deadline plus the 2 s reconnect budget) | `Stalled` 3 of 3 (1.00–1.03 s) |
| after | `Stalled` 3 of 3 (1.002 s) | `Stalled` 3 of 3 (1.002 s) |

Every run had exactly one restart and one reconnect, so the stopped daemon was the client's own. That refutes the
first guess, a second restart between the phases. In the held arm the crossing call took 1.485 s after the fix,
which shows it waited for the resumed anchor's daemon.

### The log lines

Temporary `eprintln!` lines in `Liveness::daemon_gone` and `Claim::complete`, removed before the fix. Stamps are
shortened to their last digits.

The held arm:

    claim answered: began with 1792687213142448000, published now Some(1792687213142448000)
    daemon_gone: exited true, recorded 1792687213142448000, published Some(1792687213142448000)
    claim answered: began with 1792687213142448000, published now Some(1792687215559132000)
    daemon_gone: exited false, recorded 1792687213142448000, published Some(1792687215559132000)

The plain arm, where the reconnect began after the new daemon published:

    claim answered: began with 1792687226713171000, published now Some(1792687226713171000)
    daemon_gone: exited false, recorded 1792687226713171000, published Some(1792687226713171000)

### The red test

`a_claim_begun_before_a_restart_judges_the_daemon_that_answered_it_alive` (`crates/ipc/tests/rendezvous.rs`). The
parent holds the rendezvous as the anchor does. A child under it does this:
1. daemon A opens the rendezvous;
2. a claim begins;
3. A is dropped unanswered;
4. the child waits until the wall clock has passed A's opening, so the stamps differ;
5. daemon B opens the rendezvous and answers;
6. the client must judge B alive.

It failed on macOS before the fix, deterministically, in 0.08 s ("the daemon that answered the claim is alive and
published, and the client judged it gone"). It passes after. It passes on Linux, whose code the fix does not touch.

The first version of the test was red on Linux for the test's own reason. Its serving helper dropped the daemon's
ends of what it accepted before the liveness check, and on Linux a closing control socket peer *is* the daemon's
death. The helper now hands those ends back to the caller. macOS had passed it only because its liveness does not
look at them.

## Root cause

A connection's liveness was bound to whichever daemon the header named at two different moments, instead of to the
daemon that made the connection's region. It was introduced with A-114 (2026-10-06). Before that, a restarted daemon
recreated the bootstrap object, so a claim made on the old one was never answered and this state could not arise.

## What it was not

- **A second restart between the phases:** one restart and one reconnect in every run.
- **The SDK:** the client's verdict comes from `slates-ipc`, and the red test reproduces it with no SDK at all.
- **Linux:** a connection's liveness there is its own socket's peer.

## Exact edits

In `crates/ipc/src/rendezvous.rs`, the macOS and Windows module:
- **Slot layout.** `AT_ANSWERED_STAMP` (64) and `AT_ANSWERED_PID` (72) go in the claim slot's spare bytes, which
  nothing wrote before. The `SLOT_BYTES` layout doc names them.
- **`write_answerer` and `read_answerer`.** A stamp of zero is refused as
  `Layout { "an answered claim does not name its daemon (a daemon of another version)" }`.
- **`Listener::stamp`** is the stamp `open` publishes. `answer` writes it, with `current_pid()`, before `READY`.
- **`Claim`** keeps no stamp, and `open_bootstrap` returns none (it still refuses an unpublished object).
- **`Claim::complete`** builds the `Liveness` from the answer's stamp and pid. An answer that names no daemon is
  refused after its slot is finished, so the slot is still freed.
- **The `Liveness` doc** says both come from the answer.

In `crates/ipc/tests/rendezvous.rs`:
- the test, and its child role `rendezvous_restart_child`;
- `serve_until_connected` is split from `poll_while_serving` and returns the daemon's ends.

Docs: `SLATES_DESIGN.md` §4.7 status, the A-114 note and A-143; GAPS §8a-sdk-stamp.

## Verification

- **macOS:**
  - `slates-ipc`: every test file green (the lib 24, 1 ignored), and the new test red before the fix and green
    after;
  - `slates-client`'s `client` 13 of 13 and `driver` 2 of 2, at load 24;
  - the by-use reproduction above;
  - workspace clippy `-D warnings`, the doc gate, `cargo xtask check` and `cargo fmt --check`, all clean.
- **Linux (arm64 container):**
  - `slates-ipc` green, the new test included;
  - workspace clippy clean on the first run, and `slates-ipc` clippy clean after the test's correction.
- **Windows:** the cross-lint of `slates-wire`, `slates-mem`, `slates-rt` and `slates-ipc` for
  `aarch64-pc-windows-msvc` is clean. CI's Windows lanes run the test itself.

## Siblings

- **Removed 2026-10-23 (A-159).** The header's daemon pid (`AT_DAEMON_PID`) was written by every daemon and read by
  no client of this version; only clients built before A-143 read it. Under CLAUDE.md's rule 7 a write kept for older
  versions is a compatibility shim. `docs/publish.md` ships one version, so removing it breaks only a combination that
  is not shipped.
- **The header stamp stays.** `begin` and `daemon_gone` read it as the instance's current daemon, which is what they
  mean.
