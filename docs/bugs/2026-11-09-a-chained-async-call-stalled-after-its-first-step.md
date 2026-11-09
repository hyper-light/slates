# A chained async call stalled after its first step

**Found** 2026-11-09 by CI's diag list-3 (A-263 on B8, 8548d54c): both SDKs' async suites failed on Linux and macOS,
Python's with `SlatesError: Stalled { after_ns: 10000000000 }`, at the async listing of 250 volumes A-263 added. The
A-263 commit's message said both SDKs' async lists were green over 250 volumes; they had not been run before that diag
(diag list-2's SDK legs never ran). That claim was wrong, and this is its correction.

**Root cause.** The async driver (`crates/client/src/driver.rs`) reports a call `Ready` once, until the binding finishes
it (`reported`, added 2026-10-01 so a held reply is not reported twice). A chained call answers its poll `None` after a
step: a listing's page that is not the last, while the client sends the next page under the call's word (the large edit
chains the same way). The binding kept the call and did not finish it, so the driver never reported its next reply, and
the call failed `Stalled` at the reply deadline. The sync client and the client's own async tests poll the word
directly, without the driver, so nothing below the SDKs saw it.

**Red** (`crates/client/tests/driver.rs`, `a_chained_call_is_reported_at_each_step_until_it_ends`: 250 volumes listed
through the driver as an SDK lists them): failed `no reply within 1000000000 ns` after 1.0 s.

**Fix.** `Driver::continued(ticket)`: a binding whose poll answered `None` keeps the call, and the driver reports its
next reply again, the reply deadline counting from the step. Both SDKs call it (`crates/sdk-python/src/lib.rs`
`settle_one`, `crates/sdk-node/async.mjs` `_settle` through the addon's `continued`).

**Green** on this Mac: the driver test 3 of 3; the Python SDK suites 6 of 6 and the Node SDK suites 6 of 6 against the
built daemon (`SLATES_TEST_RESTART_SLOWDOWN=10`, as CI runs them).
