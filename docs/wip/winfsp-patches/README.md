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

**Built** (owed): slates' `diag-g258-10` builds `v2.1` with these patches on `windows-2022` (WDK 10.1.26100) for x64,
with the SDK and `NTDDI_VERSION` that `build/VStudio/build.version.props` documents for Windows 10 and later
(`10.0.26100.0`, `0x0A000006`): `msbuild winfsp.sln /t:winfsp_sys /p:Configuration=Release /p:Platform=x64
/p:SignMode=Off`, and test-signs the driver with a certificate made in the job.

**Proven** (owed): on a test-signing Windows Server 2025 evaluation under KVM (adacachy), WinFsp 2.1.25156 installed and
its driver replaced by this one: the semantics suite's live hard-link test with main's `slates.exe` and with the drive's
half, and git's `clone --local` on the drive.

## Seen beside them, not changed, for the owner to judge

- `FileHardLinkInformation` (the query that lists a file's names, `fsutil hardlink list`) still answers
  `STATUS_NOT_SUPPORTED`: it would need a request kind of its own.
- WinFsp's DLL (`src/dll/fsop.c`) has no case for the link class, so a file system on the DLL's dispatcher would answer
  `STATUS_INVALID_PARAMETER`; slates' receive worker serves the request itself. An upstream change would add an interface
  callback, a link in `memfs`, and `winfsp-tests` cases.
- Each name of a file has a file node of its own in the driver, so a byte one name's cache holds is not the other's: the
  file system must notify the other names when it changes a file, as slates' drive notifies every name an inode is open
  under (`crates/bridge-winfsp/src/notify.rs`, `an_inodes_change_names_every_path_it_is_open_under`).
