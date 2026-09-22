# NFS shared attachments retained the first request's Unix owner

## Description and evidence

CI run 35615970514, jobs 106386650002 and 106386650023, failed every mounted
suite while creating a child of its working directory: `Permission denied`.
The macOS command below reproduces that failure on 2026-09-22:

```sh
CARGO_NET_OFFLINE=true cargo xtask conformance run --suite workloads \
  --records /private/tmp/slates-ci-35615970514-macos-records \
  --scratch /private/tmp/slates-ci-35615970514-macos-scratch --keep
cargo test --offline -p slates-server --test nfs_mount \
  a_mount_preserves_each_requests_unix_owner_across_shards -- --exact --nocapture
```

The wire regression fails in 0.76 s: after MOUNT under AUTH_NONE, an AUTH_SYS
request from uid 1001 creates mode 0700 with owner `(uid=0, gid=1001)`.
The expected owner is the requesting user. Logs are in
`/private/tmp/slates-ci-35615970514-{macos-conformance,owner}-red.log`.

## Root cause and impact

Commit 78d19ea introduced a shared attachment registry. `admit_mount` recorded
the first request's subject. `Export::op_context` subsequently read that saved
subject for every request, replacing only the group. `stamp_created_owner`
used this subject as the new inode's uid. Permission checks used the current
request's uid, so a user could create a directory and immediately lose write
access to it. CREATE, MKDIR, SYMLINK and MKNOD share the affected stamp.

The registry comment also claimed its subject came from the catalog attachment,
but the code passed the untrusted RPC subject. Mount capability validation still
checked each request; the fix must preserve that separate authority boundary
(design §4.6 and §4.13, AUD-01).

## Exact correction

- Admit the shared registry entry under the validated catalog attachment's principal.
- Carry an optional Unix creation uid beside the existing creation gid in `OpContext`.
  NFS supplies the current request's caller; it never replaces the authenticated subject.
- Stamp all newly created inode kinds from that request ownership. Transports without
  a Unix credential retain their enrolled principal's ownership rule.
- Exercise two Unix users over both owner shards, nested creation, and refusal of a
  different user in a private directory. Retain the capability, revocation and snapshot
  barrier tests. Rerun the original mounted workload.

## Validation

The 9 NFS daemon tests pass in 1.40 s. All 21 shared-bridge and 42 NFS procedure
tests pass, including all creation kinds. The original mounted workload gets
past directory creation and runs eight tools; four match, while four expose the
already documented provenance/AppleDouble problem. No all-conformance closure.
