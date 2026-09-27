# The NFSv4 session advertised sizes it did not hold

Date: 2026-09-26. Contracts: §4.6 "The NFSv4.1/4.2 front end" (sessions, bounded tables), RFC 8881
§2.10.6.4 (the negotiated channel attributes), §18.36.3 (CREATE_SESSION), §18.46.3 (SEQUENCE). Found
while benchmarking the NFSv4.2 transport (`cargo xtask conformance bench`): the kernel test had never
written a file larger than a page.

## Symptom

Over the Linux kernel's v4.2 client, a 1 MiB `write` into a mounted volume failed with `EIO`. A packet
capture of the mount (`tshark -d tcp.port==$PORT,rpc`, filtered on `nfs.nfsstat4`) showed 126 WRITE
operations answered `NFS4ERR_INVAL` in one run.

## Root cause

Two contracts were advertised and not held.

1. **The transfer ceiling.** The front end told the client `maxread`/`maxwrite` equal to the session's
   largest response, `MAX_TRANSFER` plus the 4,096-byte header allowance: 266,240. The v3 layer
   that serves each WRITE refuses a count above `MAX_TRANSFER` (262,144) as `NFS3ERR_INVAL`. The Linux
   client sizes its WRITEs to `maxwrite`, so every full-size write was refused.
2. **The session's request and reply sizes.** §4.6 said a compound past its bounds is
   `NFS4ERR_REQ_TOO_BIG`/`NFS4ERR_TOO_MANY_OPS`, but SEQUENCE checked neither, and no reply was held to
   `ca_maxresponsesize` or, when `sa_cachethis` asked for it to be kept, to `ca_maxresponsesize_cached`.
   The per-compound header allowance was a bare `4096`.

## Fix

- `maxread`/`maxwrite` are `MAX_TRANSFER`, the number the v3 layer holds.
- The session's request size is `MAX_TRANSFER` plus `COMPOUND_HEADER_BYTES`, a constant derived by
  summing the largest RPC header, the RPC-over-TCP record marking and the SEQUENCE, PUTFH, WRITE and
  GETATTR arguments of the largest transfer compound (2,124 bytes). It is computed in a `const fn` and
  narrowed with a compile-time round-trip check.
- SEQUENCE takes the call's byte length and operation count and refuses `NFS4ERR_REQ_TOO_BIG` and
  `NFS4ERR_TOO_MANY_OPS` before it touches the slot, so the next sequence id still serves. It returns
  the session's reply limits. The compound truncates at the first operation whose result would cross
  `ca_maxresponsesize` (`NFS4ERR_REP_TOO_BIG`) or, for a kept reply, `ca_maxresponsesize_cached`
  (`NFS4ERR_REP_TOO_BIG_TO_CACHE`), using `XdrWriter::truncate`.

## Evidence

- `crates/bridge-nfs/tests/v4.rs` `the_sessions_negotiated_sizes_bound_requests_and_replies`: a WRITE of
  exactly `maxwrite` is served, and each refusal is returned where the RFC places it.
- `crates/server/tests/nfs_v4_kernel.rs` `large_sequential_writes_round_trip`: eight 1 MiB writes,
  `fsync` and a read-back, through the kernel's v4.1 and v4.2 clients. It failed with `EIO` before the
  fix, in a privileged Linux container on 2026-09-26.

## Exact edits

`crates/bridge-nfs/src/v4/{compound,session,backend}.rs`, `src/xdr.rs` (`truncate`),
`src/server.rs` and `crates/server/src/nfs.rs` (the call's byte length reaches the compound), and the
tests named above.
