# A FUSE name that is not UTF-8 answered EIO

**Found:** 2026-10-06, by hostile name traffic through a real Linux 6.12 FUSE mount in the hermeticity trace (a
`pip install` and hostile names through the mount, the daemon under `strace -f`).

## Description

`printf z > "$(printf 'bad\xff\x01name')"` on a slates FUSE mount failed with `EIO` ("Input/output error"); so did
Python's `os.open(b"bad\xffname2", O_CREAT)`. `EIO` tells a caller the device failed, so a tool retries, reports a
broken disk, or aborts. The name was refused for a reason that is the caller's: it is not a character string.

## Root cause

A volume's names are character strings (D-4; `crates/vfs/src/names.rs` folds and compares them as such), the form
every target slates serves or lands on can hold: NFSv4 refuses others `NFS4ERR_INVAL` (RFC 8881 §14.4), APFS refuses
them `EILSEQ`, NTFS holds UTF-16. The base reader already lists a host name that is not UTF-8 as an entry the volume
never serves (`crates/base/src/unix.rs`). The FUSE codec checked the same rule but reported it as
`FuseError::UnterminatedName`, the refusal for a malformed message, and every name-carrying opcode answered a codec
refusal `EIO`.

## Fix

- `FuseError::NameNotUtf8`, a typed refusal of its own (`crates/bridge-fuse/src/error.rs`).
- `refuse_unparsed` (`crates/bridge-fuse/src/bridge.rs`) answers it `EILSEQ`, and any other codec refusal `EIO`, at
  all nine sites: lookup, mkdir, mknod, create, unlink, rmdir, symlink (name and target), link, rename and rename2.
- The name and rename parsers' slicing became checked `.get()` (the no-panic rule; the sites were bounded by their
  callers, not by the lint wall).

Proven on Linux 6.12 through the kernel: the same two calls now fail `EILSEQ` ("Invalid or incomplete multibyte or
wide character", errno 84), nothing reaches the volume, and the trace is unchanged.

## Tests

`a_name_that_is_not_utf8_is_refused_eilseq_by_every_opcode_and_never_reaches_the_seam`
(`crates/bridge-fuse/tests/dispatch.rs`): every name-carrying opcode, each name position, `EILSEQ`, nothing at the
seam; a name with no NUL stays `EIO`. It failed before the fix (`-5` for `-84`) and fails again with the mapping
mutated back to `EIO`.

## Sibling sweep

- **virtio-fs** dispatches through the same function: fixed by the same change.
- **NFSv3** decodes names inside each procedure and answers `NFS3ERR_INVAL` at every site
  (`crates/bridge-nfs/src/procedures.rs`, `multi.rs`): correct already.
- **NFSv4** answers `NFS4ERR_INVAL`: correct already, per RFC 8881.
- **MCP and the CLI** take names as JSON or argv strings: always UTF-8.
- **Inconsistency reported, not fixed here.** CLAUDE.md §2 item 6 lists `indexing_slicing` and `string_slice` as
  enforced by the lint wall, but `[workspace.lints]` in `Cargo.toml` does not enable them (the sweep of the remaining
  sites is the known debt recorded 2026-09-27).
