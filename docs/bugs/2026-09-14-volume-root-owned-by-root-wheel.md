# The volume's root directory lists as root:wheel through the mount

Status: **open — a sibling of the fixed 2026-09-09 root:wheel bug, already noted as "minor
remaining" in the GAPS.md Bridges (4.6) row; this record adds the live repro and the impact**
(owner: the provisioning create path in `crates/server`/`crates/vfs`); seen while reproducing
the conformance findings on 2026-09-14.

## Description

```
$ slates --instance x volume create xv --bounded 8MiB; slates --instance x mount <id> $M
$ cd $M && printf 'x\n' > f && ls -la .
drwxr-xr-x     3 root       wheel       0 Sep 14 09:00 .        <- the volume's root
-rw-r--r--@    1 adalundhe  staff       2 Sep 14 09:00 f        <- created by adalundhe: correct
```

Entries created through the mount are owned by the mounting user (the 2026-09-09 fix,
`docs/bugs/2026-09-09-root-wheel-mount.md`), but the volume's own root directory — created by
`volume create` — still lists as `root wheel`.

## Root cause (hypothesis, from the earlier record)

The earlier fix stamps the request subject's uid/gid on objects the bridge creates. The volume root
is born by the provisioning verb, not through the bridge, and keeps the volume core's default
owner (`uid: 0, gid: 0`, `Inode::new`). The earlier record notes the synthetic *host root* is
root:wheel by intent; this is the volume root, which a user works in.

## Impact

Tools that check the ownership of their working directory refuse or warn: git's
`safe.directory` check ("detected dubious ownership in repository") fires for a repository whose
`.git` sits directly at the mount root; `ls -la` in a freshly mounted volume shows a directory the
user apparently does not own. The conformance workloads did not trip it only because the harness
works in a subdirectory it creates (owned by the user).

## Exact edits (the owner's)

Stamp the provisioning client's uid/gid on the volume root at `volume create` (the verb carries
the client's principal; the NFS `AUTH_SYS` path already threads it for mounted creates), and add
the observable check to `crates/cli/tests/cli.rs`'s live-mount test: `stat -f %u:%g` of the mount
point equals the mounting user's.
