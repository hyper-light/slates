# Container identity semantics were unstated (AUD-29-74)

**Date:** 2026-10-01. **Audit:** AUD-29-74 (P2). **Design:** §4.6 A-9, §4.13.

## Description

The container tests ran every container as the mounting user. Nothing stated or tested what happens for any
other identity: another uid, root, a supplementary group, a remapped user namespace or SELinux labels. A
harness could not know whether a container identity grants access, is denied, or is silently translated.

## Root cause

The identity contract was never measured, so it could not be stated per profile.

## Exact edits

- `crates/bridge-oci/src/runtime.rs`:
  - `IdentityRule::HostUserThroughShare`, carried by the tested profile;
  - `SelinuxLabelsUntested`, judged from the engine's `name=selinux` security option.
- `crates/cli/src/oci_runtime.rs`: the handshake prints the profile's identity rule.
- `crates/cli/tests/cli.rs`:
  - `run_in_container_as` (any user, any supplementary groups);
  - the test `a_containers_identity_reaches_the_export_as_its_profile_states`.

## Proof

- Measured through Docker Desktop 29.3.1 on macOS on 2026-10-01, as 501:20, 0:0, 1000:1000, and 501:20 with
  group 12345. Each container wrote and saw its own ids. A 0700 directory kept 0700. The host saw every
  object as 501:20.
- The by-use test asserts this for the three non-matching identities, and asserts the handshake's stated
  rule.
- `crates/bridge-oci/tests/runtime.rs` asserts the SELinux refusal and the rule. It was red before the type
  existed.

## Not done here

A rule for any other profile (a Linux engine binding the FUSE source, Kubernetes fsGroup) comes only from a
measured workload through that profile. Until then those profiles are refused `ProfileUntested`.
