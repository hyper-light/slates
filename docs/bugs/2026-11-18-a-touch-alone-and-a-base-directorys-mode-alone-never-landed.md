# A `touch` alone and a base directory's mode alone never landed (2026-11-18)

Contracts: §4.15 step 1 ("build the manifest: … symlinks and mode changes"), §4.15 step 4's verdict table and A-458's
per-field rule (accept, identical or conflict, D-27: never inferred), §4.15 step 5's order, §4.15 step 8's sync (A-472),
§4.5's copy-up ("Metadata-only changes copy up the witness and pin nothing").

## Description

Two changes an agent makes all the time reached the volume and never the disk.

- `touch` on a file the agent did not otherwise change (a new modification time, its bytes and mode as they were) was
  planned as a replacement whose bytes equalled its witness's, and the landing passed it by: the report said `Done`,
  the entry `Skipped(Drift)`, and the disk's file kept its old time. A build tool that orders work by modification
  time saw the agent's `touch` vanish once the work landed.
- `chmod` of a base directory (a directory the disk already held, which the volume merges) changed the volume's
  directory and no diverged set named it: the landing planned nothing for it, and the disk's directory kept its old
  mode. The same held for a captured clone (A-291), whose plan compared two held directories and made no entry.

Found by g085 while building A-458 (its sibling list), and given as goal 88's next piece (A-501).

## Root cause

1. `manifest::file_entry` made a metadata entry (`Chmod`) only where the file's `FileMeta` (its permission bits and
   Windows bits) differed from its witness's. A time is no part of `FileMeta`, so a file whose bytes and mode were its
   witness's was a `Replace`, and `verdict::replaced` judged it by its bytes alone: unchanged, so `Skip`.
2. A base directory's metadata change took no witness: `Overlay::copy_up` returned at once for anything but a base file
   (`Body::Base`) or a base link, and the diverged walk named a directory only when it was made, moved or opaque
   (`directory_divergence`). Nothing recorded what the disk's directory held before the change, so no verdict could
   judge it, and no entry was made.
3. `manifest::capture_entries` answered `(Held::Dir, Held::Dir)` with no entry, whatever either held.

## Impact

Correctness, on every platform: a `touch` alone and a base directory's `chmod` (a private `0700`, a group's `g+w`, an
`attrib +h` on Windows) were dropped silently at a landing that reported them landed. Nothing wrong was written to the
disk; what was lost was the agent's change.

## Exact edits

- `crates/land/src/manifest.rs`: a witnessed file whose bytes are its witness's and whose time moved is a `Chmod`, its
  time bound by the entry's witness and overlay identity; `Action::ChmodDir { from, to }`, a base directory's metadata
  entry: tag 9 with both `FileMeta`s (every older manifest encodes as it did), its own class 6, ordered with the
  removals and after them, deepest first; `FileMeta::of_volume_dir` and `of_dir_fingerprint` (a directory's bits, its
  read-only among them); the overlay's plan makes one from the directory's witness (`dir_meta_entry`), a captured
  clone's from the two held directories.
- `crates/land/src/verdict.rs`: an overlay's `Chmod` judges the file's time beside its mode and bits by the same rule
  (`with_modified`; `MetaVerdict::Set` names the time to set); a captured clone's judges none, since its capture read
  another folder; `chmod_dir_verdict` judges a directory's fields as a file's are, a missing directory `ModifyDelete`
  and anything but a directory `TypeChanged`.
- `crates/land/src/engine.rs`: `MetaChange` (the fields, whether a directory's, and the time an overlay's file judges);
  `set_meta` opens a directory with `open_dir_to_chmod` (a name that holds another kind now a type change);
  `set_meta_through` judges the time with the handle's `fstat` and sets it first (`set_mtime`), then the bits, then the
  mode; the directory's entry is staged in its batch and synced by its barrier as a file's is (A-472).
- `crates/vfs/src/host/mod.rs`, `sim.rs`; `crates/land/src/os.rs`, `os_windows.rs`; the two test wrappers:
  `LandFs::open_dir_to_chmod` (Unix the host's own directory open, `O_DIRECTORY|O_NOFOLLOW` and its own-mount check,
  held as a file descriptor, which `fchmod` and `fsync` take; Windows `CHMOD_ACCESS` with `FILE_DIRECTORY_FILE` and
  `FILE_OPEN_REPARSE_POINT`, its `FILE_WRITE_DATA` a directory's `FILE_ADD_FILE`, which the flush needs); the Windows
  writer's `set_mode` leaves a directory alone (Windows keeps no POSIX mode for one; its read-only is among its bits)
  and its `set_windows` gives a directory a directory's bits; the simulated host's `set_mtime` on a file holds the time
  unsynced until a barrier names it, as its mode and bits are.
- `crates/vfs/src/base.rs`: a base directory's first metadata change takes a witness (`witness_directory`: its
  fingerprint through its listing's handle, homed at its parent and name, none for the root or a directory the volume
  made, moved or recreated); the diverged walk names a witnessed directory `Witnessed`; a status re-checks its drift
  by its identity and its own metadata alone (`check_directory_drift`); `rewitness` takes the disk's directory again
  (`rewitness_directory`); a landed directory's witness is forgotten, and a snapshot landing rebases one whose mode or
  bits the head changed since onto what landed (`Since::ChangedDir`, `rebase_directory`).
- `crates/server/src/landing_entries.rs` and `landing.rs`: a directory's metadata entry presents and counts as a chmod.
- Tests: `crates/land/src/chmod_tests.rs` (a touch, a directory's mode, a directory's mode past a landed snapshot),
  `captured_tests.rs` (a captured clone's directory mode), `verdict.rs` (the time and the directory, field by field),
  `manifest.rs` (the order), `manifest_vectors.rs` (two directory entries in each layout, republished);
  `crates/vfs/tests/base.rs` (the directory's witness: its drift, its image and its rewitness).

## Siblings found, not fixed here

- A made directory's mode, given by `fchmod` after `mkdirat` in the directories' class, is synced with its parent
  alone; it is no metadata entry and joins no batch, so A-472's question stays open for it (GAPS).
- A base directory both moved and changed in mode lands its move alone: its rename entry wins the diverged walk, and the
  witness it took before the move is forgotten when the move lands; one changed after its move takes none (GAPS).
- A directory whose owner or time alone the volume changed is witnessed and makes no entry (a landing carries neither),
  so its witness outlives every landing (GAPS).
- The presented entry list shows a `touch` alone as a chmod that changes nothing visible: `PlannedAction::Chmod` names
  no time, though the grant binds it (GAPS).
- A directory mode that takes its owner's search away lands last, but a landing resumed after a crash past it cannot
  open what lies beneath it and refuses (GAPS).
