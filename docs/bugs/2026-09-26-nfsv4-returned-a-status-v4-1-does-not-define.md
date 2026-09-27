# NFSv4 returned a status NFSv4.1 does not define

**Date:** 2026-09-26. **Found:** reading RFC 8881 §15.2 (the valid errors of each operation) and RFC
7863's XDR while adding byte-range locks.

## Description

At three table bounds the v4 front end returned `NFS4ERR_RESOURCE` (10018):
- the client table in EXCHANGE_ID;
- a client's sessions in CREATE_SESSION;
- the open table in OPEN.

RFC 7863's `nfsstat4` says: "NFS4ERR_RESOURCE is not a valid error in NFSv4.1". It appears in no
operation's list in RFC 8881 §15.2.

## Root cause

The bounds were written from NFSv4.0 habit. NFSv4.1 replaced RESOURCE with the session-sizing errors
and the per-operation statuses.

## Impact

A 4.1 or 4.2 client that met a full table got a status its protocol does not define. The Linux client
maps an unknown status to `EIO`. No client would have met these bounds in the tests run so far, but a
daemon at its derived bound would have failed mounts and opens with a generic I/O error.

## Exact edits

- `v4/session.rs`: a full client table is `NFS4ERR_DELAY`, since lapsed clients free room and the
  client retries. A client's session bound is `NFS4ERR_NOSPC`. An exhausted client-id counter is
  `NFS4ERR_SERVERFAULT`.
- `v4/compound.rs`: the open bound is `NFS4ERR_NOSPC`.
- The new lock bound is `NFS4ERR_DELAY`, the only exhaustion status LOCK allows.
- Tests: `tests/v4_session.rs` `the_tables_refuse_at_their_bounds` and `tests/v4.rs`
  `a_lapsed_client_makes_room_and_its_opens_go_with_it` were changed first, and failed on the old
  status.

Sibling check: no other `NFS4ERR_RESOURCE` use remains in the crate.
