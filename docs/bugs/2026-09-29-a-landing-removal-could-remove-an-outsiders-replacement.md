# A landing's removal could remove an outsider's replacement (2026-09-29, AUD-29-04)

## Description

A landing removes or displaces base entries: deletes, directory removals, clears (a directory removed and
made again), directory renames, and the old file of a replacement. §4.15 and D-26 say an outsider's change
wins: an entry that is no longer the witnessed one is a conflict, never overwritten or removed.

Several removals named their victim by path after a check made on something else. An outsider that replaced
the name in between had its entry removed, moved or overwritten unchecked.

- **Delete.** The file was opened and checked through a descriptor, the descriptor closed, then the name
  unlinked. A replacement in between was unlinked. A symlink or other entry was unlinked unchecked.
- **Directory removal.** The directory was checked by inode, then whatever held the name was renamed to a
  hidden sibling and removed with everything beneath it.
- **Directory rename.** The origin was checked by inode, then whatever held the name was renamed to the
  destination, by a rename that replaced the destination. A check made after it, at the public destination,
  could not say which directory had moved; putting "it" back moved whatever an outsider had put there
  since.
- **The exchange fallback** (a filesystem without `RENAME_EXCHANGE`/`RENAME_SWAP`). A replacement checked
  the old file through a descriptor, then renamed the new file over the name. A clear renamed the old
  directory aside unchecked.
- **The undo paths**, which only a second outsider edit reaches. After a failed replacement or clear, the
  exchange back put the displaced entry back and then unlinked or removed whatever the exchange had left at
  the hidden name, unchecked.

## Root cause

- There was no step that captured the object itself where no outsider reaches it before it was checked.
  Checks were made through a descriptor that was then closed, by inode at a public name, or not at all.
  The removal then acted on the name.
- The write seam offered a rename that replaced its destination. The engine used it for directory
  renames, the clear fallback and the replacement fallback, so any entry an outsider had put at the
  destination was removed with no check at all.

## Impact

A landing could silently remove an outsider's edit that happened during it: an editor's save or a
`git checkout` of the same path. The report said `Written`, and a directory removal took the outsider's
replacement with everything beneath it. This breaks R1/R10 (the disk is the source of truth) and §4.15's
compare-and-swap promise.

## Exact edits

- **The seam** (`crates/vfs/src/host/mod.rs`, `LandFs`).
  - It gains `rename_noreplace`: Linux `renameat2(RENAME_NOREPLACE)`, macOS `renameatx_np(RENAME_EXCL)`,
    refused `EEXIST` when the destination exists.
  - It gains `entry_fingerprint`: an unfollowed `statat`, a symlink's own fingerprint.
  - The replacing `rename` is removed. Nothing in the landing can rename over anything, by construction.
- **The real host.**
  - `crates/land/src/os.rs`: `rename_without_replacing`, which shares `renameatx` with the exchange on
    macOS.
  - `crates/base/src/unix.rs`: `OsHost::entry_fingerprint`.
- **The simulated host** (`crates/vfs/src/host/sim.rs`).
  - `rename_noreplace` changes the moved inode's ctime, as Linux does.
  - `entry_fingerprint`, and `fstat` of a temporary's descriptor, which a real host answers and the
    simulation refused `StaleHandle`.
  - Outsider edits armed for any seam call, several at once (`interfere_at_call`). Each reports the inode
    it created and the one it displaced (`Interfered`).
- **The engine** (`crates/land/src/engine.rs`). Every removal is now a move to this landing's own aside
  name, `.slates-<landing>-aside-<path hash>`, followed by a check there:
  - **Move aside.** An exchange does it; otherwise a rename that replaces nothing.
  - **Check** the entry at the aside name against the witness. That is its fingerprint in all but the ctime
    the move itself changes, and for a replaced file the content hash, as the exchange's displaced file was
    already checked.
  - **Then:** the entry is removed only when it is the witnessed one. Otherwise it goes back without
    replacing anything, and is kept under `.slates-kept-<landing>-<n>` beside its name when the name was
    taken meanwhile (`Degradation::Kept`, never removed by a sweep).
  - **Fallbacks** (a replacement, `replace_by_renames`; a clear, `clear_by_renames`) move the old entry
    aside, check it, then move the new one in without replacing anything. The name is absent between the
    two renames; that window is measured and reported.
  - **Directory rename** moves the origin aside, checks it, then moves it to the destination without
    replacing anything.
  - **Undo paths.** An exchange back removes the temporary only when the entry at its hidden name is that
    temporary (by the inode its open descriptor holds). The clear's fresh directory is removed only when it
    has the identity it had at its hidden name. Anything else is kept.
  - **The sweep** settles an entry at an aside name by its manifest entry:
    - The witnessed entry is removed, except for a replacement or clear whose name is free: that is a crash
      inside the fallback's window, so the entry goes back.
    - A rename's directory always goes back.
    - This landing's own temporary or empty fresh directory is removed.
    - Anything else goes back or is kept.
  - **Sweep order.** The sweep runs before validation, so a resume's verdicts see each path old or new,
    never missing.

## Evidence

