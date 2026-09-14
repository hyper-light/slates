# A fifo or socket created through the NFS mount fails with EIO, not an "unsupported" error

Status: **open — a declared limit, reported to the transport owner** (`crates/bridge-nfs`,
`procedures.rs` `mknod_unsupported`); found by pjdfstest through the conformance harness
(`cargo xtask conformance run --suite pjdfstest`, docs/wip/conformance.md) on 2026-09-14.

## Description

Through a live `slates mount` on macOS 26.4.1, with the pjdfstest binary at the pinned commit
(`pjd/pjdfstest@85a8aea`):

```
$ pjdfstest mkfifo ff 0644       -> mkfifo returned -1, EIO
$ pjdfstest bind fs              -> bind returned -1, EIO      (an AF_UNIX socket at a path)
$ pjdfstest mknod fc c 0644 1 2  -> mknod returned -1, EPERM   (a character device; also block)
```

## Root cause

By design slates creates no special nodes: `NFSPROC3_MKNOD` is answered with the typed refusal
`NFS3ERR_NOTSUPP` (`crates/bridge-nfs/src/procedures.rs:1133-1139`, "slates does not create
special (device, FIFO or socket) nodes"). The macOS NFS client presents that refusal to `mkfifo(2)`
and `bind(2)` as `EIO`, not `ENOTSUP`/`EOPNOTSUPP`. Block and character device nodes are refused
by the kernel itself for an ordinary user on every filesystem (`mknod(2)`: `EPERM`), so those
cases are not the transport's.

## Impact (measured, pjdfstest over the mount, unprivileged)

212 cases fail with `EIO` (every `mkfifo` and `bind` step of `chmod`, `chown`, `link`, `mkfifo`,
`mknod`, `rename`, `unlink` and the `*/00.t` type sweeps), and every later step on the name that
never came to exist fails `ENOENT` — the largest single cause of the 889 `expected 0, got ENOENT`
failures in the record `native-macos-nfs.pjdfstest.json`. A tool that puts a socket or a fifo in a
volume (an agent's IPC socket, `npm`'s, a watcher's) sees an I/O error where "not supported" is
the truth.

## Exact edits (the transport owner's decision)

None inside the conformance surface. Either:

1. Keep refusing and make the refusal legible: the macOS client maps `NFS3ERR_NOTSUPP` to
   `EIO` for MKNOD; a client-visible `EOPNOTSUPP` needs the FSKit path, so the NFS fallback's
   Degraded cell should say "no fifos, sockets or device nodes (EIO)" in §4.6 and the reviewed
   expected-failure list names every such case with this record as the reason; or
2. Create fifo and socket nodes in the volume core (namespace entries with a type and no content —
   the bridge trait would gain `mknod` for those two types, device nodes still refused).
