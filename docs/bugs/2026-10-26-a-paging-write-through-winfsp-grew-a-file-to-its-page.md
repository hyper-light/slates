# A paging write through WinFsp grew a file to its page

- **Date:** 2026-10-26
- **Status:** fixed 2026-10-26, in the same change as this record (A-177).
- **Subsystem:** the WinFsp mount host's write, `crates/bridge-winfsp/src/host.rs` (`Op::Write`).
- **Severity:** correctness, silent wrong data. A file Windows wrote through its cache came out of the volume as long
  as its last page, the tail zeros.
- **Exposure:** every cached write through a WinFsp mount. Windows caches file data on a WinFsp volume only when
  `FileInfoTimeout` is infinite (`src/sys/create.c` sets `FO_CACHE_SUPPORTED` only then), and A-177 made it infinite,
  so the bug was unreachable before A-177.
- **Found by:** `a_volume_mounted_through_winfsp_is_read_and_written_by_windows_programs` on CI's Windows runner, then
  a request log (run 40251802164).

## Symptom

PowerShell's `WriteAllText` wrote a 62-byte message. `slates read` of the file returned 62 zero bytes at once
(run 40249524066), and 4096 bytes starting with the message once the close had written it back (run 40251802164).
The log:

```
WINFSP-DIAG SetSize ino=281474976710658 size=62 -> Object ino=281474976710658 size=62 fh=0
WINFSP-DIAG Write ino=281474976710658 at=0 len=4096 append=false first=Some(104) -> Wrote 4096 size=4096
```

## Root cause

- The cache manager writes a cached file back in whole pages, as a paging write that WinFsp marks `ConstrainedIo`:
  the write may not change the file's size, and the bytes past the size are not the file's.
- The host applied every write whole, so the page's tail past byte 62 extended the file.
- WinFsp's `memfs` clips such a write to the file's size (`tst/memfs/memfs.cpp`, `Write`: `if (ConstrainedIo)`).

## Fix

- `Op::Write` carries `constrained`. A constrained write is clipped to the file's size; past the end it writes
  nothing; it never appends. The reply counts the bytes written.
- Separately, the bytes reached the volume only at the cache manager's lazy write-back, seconds after the close. The
  mount now sets `FlushAndPurgeOnCleanup`, so a file's last close writes its pages back before the `Cleanup` request,
  whose publication then carries them (`src/sys/cleanup.c`, `FspFileNodeCleanupFlush`).

## Proof

- The same test passes on the fixed commit (run 40252840969): the message reads back through the drive and through
  `slates read`, byte for byte.