- **Failing tests first.** `crates/land/tests/removal.rs` is the oracle over the simulated host; its rules
  are stated once in `crates/land/tests/common/removal.rs`.
  - One test arms one outsider save at every seam call of a one-entry landing, for nine kinds: a deleted
    file and symlink, a removed directory, a replaced file and a cleared directory each with and without
    the exchange, and a renamed directory with the outsider at its origin and at its destination.
  - The other arms two saves at every pair of calls.
  - On the old engine (a worktree with the new seam and oracle, the old `engine.rs`), with the first failing
    call named:

    | Kind | One outsider save | Two saves |
    |---|---|---|
    | File delete | red at call 5 | red |
    | Symlink delete | red at call 5 | red |
    | Directory removal | red at call 7 | red |
    | Replace, with exchange | green | red at calls 2 and 14 |
    | Replace, fallback | red at call 12 | red |
    | Clear, with exchange | green | red at calls 8 and 10 |
    | Clear, fallback | red at call 8 | red |
    | Rename, outsider at the origin | red at call 7 | red |
    | Rename, outsider at the destination | red at call 0 | red |

    In the simulation, a rename replaced a file with a directory; a real `rename(2)` refuses `ENOTDIR`
    there. The real form of the destination case is an outsider's empty directory, which a replacing rename
    removes.
- **The real kernel.** `crates/land/tests/os_removal.rs` holds a real directory on Linux tmpfs
  (`/dev/shm`, Docker) to the same rules through a delegating host that makes a real outsider
  save-by-rename before the chosen call. On the old engine it failed with one save:

  | Kind | Old engine, one save |
  |---|---|
  | File delete | call 7 |
  | Symlink delete | call 6 |
  | Directory removal | call 8 |
  | Replace, fallback | call 16 |
  | Clear, fallback | call 9 |
  | Rename, outsider at the origin | call 8 |

  The exchange paths and the destination case passed, as predicted above. Now it passes, three runs of
  three, about 4 s each.
  - Its base file is seeded an hour old. A file seeded just now is witnessed racy or not by timing, and the
    engine's seam calls then differ between runs.
- **Now: all green, and neither rule passes vacuously.**
  - 139 single-save histories, with these (stale, written) counts:

    | Kind | Stale | Written |
    |---|---|---|
    | File delete | 6 | 3 |
    | Symlink delete | 6 | 3 |
    | Directory removal | 8 | 7 |
    | Replace, with exchange | 14 | 6 |
    | Replace, fallback | 15 | 2 |
    | Clear, with exchange | 11 | 8 |
    | Clear, fallback | 12 | 6 |
    | Rename, outsider at the origin | 8 | 4 |
    | Rename, outsider at the destination | none; stale does not apply there | 1 |

  - 547 pair histories. A kept entry is reached in every kind but the destination rename (Replace fallback
    39, Clear fallback 23, Replace with exchange 16, Clear with exchange 6, and 2 each for file, symlink,
    directory and rename).
- **Crash oracle without the exchange (new).**
  `t_1_15_without_exchange_crash_at_every_write_instruction_then_resume` crashes at each of the fallback's
  79 write steps. Every path is old or new, or absent while its old entry sits aside, byte for byte; 8 path
  observations fell inside a window. The resume then reaches the reference, sweeps and plans nothing
  further.
  - Red when the sweep runs after validation: the resume refuses `Conflict`, both with and without the
    exchange, because of the rename's window.
  - Red when a witnessed aside of a replacement or clear is removed while its name is free: the resume
    refuses `Conflict`.
  - The exchange crash oracle, 71 steps, still passes.
- **Cost.** Seam calls per one-entry landing, old → new, from the reference runs:

  | Kind | Old | New | Note |
  |---|---|---|---|
  | File delete | 8 | 9 | |
  | Symlink delete | 7 | 9 | was unchecked |
  | Directory removal | 13 | 15 | |
  | Replace, with exchange | 20 | 20 | |
  | Replace, fallback | 15 | 22 | includes hashing the displaced file |
  | Clear, with exchange | 18 | 19 | |
  | Clear, fallback | 17 | 21 | |
  | Rename | 9 | 12 | |

  The landing bench's engine-only row (1,000 replacements over the simulated host, the exchange path)
  measured old 8,678 / 8,956 / 8,566 ns and new 9,029 / 8,836 / 8,877 ns per entry. The rounds were
  interleaved on this M5 Max at load average 8–10 from other clusters. The per-round difference, −120 to
  +351 ns, is inside the run-to-run spread.
- **Suites.**
  - macOS: slates-land (unit 6, grant_binding 3, oracle 20, os 5 skipped loudly, removal 2, os_removal 1
    skipped loudly); slates-vfs 165; slates-server lib 124, daemon 16, recovery 7.
  - Linux (Docker, `/dev/shm`): slates-land including os 5 and os_removal 1.
  - clippy clean on macOS and Linux.

## Siblings found, open

- **The sweep's errors are swallowed.** `unwrap_or(false)` and `.is_ok()` per entry, and `unwrap_or(0)` for
  the whole sweep. This is AUD-29-05, the next finding. A failed settle leaves the entry at its aside name
  for a later resume.
- **A recursive removal checks the directory itself (its inode), not what is beneath it.** An outsider's
  file written inside a directory the overlay removed is removed with it. This is the design's `Rmdir`
  ("removed with everything beneath it"); a witness over the subtree would be a §4.15 amendment.
- **An empty outsider directory swapped in by an exchange just before a crash is removed by the sweep**, as
  the clear's fresh directory would be. No bytes are lost, only that empty directory's own metadata.
- **Hidden siblings of a landing never resumed stay on the disk.** After a reboot the landing's manifest is
  gone, and only a landing with the same id sweeps them. With the fallback, a crash inside a window leaves
  the old entry aside under `.slates-<landing>-aside-…` until then. This predates the change for
  temporaries; it is now also true of one entry per crash.
