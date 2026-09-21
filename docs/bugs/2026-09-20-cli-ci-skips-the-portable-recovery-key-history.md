# Linux CLI CI omits the recovery-key test's RAM directory

Date: 2026-09-20 (local). Design: §4.13, T-2.14, AUD-07, R1/R5.

The local reproduction of CI's CLI command reports ten passing test functions but prints
`skipping portable recovery CLI flow: set SLATES_TEST_RAM to a RAM-backed directory`.
The workflow sets `SLATES_TEST_CLI` alone. The recovery-key test correctly requires RAM
scratch for its operator-key fixtures, so it returns without testing its real CLI history.
Log: `/private/tmp/slates-linux-delta.log`, 2026-09-21 00:48 UTC.

Supply `/dev/shm` as `SLATES_TEST_RAM` for the Linux CLI step. Keep the original test,
callers and assertions: separate CLI processes must refuse absent, wrong and oversized
keys, accept the correct node key, change the recovery group and accept an idempotent retry.
The macOS command keeps its existing mount and named-anchor tests; no disk directory is
substituted for the portable test's RAM requirement there.

Independent local execution with that environment passes the actual history in **0.42 s**:
`SLATES_TEST_CLI=1 SLATES_TEST_RAM=/dev/shm cargo test --offline -p slates-cli --test cli
recovery_approval_uses_the_provisioned_node_key_across_processes -- --exact --nocapture`.
Run inside the approved disposable Linux io_uring container as `tester`;
`/private/tmp/slates-linux-cli-recovery-key.log` records the real anchor startup and the
one executed test. This is a workflow coverage correction, with no test or daemon change.
