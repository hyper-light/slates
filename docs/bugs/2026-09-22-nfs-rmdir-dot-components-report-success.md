# NFS RMDIR dot components report success without removing anything

## Evidence and cause

CI run 35604717581, macOS conformance job 106348895431, reports pjdfstest
`rmdir/12.t:4`: removing `parent/child/..` succeeds although it must refuse.
On 2026-09-22 the wire regression reproduces the cause in under 0.01 s:

```sh
cargo test --offline -p slates-bridge-nfs --test procedures \
  rmdir_refuses_dot_entries_and_preserves_the_directory -- --exact --nocapture
```

Actual status is NFS3ERR_NOENT (2), expected NFS3ERR_NOTEMPTY (66). The export
looks up `..` as an ordinary stored directory entry before calling RMDIR. The
VFS represents parent relationships separately, so that lookup misses. Apple's
[`nfs3_vnop_rmdir`](https://github.com/apple-oss-distributions/NFS/blob/main/kext/nfs_vnops.c)
maps ENOENT to success, treating it as a retry of a completed removal. The tree
remains unchanged, but the application's success claim is false.

## Correction and scope

After validating the parent and its write/search permissions, refuse RMDIR `.`
with INVAL and `..` with NOTEMPTY before ordinary entry lookup (§4.6, RFC 1813
§3.3.13). The regression checks both refusals, that the directory survives, and
that an ordinary removal still succeeds. No expected-failure list changes.

Sibling audit: LOOKUP and other namespace procedures also encounter dot names;
their full behavior needs review independently. This correction claims only RMDIR.

## Validation

Red log: `/private/tmp/slates-ci-35615970514-rmdir-red.log`. All 42 NFS procedure
tests pass after the correction. Mounted validation remains pending.
