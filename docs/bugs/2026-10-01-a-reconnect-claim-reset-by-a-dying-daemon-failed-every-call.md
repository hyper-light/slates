# A reconnect claim reset by a dying daemon failed every call

**Date:** 2026-10-01. **Area:** `slates-ipc` (Linux rendezvous), the SDK acceptance tests. **Found by:** CI
runs `36837774296` (ubuntu, the Node SDK suite) and `36839369212` (macOS, the same suite), on `03a1492` and
`fd5b1e7`. **Audit:** follow-up to AUD-29-19/20 (A-56).

## Description

Two failures in the new async acceptance tests, only on CI.

- **Linux.** Every call in the restart phase failed `Ipc(OsRefused { call: "recvmsg", code: Some(104) })`,
  where all of them should have been answered after the restart.
- **macOS.** "The loop was never held: worst lateness 104 ms, bound 100 ms".

## Root cause

- **Linux: the claim mapped a reset to a hard refusal.** A daemon killed with a reconnect claim still in its
  listener's backlog resets the queued connection, so the client's non-blocking `recvmsg` returns
  `ECONNRESET` (104). `Claim::poll` (new in A-56) mapped every error except `EAGAIN`/`EINTR` to `OsRefused`.
  `Client::try_reconnect` passes `OsRefused` through as an error, and the driver's recovery fails every call
  on an error. A reset is a daemon that never answered, which a reconnect retries. The hello `send` after
  `connect` had the same mapping for `EPIPE` and resets.
- **macOS: the test measured its own harness.** The test's ticker measures how late the loop runs ready
  callbacks, but the harness itself blocked the loop with `spawnSync` for `pgrep` and the bootstrap. Those
  process spawns take tens of milliseconds on a loaded runner. The Python test had the same pattern
  (`subprocess.run`).

## Fix

- **Claim errors.** `Claim::poll` and the hello `send` map `ECONNRESET`, `ECONNREFUSED`, `EPIPE` and
  `ENOTCONN` to `DaemonUnavailable` ("the daemon closed the rendezvous before answering the claim"). Other
  errors stay `OsRefused`.
- **Test harnesses.** Both acceptance tests spawn their helpers asynchronously: Node `runAsync` over
  `spawn`, Python `asyncio.create_subprocess_exec`. The ticker now measures only the SDK.

## Tests

- `a_claim_whose_daemon_died_before_answering_is_unavailable_not_refused` (`crates/ipc/tests/rendezvous.rs`).
  **Red first on Linux:** `OsRefused { call: "recvmsg", code: Some(104) }`, in Docker `rust:1.98.0`. Now green
  on Linux and macOS.
- The Node async suite passes 3/3 on Linux (Docker, Node 20, io_uring available) and on macOS. The Python
  async suite passes on macOS.

## Siblings reported

- CI was red on these commits for hours before I looked. Every push's run must be read before the next item.
- Other recent red runs are not caused by this, and are under diagnosis:
  - `a_holders_replicas_cap_at_its_unpromised_capacity_through_churn_and_retire_to_the_survivors_baseline`
    (since `dd66fe7`);
  - two takeover tests on `9a46ad5`;
  - `three_daemons_form_a_fleet_and_the_survivors_retire_a_dead_node` on `34b48f9`;
  - `a_tight_quota_refuses_the_same_writes_as_the_model` on `5cce86a`;
  - a packaging `ENOENT` on `target`.
