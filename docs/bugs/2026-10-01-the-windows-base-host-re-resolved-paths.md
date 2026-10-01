# The Windows base host re-resolved paths (AUD-29-62), and the Unix host let `..` through

**Date:** 2026-10-01. **Audit:** `docs/audit/2026-09-29_audit.md` AUD-29-62 (P1). **Design:** §4.15 (the
base plane), §4.5, R1, R8.

## Description

The base plane's host must read only what lies inside the directory a volume overlays, however the disk
changes beneath it. On Unix it holds descriptors and opens every entry with `openat` relative to them. On
Windows, `crates/base/src/windows.rs` held *path strings* as directory handles: every list, open and link
read re-resolved the base path from the root of the drive. A file was checked with `symlink_metadata` and
then opened with `File::open` — the check and the open could name different objects — and any
intermediate component could become a junction or a directory symlink between calls, sending the open
outside the base. A renamed base directory was simply lost, where the Unix host keeps naming it.

Writing the containment test exposed a sibling on Unix: `openat(dir, "..", O_RDONLY | O_NOFOLLOW)` opens
the base's parent. `O_NOFOLLOW` guards a final symbolic link only; it says nothing about `..` or about a
name holding `/`, whose intermediate components follow links. `open_dir(root, "..")` returned a handle
outside the base (`Ok(HostDir(2))`, macOS, 2026-10-01).

## Root cause

Windows: the module was the Phase 1 placeholder its own header described ("a directory handle form …
arrives with the Windows bridge"); `CreateFileW` cannot open relative to a handle, and the NT call that can
was never wired. Unix: the host trusted callers to pass one entry name and never checked it.

## Impact

A base on Windows could be redirected outside itself by anyone able to edit it (a junction needs no
privilege), so an overlay could serve, digest and land bytes from outside the directory the user named
(R1). On Unix, any caller passing `..` or a path could do the same; no current caller is known to pass one
(the volume's names are single entries), so this was latent.

## Exact edits

- `crates/base/src/lib.rs`: `one_entry` — a lookup names exactly one entry: empty, `.`, `..`, and names
  holding the platform's breaks (`/` and NUL on Unix; `\`, `/`, `:` and NUL on Windows) are refused as
  absent. Unit test on every host.
- `crates/base/src/unix.rs`: `open_dir`, `open_file`, `read_link`, `entry_fingerprint` check `one_entry`.
- `crates/base/src/windows.rs`, rewritten: retained `OwnedHandle`s; `open_relative` (`NtCreateFile` with
  `RootDirectory`, `FILE_OPEN`, read access only, every share mode); `open_contained` and `settle` (the
  reparse check on the opened object: a name surrogate is the wrong kind; a non-surrogate reparse point is
  reopened through its filter and kept only if its volume serial and file index match); `list` enumerates
  the retained handle (`FileFullDirectoryRestartInfo`, then `FileFullDirectoryInfo`, a 64 KiB buffer —
  MS-SMB2 §3.3.5.4's SMB 2.0.2 `MaxTransactSize`, cited from the specification and listed for verification)
  and fingerprints each entry through its own contained open; `read_link` reads `FSCTL_GET_REPARSE_POINT`
  on the link's own handle; `read_at` is `seek_read`.
- `crates/base/src/windows_records.rs` (new, cfg-free): the `FILE_FULL_DIR_INFO` chain and the
  `REPARSE_DATA_BUFFER` parsed with every offset and length checked against the bytes filled and the
  record's declared data (the standard library's `readlink` slices at the record's offsets unchecked).
  Golden and hostile-input tests: empty, cut heads, lengths past the end and `u32::MAX - 1`, odd lengths,
  a looping next offset, a name reaching past the declared data into bytes the buffer does hold, a tag
  with no link layout.
- `Cargo.toml`: windows-sys `Wdk_Foundation`, `Wdk_Storage_FileSystem`.
- `xtask/src/main.rs`: the R1 write wall names `NtCreateFile`, `NtWriteFile`, `NtSetInformationFile` and
  `NtDeleteFile`; the two `\Device\Afd` opens in `crates/rt/src/afd.rs` carry their reason.
- `unsafe-budget.toml`: `slates-base` 6 → 10, each new site named (the two zeroed records became literals).
- `.github/workflows/ci.yml`: the native Windows lane lints and tests `slates-base`.

## Proof

- `crates/base/tests/host.rs` `a_lookup_that_is_not_one_entry_never_leaves_the_base` (every host): red on
  macOS before the change, green after.
- Windows, on the native lane (not runnable on this Darwin host): the root moved away and replaced by a
  junction (`a_retained_root_keeps_naming_the_base_after_its_path_becomes_a_junction`), an intermediate
  swapped for a junction (`an_intermediate_swapped_for_a_junction_is_never_traversed`), and a final
  component swapped for a file symbolic link with an older handle still open
  (`a_final_component_swapped_for_a_link_is_refused_and_an_open_file_keeps_its_bytes`; the link half skips
  loudly where creating a file link is refused). Every read returns the base's bytes, never the outside
  sentinel's.
- Cross-lint for `x86_64-pc-windows-msvc` clean here (clippy `-D warnings`, with stub C tools in the
  scratchpad so zstd's build script completes; nothing is linked).

## Siblings

- The Windows host still has no watcher (`ReadDirectoryChangesW`) and reports the coarsest timestamp
  class; both stay in GAPS.
