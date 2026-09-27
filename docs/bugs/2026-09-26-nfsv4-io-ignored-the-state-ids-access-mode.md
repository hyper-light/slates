# NFSv4 I/O ignored the state id's access mode, and exclusive creates lost their mode

**Date:** 2026-09-26. **Found:** pjdfstest and the workloads over the NFSv4.2 conformance transport
(privileged Linux container): `open/07.t` 6, 8, 10 and 23, `chmod/12.t` 3–12, and git's hook templates
arriving mode 644 instead of 755.

## Description

- An `O_RDONLY|O_TRUNC` open by a file's owner, whose mode (0477) denies the owner writing, truncated
  the file. The Linux client sends the truncate as a SETATTR of size 0 under the read-only open's state
  id; the server accepted it. Later truncating opens by others then found an empty file and the kernel
  sent nothing, so 8, 10 and 23 failed after 6.
- Every `O_EXCL` create took the default mode 0644.

## Root cause

- `FileState::check_io` checked only that the state id named an open or lock of the file. It ignored
  the operation's access (RFC 8881 §9.1.2: a write-type operation needs an open that allows writing,
  `NFS4ERR_OPENMODE`) and, for the special state ids, other opens' share denials (`NFS4ERR_LOCKED`).
  The v3 layer's I/O owner override then let the owner's truncate through.
- `suppattr_exclcreat` was empty. The Linux client sends in an EXCLUSIVE4_1 create only the attributes
  it names, and sets no mode afterwards.

## Impact

A file could be truncated through a descriptor opened read-only by its owner; a special state id's
WRITE ignored another client's DENY_WRITE; every exclusive create over NFSv4 lost its mode (git's
executable hooks, pjdfstest's `create`, any `O_EXCL` tool).

## Exact edits

- `v4/files.rs`: `check_io` takes the wanted access (`IoWant`) and returns what authorizes the I/O
  (`IoAuthority`): an open's access mode is enforced, a READ on a write-only open and every special
  state id are checked against other opens' denials.
- `procedures.rs`: `state_io` computes the wanted access (a SETATTR writes when it sets the size) and
  serves READ, WRITE and SETATTR under the authority; an open authorizes the I/O it allows without the
  mode check, a special state id keeps the v3 rule. STATE_CHECK carries the wanted access.
- `access.rs`: `setattr_denial` takes the authority.
- `v4/compound.rs`, `v4/v42.rs`: `check_state` sends the access and checks special state ids too;
  READ_PLUS and COPY move their bytes under their state ids.
- `v4/attr.rs`: `suppattr_exclcreat` is the settable set.
- Tests first: the kernel test's `exclusive_create_keeps_its_mode` (failed: 0644 for 0755) and
  `truncating_open_needs_write_permission` (the owner's case of `open/07.t`); `tests/v4.rs`
  `a_state_ids_access_mode_and_other_opens_denials_govern_io`.

Sibling check: SEEK's mode check at the v3 layer remains (lseek needs no permission; an open's state id
is checked before it, so only a special state id meets the mode rule there).
