# An empty-wake test mistook an armed client for a sleeper

Observed in main CI 41558297045, TSan job 125662475876, at 01:18 UTC on 2026-11-03 (2026-11-02 locally).
The existing regression serves AC-2.1 / §4.7 Wake strategy:
`a_wake_without_a_reply_is_counted_and_the_client_parks_again` in `crates/ipc/tests/rings.rs`.

## Failure and cause

The IPC ring target passes seven cases and fails this one in 5.01 s. The peer reports `the client did not park
again`; the client returns `DeadlineExceeded`. These are the original five-second guards. The failure is the
test's assertion at that point, rather than a sanitizer race report.

The fixture treats `client_parked == 1` as proof of a kernel sleeper. The flag is raised earlier: `ClientEnd::wait`
clears its reply stamp, raises the flag, fences, captures the wake word, then rechecks its ring. One permitted
schedule explains the timeout: the peer sees the flag, writes its marker and advances the word before the client
captures it; the wake finds no sleeper. The client then captures the advanced value, finds no reply, and waits on
that value. The peer waits for another park before sending the real reply, so both guards expire.

A real reply has its ring entry for the mandatory recheck to find (§4.7, A-44); the fixture's empty wake has no
such condition. An armed flag cannot establish the empty wake this test requires. The wake API already reports
whether the kernel found a sleeper (`wake::wake_one`), as A-31 uses to confirm a measured wake.

## Exact correction

On Linux and macOS, the injector uses the existing bounded `take_within` helper until `wake_one` confirms a
sleeper. It does not advance the word ahead of the wait. The empty kernel wake then makes the client return and
repark, clearing the peer's marker before the real reply is sent. One diagnostic line records the confirmation.
Windows retains its auto-reset Event signal, which already survives the preparation window.

The five-second guard, exactly-one unanswered-wake assertion, reply bytes, request id and park accounting are
preserved. The peer is joined before a client-wait error is propagated. Only the fixture changes;
`ClientEnd::wait` and its ring-recheck protocol retain their rules.

## Verification

The failing TSan job is the before-fix counterexample. Before correction, 100 native Linux runs pass (14.737 s
including the build); that sample does not reproduce the instrumented failure. The temporary diagnostic records
the kernel's wake result. Log: `~/.cache/slates-s9-ipc-empty-wake-red-native.log`.

After correction, 100 native Linux runs pass, then all nine ring cases pass (0.03 s). The capped unit takes
8.189 s and verifies the transferred source hash before and after testing:
`c618c33ff22ad8fbc424f056b363a31285dccbac55f8b3f374731ff43542c433` (before the final comment clarification).
Log: `~/.cache/slates-s9-ipc-empty-wake-green-transfer.log`, adacachy, 2026-11-02 locally.

```sh
cargo +1.98.1 test --locked -p slates-ipc --test rings \
  a_wake_without_a_reply_is_counted_and_the_client_parks_again -- --exact --nocapture --test-threads=1
cargo +1.98.1 test --locked -p slates-ipc --test rings -- --nocapture
```

On this aarch64 Mac the complete ring target passes nine cases in 0.53 s after a 34.56 s build:
`cargo +1.98.1 test --locked --offline -p slates-ipc --test rings -- --nocapture`.
Log: `~/.cache/slates-scratch/s9-diag/ipc-empty-wake-mac-native.log`.

A local nightly sanitizer build was stopped and reaped at its 90-second cap while rebuilding std; it reached no
test, so it supplies no local sanitizer verdict. The complete CI lane, including its race canary, remains required.
No toolchain or component was installed.

The final Linux compile/documentation chain passes in a capped 25.159 s unit: `cargo +1.98.1 fmt --check`,
the focused empty-wake case, workspace/all-target clippy with warnings denied, `cargo +1.98.1 xtask check`,
then private workspace docs with `RUSTDOCFLAGS="-D warnings -A rustdoc::private_intra_doc_links"`. The source
packet is hashed before and after the chain. Log: `~/.cache/slates-s9-ipc-final-compile-gates.log`.

An additional macOS workspace gate chain was capped before its initial `cargo +1.98.1 fmt --check` returned.
It produced no diagnostic and was stopped and reaped (exit 124; 289.365 s including termination). It supplies
no extra macOS gate verdict. The Linux workspace gates above and the completed macOS ring target are the
local evidence; final main CI still decides the complete platform matrix.

## Sibling timing assertion still open

A subsequent all-ring run over the final source passes the corrected empty-wake case but fails the unchanged
`a_client_learns_its_wake_from_the_parks_a_reply_ended`: 7 of 32 trips teach its estimate, below its required
16, although all 32 park and deliver their replies. Eight of nine cases pass in 0.53 s. The capped unit exits
101 in 7.858 s; adacachy reports zero of 32 cores idle. Command: `cargo +1.98.1 test --locked -p slates-ipc
--test rings`. Log: `~/.cache/slates-s9-ipc-final-gates.log`.

That fixture waits one millisecond after the armed flag before replying and assumes most clients are asleep by
then. The delay does not establish that condition under preemption. This failure leaves the learning fixture's
timing assumption open; it does not justify weakening its sample assertion or changing the production learner.
The focused empty-wake check and compile/documentation gates are recorded separately. Main CI must still pass
the whole target.
