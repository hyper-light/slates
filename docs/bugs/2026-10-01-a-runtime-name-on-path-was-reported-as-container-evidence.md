# A runtime name on the daemon's PATH was reported as container evidence (AUD-29-67)

**Date:** 2026-10-01. **Audit:** AUD-29-67 (P2). **Design:** §4.6 A-9 ("Capabilities differ by host, kernel,
runtime and VMM"), Appendix C.

## Description

The transport report carried `oci_runtime`: the first of six runtime names found on the daemon's `PATH`. Wherever
the macOS host mount was offered, the OCI row also claimed `ContainerWorkloadTest`. Neither said which runtime
profile had been tested, and a name proves nothing about it:
- whether that runtime will consume the source at all;
- whether its engine is local or remote, since Docker resolves a bind source on its daemon's host;
- whether the engine runs in Desktop's VM;
- whether it remaps users through a user namespace.

## Root cause

The report described the runtime from the daemon's side, but only the harness holds the runtime.

## Exact edits

- `crates/ipc/src/protocol.rs`: `OciRuntime` and `TransportReport.oci_runtime` are removed;
  `Conformance::ContainerWorkloadTest` is replaced by `VerifiedSourceExport`, the export's own evidence.
- `crates/server/src/transports.rs`: the `PATH` probe and its classifier are removed (`command_on_path` stays
  for Linux's FUSE helper); the OCI row reports `VerifiedSourceExport`.
- `crates/mcp/src/lib.rs`, `crates/cli/src/verbs.rs`, `crates/client/src/lib.rs`: the runtime field is removed
  from the JSON and text surfaces; the evidence name is `verified_source_export`.
- `crates/bridge-oci/src/runtime.rs` (new, pure): `admit_runtime`, `admit_endpoint`, `runtime_profile`,
  `judge`, and the typed `ProfileRefusal`.
- `crates/cli/src/oci_runtime.rs` (new): `slates oci-runtime RUNTIME` runs the bounded queries. The time bound
  is the observe budget; the answer bound is one page. It reads with `poll` against the time left and decides
  from the answer, then parses hostile answers into typed refusals.
- `crates/cli/tests/cli.rs`: T-4.13 runs the handshake before each bind and requires evidence T-4.13.

## Proof

- `crates/bridge-oci/tests/runtime.rs`, 3 tests: the tested profile; the untested engines and hosts; remote,
  unknown and unspoken endpoints and runtimes; rootless and `userns`. Each fails with its check mutated out
  (the user-namespace check; the remote-endpoint arm).
- The parser's hostile-input tests: garbage, truncation, wrong types, an empty version, server errors, and a
  non-string endpoint.
- Live on this machine: `slates oci-runtime docker` answered Docker Desktop 29.3.1 on its user socket, evidence
  T-4.13, exit 0; `podman` was refused `RuntimeUnsupported` and a missing binary `EngineUnreachable`, exit 1.
  T-4.13 passed with the handshake before its binds (`--nocapture`: "T-4.13 over 29.3.1 Docker Desktop").

## Not done here

Evidence for any other profile (a Linux engine binding the FUSE source, a rootless engine) comes only from a
container workload run through it.
