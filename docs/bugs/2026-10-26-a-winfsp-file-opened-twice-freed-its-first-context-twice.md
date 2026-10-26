# A WinFsp file opened twice freed its first open's context twice

- **Date:** 2026-10-26
- **Status:** fixed 2026-10-26, in the same change as this record (A-177).
- **Subsystem:** the WinFsp mount host, `crates/bridge-winfsp/src/host.rs` (its open-file contexts) and the volume
  parameters it mounts with.
- **Severity:** memory safety. A use-after-free and a double free in the mount's own process, the daemon since
  A-177. A callback read a freed `OpenFile` (a wrong inode and handle) and could act on a different file.
- **Exposure:** every WinFsp mount since the host was first written (2026-09-10, `winfsp-mount`). Before A-177 only
  the host's own live test mounted, and it opened each file once at a time, so nothing showed.
- **Found by:** a request log on CI's Windows runner (diagnostics branch, run 40251802164, Windows Server 2025,
  WinFsp 2.1.25156).

## Symptom

The log of the requests the owner shard served, for a file opened by two handles in turn:

```
WINFSP-DIAG Create \hello.txt dir=false -> Object ino=281474976710658 size=0 fh=5
...
WINFSP-DIAG Open \hello.txt -> Object ino=281474976710658 size=4096 fh=4294967296
WINFSP-DIAG Release ino=281474976710658 -> Ok
WINFSP-DIAG Release ino=4096 -> Ok
```

The second release names inode 4096, which no file had. It read an `OpenFile` box after that box was freed.

## Root cause

- The host leaks one `OpenFile` box per open as its WinFsp `FileContext` and reclaims it at that open's `Close`.
- WinFsp hands a callback `UserContext[UmFileContextIsUserContext2]` (`src/dll/fsop.c`, `ValOfFileContext`). With the
  flag clear (the default), that is the file node's `UserContext`, which the kernel keeps from the file's first open
  and shares with every later one. Each later open's own box was never handed back; each `Close` reclaimed the first
  open's box again.
- WinFsp's `memfs` uses the default safely because its context is a reference-counted file node. Ours is per open.

## Fix

- Mount with `UmFileContextIsUserContext2` (bit 16 of the volume parameters' flags, `VOLUME_FLAG_FILE_CONTEXT_PER_OPEN`):
  each callback gets the context its own open returned, the kernel's per-handle `UserContext2`, and each `Close`
  reclaims that one box once.

## Proof

- The cached-read and mapped-view tests (`crates/cli/tests/winfsp.rs`) open one file by two handles, and the read and
  write test opens a file after its creating handle closed. Run 40252845459 on the fixed commit logged each release with the
  inode and handle its own open had.

## Siblings swept

- The other per-open pointers the host hands WinFsp are the handler (one per file system, `UserContext` of the
  `FSP_FILE_SYSTEM`) and the directory buffer (WinFsp's own). Neither is per open.
