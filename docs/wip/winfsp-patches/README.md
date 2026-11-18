# Patches for WinFsp

WinFsp is another project; slates never pushes to it. Its patches live here, as Shards' do in `../shards-patches`.
Apply with `git am docs/wip/winfsp-patches/*.patch` on WinFsp's `v2.1` (`ddca7bd5`, the release slates' CI pins as
2.1.25156); on `v2.2` (`ada6de15`, the 2026 release, which is master) with `git am -3`, which merges them cleanly.

## Hard links (2026-11-18, A-360)

WinFsp's driver completes `FileLinkInformation` itself with `STATUS_NOT_SUPPORTED` ("no hard link support",
`src/sys/fileinfo.c`), sends `FileLinkInformationEx` to `STATUS_INVALID_PARAMETER`, hardcodes `NumberOfLinks` to 1 and
leaves `FILE_SUPPORTS_HARD_LINKS` commented out (`src/sys/volinfo.c`): at v2.1, v2.2B5 and v2.2 alike. So no WinFsp
file system makes a hard link, and git's and Mercurial's local clones copy on slates' drive (A-300).

- `0001` forwards `FileLinkInformation` and `FileLinkInformationEx` to a volume that declares `HardLinks`, as a
  SetInformation request in the rename request's layout (the new name, ReplaceIfExists in `RenameEx.Flags` bit 0, the
  requestor's token when a name may be replaced). It refuses a directory, a deleted file, a stream, and a replaceable
  name with an open handle, as a rename's target. Its completion acquires no second file node (it runs on the file
  system's answering thread); it takes a replaced name's node out of the name table as a rename does, invalidates the
  new name's directory listing and notifies its watchers. `NumberOfLinks` becomes the file system's count for such a
  volume, 1 for a directory; the file node keeps the count with the rest of the information it caches, since a query
  is answered from that cache (the first build read a field the cache never filled: 1 for a file with two names, on the
  VM). A volume without `HardLinks` is answered as before.
- `0002` advertises `FILE_SUPPORTS_HARD_LINKS` for such a volume.

The definitions some WDKs lack (`FileLinkInformationEx`'s class 72, its `Flags`, two `FILE_LINK_*` flags) are WinFsp's
own `FSP_` forms in `driver.h`, as it does for `FILE_STAT_INFORMATION`.

**Built** (2026-11-18): slates' diagnostics run 44341577805 (`diag-g258-17`, at `e8d3cae6`, whose two patches are
these, `git patch-id` equal) applies them to `v2.1` with `git am` on `windows-2022` and builds the driver alone for
x64, with the SDK and `NTDDI_VERSION` that `build/VStudio/build.version.props` documents for Windows 10 and later
(`10.0.26100.0`, `0x0A000006`): `msbuild winfsp.sln /t:winfsp_sys /p:Configuration=Release /p:Platform=x64
/p:SignMode=Off`. Under the project's `/W4 /WX`: 0 warnings, 0 errors. `winfsp-x64.sys` is 163,760 bytes, file
version 2.1.26322.8570a40, SHA-256 `F8288AF4806E7161DAA2F0245B0B79687872E355A7A66C697CACF527C9715B1D`; the job
test-signs it with a certificate it makes (SHA-256 `C10A33B3…E8CF`).

**Proven** (2026-11-18): on a Windows Server 2025 evaluation (10.0.26100.32230) under KVM on adacachy, test signing
on, WinFsp 2.1.25156 installed and the side-by-side driver its `WinFsp+…` service loads replaced by this build (the
service reports it running, file version 2.1.26322.8570a40, signed by the job's certificate). The drive's half is
slates at `28ad74fcd` (diagnostics run 44350120133's release build); main's is `07b9bd7cb`.
- The semantics suite's live test, `a_hard_link_made_through_the_drive_names_the_same_file`, with
  `SLATES_WINFSP_HARD_LINKS=1` (a refused link fails instead of skipping): passes with the drive's half. Both names
  report one file index and two links; the first name read a write through the second 922 µs after the writer's
  close; the second counted one link 23 ms after the first name's delete. With main's `slates.exe`, which declares
  no `HardLinks`, it fails `ERROR_NOT_SUPPORTED`: a volume that does not declare them is answered as before.
- Two names, one write (`mklink /H`, then a `CREATE_ALWAYS` overwrite through the second name): the writer's own
  name read back at once, and 50 times more, sharing only reads, with no refusal; the first name read the new bytes
  7 ms after the close, and an in-place write 35 ms after. One read of the first name sharing only reads met
  `ERROR_SHARING_VIOLATION` while that name's purge ran, as any read can while A-190's purge after another change
  holds the file open for writing.
- git 2.56.1 (MinGit), `git clone --local` of a five-file repository on the drive: exit 0, its 7 object files with
  2 names each, as on NTFS (7 of 7).
- WinFsp's own driver back (a reboot): the live test with the flag fails `ERROR_NOT_SUPPORTED`; as CI runs it, it
  skips loudly and passes; `git clone --local` on the drive dies, `fatal: failed to create link
  'G:\dst/.git/objects/4d/4bc1…': Function not implemented` (exit 128). Then this driver again (a reboot).

**Open beside the proof** (2026-11-18): a delete of one name racing the purge that a write through the other name
owes it (A-190) can hang the deleter in its cleanup; seen in every run of that race on the VM (GAPS, "A delete
racing a coherent purge can hang its caller"). Whether WinFsp as released reaches it is not yet known.

## Seen beside them, not changed, for the owner to judge

- `FileHardLinkInformation` (the query that lists a file's names, `fsutil hardlink list`) still answers
  `STATUS_NOT_SUPPORTED`: it would need a request kind of its own.
- WinFsp's DLL (`src/dll/fsop.c`) has no case for the link class, so a file system on the DLL's dispatcher would answer
  `STATUS_INVALID_PARAMETER`; slates' receive worker serves the request itself. An upstream change would add an interface
  callback, a link in `memfs`, and `winfsp-tests` cases.
- Each name of a file has a file node of its own in the driver, and a node outlives its last handle while the cache
  manager holds the file, so the bytes and size one name's node caches are not the other's: the file system must
  notify the other names when it changes the file, its own changes through one name included. slates' drive tells a
  file's other open names at its next notification round, never the name the change came through (A-360); on the
  VM, before it did, a write through one name read back through the other as the bytes before it.
