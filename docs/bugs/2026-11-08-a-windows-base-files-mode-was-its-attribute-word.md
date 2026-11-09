# A Windows base file's mode was its attribute word (2026-11-08, A-262)

## Description

Over an overlay of a Windows directory, every untouched base entry reported a POSIX mode equal to its Win32 attribute
word. An ordinary file (`FILE_ATTRIBUTE_ARCHIVE`, `0x20`) read as mode `0o040`, a `NORMAL` one (`0x80`) as `0o200`, and
a directory (`0x10`) as `0o020`, with no file-type bits.

## Root cause

`crates/base/src/windows.rs` `fingerprint_of_handle` set `Fingerprint::mode` to `dwFileAttributes`. The vfs takes a base
inode's mode from its fingerprint (`crates/vfs/src/base.rs`), and the landing reads the type bits from it
(`manifest::kind_is_dir`).

## Impact

Before A-262, nothing visible: the WinFsp drive reported attributes by kind alone, and its callers act as root. With A-262
stage 1 the drive reports `READONLY`, and refuses a delete and a replacing rename, for a file whose owner write bit is
clear. So through a drive over a Windows overlay, every `ARCHIVE`-only base file would have read as read-only and refused
deletion. Stage 1 was held from landing until this fix.

## Exact edits

- `crates/base/src/lib.rs`: `windows_mode_and_bits`, DrvFs's mapping (`S_IFDIR | 0o755`, `S_IFREG | 0o644` or `0o444`
  when `READONLY`, `S_IFLNK | 0o777` for a reparse point), and the bits `WindowsAttributes::kept` keeps, unit-tested on
  every host.
- `crates/base/src/windows.rs`: the fingerprint's mode and its new `windows` field from that mapping.
- `crates/vfs/src/inode.rs`: `Fingerprint::windows`, and `adopt_observed` takes it with the mode and owner.
- `crates/vfs/src/base.rs`: a base inode takes the fingerprint's bits; both adopt paths pass them.
- `crates/vfs/src/host/sim.rs`: `SimHost::set_windows_attributes`, an outsider's `attrib`.
- Image layout 24, delta 7, and the golden `a_witness_encoding_is_its_golden_vector`.

Test: `an_untouched_base_file_reports_the_disks_windows_attribute_bits_and_follows_their_change`
(`crates/vfs/tests/base.rs`). Red with the adoption removed: the outsider's cleared `HIDDEN` never arrived (`34` stayed
`34`).
