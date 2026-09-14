# Every entry created through the macOS NFS mount gets an AppleDouble `._` sidecar

Status: **open — a declared limit of the NFS loopback transport, reported to its owner**
(`crates/bridge-nfs`, `crates/cli/src/mount.rs`); found by the conformance workload suite
(`cargo xtask conformance run --suite workloads`, docs/wip/conformance.md) on 2026-09-14.

## Description

On this macOS 26.4.1 host, every file, directory and symlink a process creates inside a live
`slates mount` gets a sibling `._<name>` of 4,096 bytes next to it, and it is visible to every
tool:

```
$ slates --instance x volume create xv --bounded 8MiB; slates --instance x mount <id> $M
$ cd $M && printf 'x\n' > f && mkdir d && ln -s f l && ls -la@ .
-rw-r--r--     1 adalundhe  staff    4096 Sep 14 09:00 ._d
-rw-r--r--     1 adalundhe  staff    4096 Sep 14 09:00 ._f
-rw-rw-rw-     1 adalundhe  staff    4096 Sep 14 09:00 ._l
drwxr-xr-x@    2 adalundhe  staff       0 Sep 14 09:00 d
	com.apple.provenance	    11
-rw-r--r--@    1 adalundhe  staff       2 Sep 14 09:00 f
	com.apple.provenance	    11
lrwxrwxrwx@    1 adalundhe  staff       1 Sep 14 09:00 l -> f
	com.apple.provenance	    11
$ xxd ._f | head -2
00000000: 0005 1607 0002 0000 4d61 6320 4f53 2058  ........Mac OS X
00000010: 2020 2020 2020 2020 0002 0000 0009 0000          ........
```

The same `printf 'x\n' > f` on the APFS host directory carries the same `com.apple.provenance`
(11 bytes) with no sidecar: APFS stores the attribute natively.

## Root cause

The kernel attaches the `com.apple.provenance` extended attribute to every entry created by a
process carrying a provenance tag (this session's processes do; a bare `sh` on the host shows the
same attribute on APFS). NFSv3 has no extended-attribute protocol, so the macOS NFS client
stores the attribute the only way it can — an AppleDouble file `._<name>` beside the entry (the
`Mac OS X` header and `ATTR` block above). `mount_nfs(8)` has no option to disable that: the
only attribute option, `namedattr`, is "for NFSv4 mounts". Unlinking an entry removes its sidecar
(the client keeps the pair consistent), so a directory can still be emptied; the sidecars are
visible in between.

Whether a host's processes carry the tag is environmental: the CI macOS runner's record will show
whether its runs see sidecars at all (docs/wip/conformance.md, the workloads cell).

## Impact (measured, the workload suite's record `native-macos-nfs.workloads.json`)

Every workload differs from its host run because of the sidecars: `git add -A` stages `._link`,
`d/._c.txt`, and `.git` itself gains `refs/._heads` and `refs/heads/._master`, so `git fsck
--strict` exits 8 ("badRefName: invalid refname format"); python's `os.listdir` lists `._main.py`;
`rg --files` and the rsync tree carry `._src`, `._one.txt`; vim's save leaves `._note.txt`. A
coding agent's repository inside a mount is polluted by names it never wrote.

## Exact edits (the transport owner's decision)

None inside the conformance surface. The options, for the owner:

1. Declare the NFS fallback Degraded for extended attributes (§4.6 "Documented Degraded cells")
   and keep the FSKit primary path — where FSKit stores attributes natively — as the path that
   meets AC-4.2 ("xattr and hard-link workloads pass"). The conformance matrix carries the cell as
   DIFFERS with this record as the reason until then.
2. Serve NFSv4 named attributes so `mount_nfs -o namedattr` keeps the attribute on the server
   (large: an NFSv4 server).

Refusing or hiding `._`-prefixed names on the server is not an option: the client expects to read
back what it wrote, and a refused xattr store would surface as an I/O error on ordinary creates.
