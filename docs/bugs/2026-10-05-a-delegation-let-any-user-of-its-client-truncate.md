# A delegation's state id let any user of its client truncate a file without write permission (2026-10-05)

## Description

CI's Linux conformance lane, pjdfstest over NFSv4.2 (`f706876` and later): `open/07.t` 6, 8 and 10 opened a file
`O_RDONLY|O_TRUNC` as users the mode denied writing (the owner of a 0477 file; a group member of a 0747 one; an other
user of a 0774 one) and got 0 instead of `EACCES`; 23 then found the file truncated. The 2026-09-26 fix of the same
cases (docs/bugs/2026-09-26-nfsv4-io-ignored-the-state-ids-access-mode.md) held in a local run; CI's slower runner
let the file leave the delegation quiet period between pjdfstest's `chmod` and `open`, so the client held a delegation.

## Root cause

- Under a delegation the Linux client opens locally, checking only the open mode's access for the opening user
  (`nfs_may_open` asks for read or write by `O_ACCMODE`, never for `O_TRUNC`), and sends the truncate as a SETATTR of
  size 0 under the delegation's state id.
- `FileState::check_io` answered a delegation's state id with `IoAuthority::Open`, the authority of an open, and a
  SETATTR under `Open` skips the permission check, since a descriptor keeps its access whatever the mode becomes. But
  a delegation's state id stands for the whole client, not for one user's permission-checked open, so any user of the
  client truncated through it.

## Impact

A privilege bypass between users of one NFSv4 client: while the client held a write delegation of a file, any local
user could truncate that file regardless of its mode.

## Exact edits

- `crates/bridge-nfs/src/v4/files.rs`: `IoAuthority::Delegation`, returned for reads and writes under a delegation's
  state id.
- `crates/bridge-nfs/src/access.rs`: `setattr_denial` refuses a size change under `Delegation` unless the caller's
  mode bits allow writing, strictly, with no owner override. The server cannot tell a new `O_TRUNC` open from an
  `ftruncate` on a descriptor opened while writable, so it takes the refusal. A data WRITE under a delegation passes,
  as knfsd's does: the client checked the opening user's write access.
- Tests: `a_truncate_under_a_delegation_needs_write_permission_by_the_bits` (the three pjdfstest users refused, a
  permitted user allowed, an open's own authority kept); the delegation tests now expect `Delegation` for a
  delegation's state id.

## Siblings checked

- READ and WRITE under `Delegation` pass as under `Open`: `read_as` and `write_as` check the mode only under `Mode`.
- A special state id keeps `Mode`, with the NFSv3 owner override.
