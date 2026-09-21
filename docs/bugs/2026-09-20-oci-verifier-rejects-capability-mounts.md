# OCI verification still expects an unauthenticated NFS export name

Date: 2026-09-20. Design: §4.6 A-9, §4.13 AUD-01, AC-4.11/T-4.13.

The native macOS CI CLI step fails `an_oci_container_consumes_the_host_attachment_through_the_runtime_bind`:
`attach --oci-source` reports `NotThisVolume` for `localhost:/oci@1.<REDACTED>`.
Log: `/private/tmp/slates-macos-gates-extra.log`. Eight other CLI test cases pass.
The authorized mount exists and its source carries the attachment capability added for
AUD-01; `bridge-oci::verify` still compares the whole source to `localhost:/oci`.
Its simulated mount fixtures also retained that obsolete source shape.

Replace that comparison with exact volume-prefix and capability-format validation:
attachment id fits u64 hexadecimal, token is exactly 16 bytes of hexadecimal, and neither
is empty. A bare export is not an authenticated volume mount. Preserve all filesystem,
mount-point and visible-overmount checks. Strip the bearer suffix from the returned
evidence and mismatch refusal; neither needs to disclose the secret. Runtime binding uses
the mount-point path, not that descriptive source field. NFS still checks the attachment's
actual authority on every filesystem request; the mount-table matcher grants none.

Use the real source shape in the existing pure regressions, add malformed/foreign-source
cases, and rerun the real macOS container workload. The red run precedes the code change.

The corrected pure suite passes 8/8. The next real run completes the shared workload but
fails cleanup: `detach` returns `NotFound` for an exited CLI's attachment. The short-lived
CLI owns that record; the persistent kernel mount owns a separate bridge attachment.
Accepting `NotFound` during cleanup was a rejected experiment: it hides whether the returned
binding survived for its consumer. The regression requires every explicit detach to succeed
and only the kernel mount's attachment to remain. Moving the workload to a persistent SDK
client would avoid the CLI lifecycle defect rather than prove it fixed; the CLI path stays
under test. The production lifetime needs a deliberate owner after the CLI exits.
Logs: `/private/tmp/slates-macos-gates-resume.log`,
`/private/tmp/slates-oci-reaper-red.log` and `slates-oci-reaper-ordered-red.log`.

## Sibling discovered by the forced-reaping probe

Waiting for both CLI records to be reaped exposes an independent cross-shard lifecycle
gap: the log shows client 14's attachment 2 removed, but the attachment count does not
reach the kernel-only count within the existing ten-second fixture deadline.
`reap_client` removes records on its own partition only; a forwarded attach may live on
another shard. Client identity in `Consumer::Sdk` also lacks the origin-host field used
by completion identities. Repair needs scoped identity, retirement across owner shards,
and ordering against in-flight forwarded work before identity reuse. Explicit cleanup in
the OCI workload exercises its own detach semantics; it does not close this lifecycle gap.
