# OCI binding lifetime belongs to its source mount

Date: 2026-09-20. Design: §4.4, §4.6 A-9, §4.7, §4.13, AC-4.11/T-4.13.

## Reproduction and impact

Run `SLATES_TEST_CLI=1 cargo test --offline -p slates-cli --test cli
oci_bindings_outlive_the_cli_and_end_with_detach_or_their_source_mount -- --exact --nocapture`.
The real CLI creates a host mount and two OCI bindings, then exits. A separate observing
connection waits until the daemon reports that every command's connection was retired.
Expected: three attachments. Actual: two. The red run takes 3.57 s on the ARM64 macOS
host; `/private/tmp/slates-oci-lifetime-red.log` records the result. The original Docker
workload also reaches a failing explicit detach (`NotFound`). Neither assertion is relaxed.

## Cause

`consumer_of` assigns OCI bindings to `Consumer::Sdk`. The issuing CLI exits after handing
the binding to its caller, so its connection is not the binding's lifetime owner. The
underlying mount has an independent bridge attachment and keeps serving. A separate bug
in client retirement hides half the failure: it visits only the client's own partition,
although a forwarded attach lives on the volume's owner partition.

## Fix plan

An OCI bind borrows a specific live host mount. Record that mount's attachment id as its
consumer, after checking the kernel source's token against the live bridge attachment,
the volume, principal and requested rights. Do not turn it into an ownerless permanent
record. A CLI exit leaves the binding intact; explicit detach removes exactly that binding;
ending the parent mount removes its bindings atomically with the parent. Recovery replays
that relationship. The attachment table's existing capacity bounds bindings.

The runtime owns its bind namespace. Removing the binding record does not unmount a
container or promise a container cache flush; callers must finish the runtime's use before
explicit detach. Parent revocation ends the authority of that mount. A mount without a
verifiable live attachment is refused, never associated by pathname alone.

The regression keeps real CLI attach/detach and adds client-retirement and parent-unmount
checks. Database tests must cover dependent removal, recovery, unrelated mounts and
refused dependencies. Cross-shard SDK retirement needs its own killed-client regression;
giving OCI bindings the right owner does not repair that separate path.

## Implemented and verified

`Consumer::Mount` records the checked parent. The database refuses foreign volumes,
principals, dependency chains and excess rights before appending; parent removal and its
dependent removals replay as one transition. The server checks the source capability and
does not mint a second mount token for a borrowed binding. Unsupported green/guest forms
refuse before creating an attachment instead of claiming an unestablished binding succeeded.

The strict regression passes in 7.90 s, including a daemon crash, fresh CLI status/detach
commands, a kernel write/read after recovery, parent revocation and read-only rights.
Log: `/private/tmp/slates-oci-lifetime-recovery-green.log`. The current complete real CLI
suite, including the Docker workload, passes **10/10 in 28.85 s** with
`SLATES_TEST_CLI=1 cargo test --offline -p slates-cli --test cli -- --test-threads=1 --nocapture`.
Log: `/private/tmp/slates-macos-current.log`. The observer only observes: all attachment
creation and explicit cleanup still use the real CLI, and every detach must succeed.

Database dependency/recovery checks also pass in both current workspace runs: Linux
1,554 passed and macOS 1,546 passed, zero failures and 14 existing ignored cases each.
These totals do not claim a Linux mounted OCI run or complete CI: remaining workflow steps,
performance failures and broader lifecycle proofs remain in `docs/wip/TBD_FIXES.md`.
