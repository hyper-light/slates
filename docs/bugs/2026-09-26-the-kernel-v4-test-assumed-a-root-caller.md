# The kernel NFSv4 test assumed a root caller

Date: 2026-09-26. Contracts: §4.6 A-38 (the server never refuses an owner a root caller gives), A-26
(no device nodes), CLAUDE.md §4 (a test must not assume the host's conditions). Found by CI run
36296710241 (`2bf912e`), step "Linux kernel NFSv4.1/4.2 client".

## Symptom

```
thread 'the_linux_kernel_nfsv4_client_mounts_and_works_a_volume' panicked at
  crates/server/tests/nfs_v4_kernel.rs:427:31:
chown 65533:65532: Operation not permitted (os error 1)
```

It passed in the privileged Linux container used to develop A-38.

## Root cause

GitHub's Linux runner runs the test as the unprivileged `runner` user with passwordless `sudo`. The
test uses `sudo -n` only for `mount`. The container ran as root. Three steps A-38 added need root,
and the test made them as its own caller:

- `owners_change_as_numbers` gave files to foreign owners. For a non-root caller POSIX refuses that
  with `EPERM`, and the server rightly did.
- `truncating_open_needs_write_permission` chowned its file and started children with `Command::uid`,
  which needs root to change ids.
- `special_names` asserted that a device `mknod` is refused. As non-root, the local kernel's
  `CAP_MKNOD` check refused it before any request reached the server, so the assertion passed without
  testing the server's A-26 refusal.

The server was correct. The test assumed a root caller.

## Fix

Each root-only step now runs as root the way `mount` already did, through `privileged` (direct when the
test is root, else `sudo -n`): `chown_as_root`, children through `setpriv --reuid --regid
--clear-groups` (`as_ids`), and the device through `mknod`. `owners_change_as_numbers` hands the object
back to its owner, so the caller's later steps (explicit times) still act on its own file. Each
truncating-open case uses its own file, created and moded by the caller before root gives it away.

## Evidence

In a privileged `rust:1.98` container with a `runner` user and passwordless `sudo`, the test binary run
as `runner`: the old test fails with CI's exact panic, and the new one passes. The new one also passes
as root (2026-09-26). No other kernel-mount test assumes a root caller (`crates/cli/tests/nfs_v4_restart.rs`
checks root only to choose how to mount).

## Exact edits

`crates/server/tests/nfs_v4_kernel.rs`.
