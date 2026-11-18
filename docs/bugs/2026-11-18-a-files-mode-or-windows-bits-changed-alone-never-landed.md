# A file's mode or Windows bits changed alone never landed (2026-11-18)

Contracts: §4.15 step 1 ("build the manifest: … symlinks and mode changes"), §4.15's data model (`Action … | Chmod`),
§4.15 step 4's verdict table (accept, identical or conflict, D-27: never inferred), A-262 (the Windows bits a volume
keeps), A-291 (a captured clone's landing), AC-1.11 (a witnessed entry's disk bytes are read only while the disk
matches its witness).

## Description

`chmod +x` on an agent's script never reached the disk. A file whose bytes the agent left alone and whose mode it changed
was planned as a replacement, and the verdict passed it by: the landing reported `Done`, the entry `Skipped(Drift)`, and
the disk's file kept its old mode. The same held for the Windows attribute bits A-451 began to land (`attrib +h` on a
file the agent never otherwise changed), and for a captured clone (A-291), whose plan never named such a file at all.

The failing-first tests found a second fault beneath the first. A file the agent copied up for its metadata alone (a
`chmod`, a `chown`, a `touch`, an `attrib`) keeps its bytes on the disk, and the plan read them from the disk to hash them.
An outsider's change to the disk file's metadata moves its fingerprint, so that read was refused `BaseDrift`, and the
whole landing failed to plan: an outsider's `chmod` of one such file stopped every entry from landing.

Found by g085 while landing A-451, reported to main, and given as goal 88's next piece (A-458).

## Root cause

1. `verdict::replaced` judged a replacement by its bytes alone: `overlay_changed = overlay.hash != witness.identity`.
   A file with unchanged bytes was therefore "unchanged" in the overlay whatever its mode, so `Skip` (or `Skip` as
   drift where the disk moved). The data model's `Chmod` action, which §4.15 names for exactly this, was never built.
2. `manifest::capture_entries` gave a captured clone's file no entry where its bytes equalled the capture's
   (`hash == captured → Vec::new()`), whatever its mode and bits.
3. `manifest::entry_for` hashed every witnessed file by reading it through the volume. For a metadata-only copy-up the
   body is still the disk's (`Body::Base`), and the read path refuses unpinned disk bytes once the disk's fingerprint
   leaves the witness (AC-1.11). A landing does not need those bytes: they are the witness's by construction.

## Impact

Correctness, on every platform: a mode change alone (`chmod +x`, a setuid bit, a private `0600`) and a Windows bits
change alone (hidden, system, archive, offline, not indexed) were silently dropped at the landing that reported them
landed, and an outsider's metadata change to any file the agent had touched for its metadata alone refused the whole
landing at its plan. Nothing wrong was written to the disk; what was lost was the agent's change, and with the second
fault the rest of the landing.

## Exact edits

- `crates/land/src/manifest.rs`: `Action::Chmod { from, to }`, a metadata entry; `FileMeta` (the permission bits and the
  Windows bits a volume keeps for a file); tag 8 with both `FileMeta`s in the canonical encoding, so a grant binds
  them; class with the replacements; a summary name and no bytes. The overlay's plan makes a witnessed file whose bytes
  are its witness's and whose `FileMeta` is not a `Chmod`, and takes a metadata-only copy-up's identity from its
  witness instead of reading the disk (`Source::holds_witnessed_bytes`); a captured clone's plan makes a `Chmod` where
  the bytes equal the capture's and the metadata does not.
- `crates/land/src/verdict.rs`: `meta_verdict`, each field judged on its own by step 4's rule (set where the disk still
  holds the base's value, identical where it holds the volume's, a conflict where it holds a third, the disk's where the
  volume kept the base's); `chmod_verdict` for both bases (a missing file `ModifyDelete`, anything but a file
  `TypeChanged`).
- `crates/land/src/engine.rs`: the write (`Landing::chmod`): the file opened through its own handle, judged again by its
  `fstat`, each field to set set through it (`set_mode`, `set_windows`), no byte written; the fingerprint it leaves
  recorded for the advance.
- `crates/vfs/src/host/mod.rs`, `sim.rs`; `crates/land/src/os.rs`, `os_windows.rs`; the two test wrappers:
  `LandFs::open_to_chmod` (Unix a read-only no-follow open, which `fchmod` needs; Windows `FILE_READ_ATTRIBUTES |
  FILE_WRITE_ATTRIBUTES` with `FILE_OPEN_REPARSE_POINT`, a reparse point refused).
- `crates/vfs/src/volume.rs`: `Volume::holds_witnessed_bytes` and `_in` (a base body with a witness, nothing pinned, its
  length the witnessed length).
- `crates/ipc/src/protocol.rs` (`PlannedAction::Chmod`, `ActionTally::chmod`, appended; `tests/wire_order.txt`),
  `crates/server/src/landing_entries.rs` and `landing.rs`, `crates/api/src/outcome.rs` and `lib.rs` (contract 41).
- Tests: `crates/land/src/chmod_tests.rs` (four over the engine), `captured_tests.rs` (one), `verdict.rs` (the field
  table); A-451's oracle test now expects the untouched file's bits to land.

## Siblings found, not fixed here

- A metadata-only write is made durable by its directory's sync alone, as a directory's mode given after `mkdirat` is.
  From memory of the kernels' fsync paths, to verify (neither read nor measured here): ext4 commits only up to the
  directory inode's own last transaction and XFS forces its log only to the directory's own last change, so a later
  `chmod` of a file in a directory the landing did not otherwise change may wait for the next journal commit. A file-level `fsync` (not
  `fdatasync`, which skips a mode) off the shard is owed (GAPS).
- A file's `touch` alone (its mtime) still lands nothing; a captured clone's replacement overwrites an outsider's mode
  change without a conflict, where an overlay's refuses it (T-1.14); a base directory's mode change alone is in no
  diverged set and never lands.
- The manifest's canonical encoding has no golden vector, though grants bind its hash.
