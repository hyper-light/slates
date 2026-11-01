# A WinFsp purge Windows did not take left a mapped view stale (2026-11-01)

Step 8 of the VFS and DAX plan; A-190, §4.6 "Windows".

## Description

A-190 purges a changed file from Windows' cache and from every view mapped of it: after each notification round the
mount's thread opens the file through the drive unbuffered and writes the changed range, and WinFsp's non-cached write
calls `CcCoherencyFlushAndPurgeCache` before the host drops the write. In main's Windows job the mapped-view test once
found the view, and a read through a handle opened after it, holding the old bytes for 10 s each (run 41148561681,
2026-10-31), with no purge counters printed, on a runner its daemon's profile found loaded.

Reproduced on CI's runners by running the mapped-view test alone a hundred times beside two CPU spinners a core and two
I/O loops (`diag-s8-winfsp-*`, each round's purge plan and outcome, each read the host served and the test's change
logged with their times):

| runner | run | stale iterations |
|---|---|---|
| windows-latest (x86_64, 4 cores) | 41326838360 | 4 of 100 (27, 36, 96, 100) |
| windows-latest (x86_64, 4 cores) | 41327264613 | 5 of 100 (5, 70, 90, 92, 97) |
| windows-11-arm (arm64, 4 cores) | 41326838360 | 0 of 100 |
| windows-11-arm (arm64, 4 cores) | 41327264613 | 0 of 100 |

## Root cause

Of the three candidates A-190's open entry named, the third holds: **a purge made that did not take.**

- Not *no purge planned*: every stale iteration planned one (`DIAG plan … purge [0, +4096) names ["\file.bin"]`) and
  counted it made.
- Not *a purge refused*: every stale iteration counted `1 made, 1 writes answered without being applied, 0 refused, 0
  unavailable`; the purge's open and its write succeeded (`done true`) and the host dropped the write as its own.
- *Made and not taken*: after the purge no read of the file reached the host for 10 s, in all nine stale iterations.
  In every one of the 95 passing iterations of run 41327264613 the view's next touch faulted and a read reached the host
  after the purge, carrying the new bytes. So Windows had not dropped the page.

`CcCoherencyFlushAndPurgeCache` reports a page it could not invalidate as `STATUS_CACHE_PAGE_LOCKED`, which Microsoft
documents as a success status ("STATUS_CACHE_PAGE_LOCKED indicates that page invalidation failed"), and WinFsp's
`FspFileNodeFlushAndPurgeCache` (`src/sys/file.c`) turns it into `STATUS_SUCCESS`. The non-cached write then proceeds and
succeeds, so nothing reaches the host that tells a purge taken from one not taken.

What held the page is not visible from user mode. It was not one of the host's own reads: none was in flight at any
purge, stale or passing (WinFsp's paging read holds the file node's `Full` resource shared until its request ends, and
the non-cached write takes it exclusive before it purges). Nor did anything the host can see tell the stale iterations
apart: the purge's open waited 335–356 ms after the round (passing median 345 ms, 86 of 95 at 300 ms or more), its write
took 42–112 ms (passing median 79.5 ms), and the last read before it ended 643–759 ms earlier (passing median 712 ms).

## Impact

A program's mapped view of a WinFsp file, and cached reads of it through any handle, kept the bytes another path had
replaced, for as long as Windows kept the file's section (indefinitely while the program held the file), in about one
purge in twenty under load on CI's x86_64 runner. Nothing was counted: the purge reported done.

## The fix

A purge is checked, and one Windows did not take is made again until it takes.

- `crates/bridge-winfsp/src/purge.rs`: `PurgeTarget` (the file through the drive: an unbuffered write of zeros, a read
  through Windows' cache, a read around it), `PurgeOutcome` (`Taken`, `Untaken`, `Refused`) and `purge_through`, which
  writes each piece and reads it back both ways: a page Windows dropped is read from the host again, so the two agree; a
  page it kept still holds its old bytes, so they differ, and the purge is untaken. Pure and cfg-free, tested on every
  host against a modelled file whose locked pages a purge cannot drop.
- `crates/bridge-winfsp/src/host.rs`: `DriveFile`, the Win32 `PurgeTarget` (the purge's open made for reading too, a
  cached open made at the first read back, `ReadFile` into a sector-aligned buffer for the unbuffered read);
  `Mounted::purge` counts each outcome; `Purged` gains `untaken`.
- `crates/server/src/winfsp.rs`: `winfsp.purges_untaken`; a round Windows took whose purge was untaken is owed again a
  tick on, as a deferred round is (the cursor put back, so the round and its purges are made again).
- `crates/cli/tests/winfsp.rs`: the mapped-view test prints the untaken purges beside the others.
- `unsafe-budget.toml`: `slates-bridge-winfsp` 111 → 112, for the `ReadFile`.

**Failing test first.** `purge.rs`'s `a_purge_windows_could_not_drop_a_page_of_is_untaken` and
`one_page_windows_could_not_drop_makes_the_purge_untaken_and_the_rest_drop` were written against a `purge_through` that
only wrote its pieces, as the mount's purge did, and failed: `left: Taken, right: Untaken`. With the read back they pass,
eight of eight.

VERIFICATION_TO_FILL

## Siblings, reported

- The purge's open through the drive waited about 345 ms under this load (median), with two dispatcher threads serving
  the mount; not a cause, but the time a view stays stale before any purge.
- The check doubles a purge's reads of its range (one through the cache, one around it); purges are made only for files
  Windows holds open with a change.
- A round owed again tells Windows its records again; WinFsp takes a repeated record as it took the first.
