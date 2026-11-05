# The WinFsp guard read a field WinFsp always zeroes
n**Amendment:** A-206 (corrects A-198).

**Found by:** the audit of 2dc9f3eb..98000d56 (2026-11-03), step 6 of plan 01a1a34bf7e973069e94f773afdea44a.

## Description

The hand-written receiver (`crates/bridge-winfsp/src/dispatcher.rs`) replaces WinFsp's own dispatcher: it writes
`FSP_FILE_SYSTEM` fields and calls the DLL's operation table by offset, transcribed from v2.1. Its guard compared
`FileSystem.Version` with zero, to refuse a DLL with another layout.

## Root cause

`FSP_FILE_SYSTEM.Version` is reserved: v2.1's `src/dll/fs.c` zeroes the object and never sets the field, so the guard
passed every DLL. It also ran after `FspFileSystemCreate`, through the very layout it was meant to check.

## Impact

Any WinFsp other than the version the receiver was read from was driven through the wrong offsets, which is memory
corruption. CI's `choco install winfsp` is unpinned, so a new WinFsp release could have reached it silently.

## The fix

`crate::dll_version` (pure and cfg-free, tested on every host) compares the DLL's `FspVersion` (file version, major in the
high 16 bits, minor in the low) with `CHECKED_VERSION`, 2.1. `Mounted::new` asks before anything touches the file system
object, and any other version is `WinFspError::UnsupportedDll`. A mount's reply carries it as `TargetUnavailable`, with the
number in the words.

`FspVersion` answers major and minor only, so a 2.1 patch release is not told apart. The layout is what the check
protects, and WinFsp keeps it within a minor version (from memory, flagged for review at the next upgrade).

**What this makes of an upgrade:** a new WinFsp minor version on a runner or a user's machine refuses every mount until
the receiver's layout is reviewed and `CHECKED_VERSION` raised. That is the intent of the goal. It means CI's unpinned
`choco install winfsp` will turn the Windows lanes red on the day upstream ships one.

## Evidence

See the entry at the top of `docs/wip/GAPS.md`. `any_other_version_is_refused_with_its_number` runs on every host. The
red-check is a diagnostics tree with `CHECKED_VERSION` changed: the unchanged live tests' first mount is refused with
the DLL's real version in the refusal.
