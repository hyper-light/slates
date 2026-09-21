# The CLI fleet fixture passes a BSD-only mktemp template

Date: 2026-09-20. Design: §4.8 fleet deployment; the real-process CLI flow.

The Linux CI command `SLATES_TEST_CLI=1 cargo test -p slates-cli --test cli
-- --test-threads=1 --nocapture` fails before starting the fleet: `mktemp -d` refuses
the template `slates-fleet-PID`, which lacks GNU mktemp's required trailing Xs.
Eight other CLI cases pass. Log: `/private/tmp/slates-linux-gates-landing-green.log`.

Use the same `.XXXXXX` template already used by `crates/client/tests/client.rs` and
include stderr in the assertion. This changes only fixture creation; keep the actual
three-process replication and owner-death assertions. The other CLI mktemp call uses
the command's default template and does not have this defect. The server fixture also
already uses trailing Xs.

The complete real-process Linux CLI flow now passes **9/9 in 7.65 s**, including the
three-process replication and owner-death case. Log: `/private/tmp/slates-linux-cli-bench.log`.
