# Sealed content left its plaintext in RAM

**Found:** 2026-10-06. An adversarial scan of a live daemon's anchor content memfd, read from outside through
`/proc/<pid>/fd` on Linux 6.12: a file with a distinctive marker, written through a FUSE mount, then left idle.

## Description

After 25 s idle the marker was still in the content object, 800 times for 400 written: two plaintext copies of the
file. That was so although the daemon had a sealing root (`seal.state=minted`) and its sweep had sealed the file
(`content.sealed` moved). Condition 9 requires content at rest to be encrypted when it is not being written or read.

## Root cause

There were two copies, from two causes.

1. **The write log (A-63) kept every logged write's plaintext after the publication that captured it.**
   `WriteLog::clear` reset the length and stamp only. The records stayed in the content object until a later append
   happened to overwrite them.
2. **A seal deferred the old block's scrub to a publication that never came.** Sealing an imaged chunk writes the
   ciphertext elsewhere and defers the old block's free to the commit that releases it, which zeroes it
   (`slates_mem::arena`). An idle volume publishes nothing of its own, so the plaintext block waited indefinitely.

## Fix

1. `WriteLog::clear` zeroes the records it drops, after setting the length to zero. A daemon killed mid-scrub
   replays nothing rather than half-zeroed records.
2. The idle sweep (`seal_idle_content`) publishes its shard once, on a tick that sealed something. That commit
   releases the deferred blocks, zeroed. A tick that sealed nothing publishes nothing.

## Tests

- `a_cleared_log_keeps_none_of_the_bytes_it_logged` (`crates/server/src/write_log.rs`, Linux): red first.
- `a_sealed_files_plaintext_leaves_the_content_object` (`crates/server/tests/recovery.rs`, Linux) reads the content
  memfd's data ranges (`SEEK_DATA`/`SEEK_HOLE`), the control being the plaintext right after the write. It failed
  with 32 copies left, and fails again with the publish removed.
- The recovery suite passes, 27 of 27 on Linux and 26 of 26 on macOS.
- The live scan, after: 400 copies right after the write (the open extent, plaintext by design), 0 after idle.

## Sibling sweep

- **The kernel's own page cache for a mount** holds plaintext. It is outside what slates protects, as A-99 states.
- **Other deferred frees** (a deleted file's blocks, a truncate) wait for the next publication the same way. A volume
  that is idle after a delete keeps that plaintext until its next publication. Open in GAPS.
