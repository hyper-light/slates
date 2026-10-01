# The container bind was recursive (AUD-29-65)

**Date:** 2026-10-01. **Audit:** AUD-29-65 (P1), with AUD-29-66's harness half. **Design:** §4.6 A-9 (the
OCI handoff), `docs/wip/oci-handoff.md`.

## Description

The recipe the daemon hands a container runtime (`crates/bridge-oci/src/binding.rs`) always said
`options: [rbind, ro|rw]`. A recursive bind carries every mount beneath the source into the container, so a
foreign filesystem mounted inside the slates mount point would be exposed through a slates attachment that
never authorized it, and `ro` on a recursive bind does not make the submounts read-only (Docker documents
writable submounts below Linux 5.12). The verifier checked only the source mount itself. The CLI's container
leg also ignored the recipe's options and passed `docker -v`, which is recursive and creates a missing
source directory on the host.

## Root cause

The recipe was written for the common case of a mount with nothing beneath it, and the harness translated it
loosely.

## Exact edits

- `crates/bridge-oci/src/binding.rs`: `options: [bind, ro|rw, private]` — non-recursive, private
  propagation.
- `crates/bridge-oci/src/verify.rs`: `HostPathRefusal::DescendantMount` — a source with any mount strictly
  beneath it is refused before the recipe exists.
- `crates/ipc/src/protocol.rs`: `HostPathReason::DescendantMount` (append-only); `crates/server/src/oci.rs`
  maps it.
- `crates/cli/tests/cli.rs`: the container leg passes the recipe as `--mount type=bind,…,
  bind-recursive=disabled,bind-propagation=private[,readonly]`, which refuses a missing source.
- `docs/wip/oci-handoff.md`, the design's OCI status.

## Proof

- `a_source_with_a_mount_beneath_it_is_refused` (every host): a mount beneath refused naming it; a sibling
  whose name extends the source's and a clean source verify.
- `the_runtime_entry_carries_the_attachment_policy`: the new options.
- T-4.13 through Docker Desktop 29.3.1 (runc) on macOS, `SLATES_TEST_CLI=1`: the container reads and writes
  through the non-recursive bind, and the read-only leg is refused `EROFS`; the binding lifetime test passes.

## Carried

AUD-29-66's kernel half (pinning the verified mount's identity through the runtime's consumption) and
AUD-29-67 (the report describing the consuming runtime) remain open.
