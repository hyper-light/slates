# Explicit shard counts keep the automatic memory split

Date: 2026-09-20 (local). Design: §4.1, §4.2, §4.3; D-11, D-12; AC-0.10, AC-2.11.

## Reproduction and impact

The Linux SDK packaging run builds and validates the Python 3.14.3 wheel, but the unchanged
async lifecycle fails its eight simultaneous 8 MiB creates with
`BudgetExceeded { available: 6824 }`. The container has four CPUs and a 4 GiB memory bound;
the test starts the real CLI anchor with `--shards 1`.

Command wrapper: `/private/tmp/slates-run-linux-sdk.sh`; log:
`/private/tmp/slates-linux-sdk-ci.log` (five Python tests, one error, 0.355 s).

The diagnostic runs fresh and create/snapshot/resize/destroy histories, then serial and
concurrent admission with exactly the same requests. All four admit five volumes and refuse
three. The live status reports 165,700 inode slots; five volumes reserve 158,875, leaving
6,824 beyond the copy-up headroom. Content reservation is only 41,943,040 bytes out of
268,435,456 usable bytes. After destroy, both committed counters return to zero. Neither
an async reply mismatch nor leaked destroy reservations explain this failure.

Commands: `/private/tmp/slates-diagnose-linux-sdk.sh` and
`/private/tmp/slates-sdk-admission-diagnose.py`; log:
`/private/tmp/slates-linux-sdk-admission-diagnosis.log`. The diagnostic shell's printed
`Original async exit: 0` is wrong: its helper printed after a deliberately captured failing
command and replaced that command's status. The unittest traceback and the original
fail-fast packaging run establish the failure; that diagnostic status is not pass evidence.

## Root cause

`DaemonConfig::derive` divides effective memory by the automatic runtime shard count
(three here), then derives content, metadata, slab, client and retained-state capacities.
`with_shards(1)` changes only the runtime count and partition count afterward. Its surviving
shard still receives 477,218,588 bytes per class instead of its full share. Choosing more
shards has the opposite defect: it multiplies those already-derived shares past the host's
bound. Anchor and daemon both use the same incorrect override.

## Correction

Select the operator's optional shard count before any dependent derivation. Keep the
existing automatic choice when absent and the existing unpinned scheduling of explicit
counts. Replace `with_shards` with a construction-time argument, so no caller can retain
capacities derived for a different count. Update callers mechanically; no SDK request,
assertion, concurrency, container memory or timeout changes.

Two regressions run the real daemon and ring on a four-core, 4 GiB profile. With one shard,
all eight unchanged SDK-sized volumes must be admitted. With six shards, the content and
metadata reported by the running shards together must remain within the host's bound.
Before the fix, the first fails at create 5 with the same 6,824-slot refusal. The second
reports 4,758,318,216 bytes against the 4,294,967,296-byte limit. Both fail in 1.43 s;
both pass after the fix in 1.42 s on macOS. Command: `cargo test --offline -p slates-server
--test daemon choosing_ -- --test-threads=1 --nocapture`; logs:
`/private/tmp/slates-shard-selection-red.log`, `/private/tmp/slates-shard-selection-green.log`.

The unchanged Linux packaging command now passes with the same four CPUs and 4 GiB bound:
five Python cases (0.384 s), five direct Node cases, and three installed npm cases
(0.192 s), with zero skips. Python 3.14.3 wheel and source distribution pass strict Twine
checks. Log: `/private/tmp/slates-linux-sdk-ci-green.log`.

The subsequent Linux caller sweep passes 1,557 workspace cases, zero failures, 14 existing
exclusions, including all 49 fleet histories (271.81 s). Strict Clippy, xtask, the non-root
FUSE regressions and the real CLI step pass. The CLI step reports ten passing functions
(8.36 s), with platform-specific skips; its portable recovery-key history now executes.
Command/log: `/private/tmp/slates-run-linux-shard-selection.sh`,
`/private/tmp/slates-linux-shard-selection.log`.

macOS passes 1,548 workspace cases, zero failures, 14 existing exclusions, including all
48 fleet histories (321.69 s). Strict lint/structural checks, the real CLI binding/restart/
strict-detach flow (ten functions, 28.19 s), and all thirteen installed SDK cases pass.
The portable operator-key CLI history skips on macOS without its RAM directory; the Linux
run above executes it. Command/log: `/private/tmp/slates-run-macos-shard-selection.sh`,
`/private/tmp/slates-macos-shard-selection.log`.

## Sibling findings

`BudgetExceeded` on the wire omits the resource dimension, so its available count can mean
bytes or inode slots. The existing diagnostic logs identify versions; a clearer closed
wire refusal is still owed. Hand-edited public configurations still need startup validation
against the effective capacity (already tracked in §4.2). This fix does not establish that
separate defense or pressure-driven admission.
