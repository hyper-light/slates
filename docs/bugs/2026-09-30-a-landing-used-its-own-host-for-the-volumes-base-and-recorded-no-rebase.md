# A landing used its own host for the volume's base, and recorded no rebase

**Date:** 2026-09-30. **Area:** `slates-server` (landing), `slates-land` (`OsLand`), `slates-db` (`Op`).
**Found by:** AUD-29-16's sibling sweep (`docs/bugs/2026-09-30-a-rename-replaced-a-base-directory-it-had-not-listed.md`).

## Description

Three defects on the path from a landing to what the volume serves and recovers.

1. **Two hosts for one base.**
   - A host handle (`HostDir`, `HostFile`) is an index into its own `OsHost`'s table.
   - An overlay volume names its base through the host its slot holds. The server ran the landing writer
     over a fresh `OsHost`, whose table has other entries under the same numbers.
   - So the engine's reads of the base went through the wrong table. Planning the landing of a large base
     file edited in one chunk window refused `StaleHandle`, and the landing was not even presented.
2. **A restart dropped the base.** A landing that makes a scratch volume an overlay over its target
   changed the volume in memory only. Its record kept `BaseRecord::Scratch`, and the landing published no
   content image.
3. **A restart refused the volume.** Once only the record was fixed, recovery met an image that said
   scratch and a record that named a base. It refused the volume `RecoveryIncomplete` (`EIO`), which is
   correct fail-closed behaviour; the image was what was wrong.

## Fix

- **One host per volume.** `OsLand::open_target_in(host, path)` walks the target (the same containment and
  ownership checks, `walk_target`) inside a given host.
  - For an overlay volume, `run_landing` takes the slot's host when the landing runs, not while a granted
    landing waits for its lease, so the volume keeps serving meanwhile.
  - On a refusal it hands the host back. After the landing it returns the host with the walked target's
    handle closed, so a landing adds no handle to the host.
  - A scratch volume keeps the writer's host (`OsLand::into_host`) when the landing made it an overlay.
- **`Op::VolumeRebased { id, base }`.** It is appended, guarded on the volume existing, and applied as
  `record.base = base`. `finish` records it among the landing's ops when the landing rebased the volume.
- **Publishing after a landing.** `finish` publishes the shard's content images after a landing that
  advanced the volume (`publish_landed`), and refuses `RecoveryIncomplete` if the volume was not captured,
  as every mutating verb does (§4.8, AUD-05).

## Tests

All three are in `crates/server/tests/`:

- `snapshot_landing.rs`, `a_landing_of_a_partly_edited_large_base_file_keeps_the_disk_bytes_it_did_not_edit`:
  a 2 MiB base file, the large class set at one page, and its first bytes rewritten through NFS.
  - Red with the writer's own host: "the landing was not presented: … BadRequest { reason: "StaleHandle" }".
  - Green now: the disk holds the edit, then the base's own bytes.
  - A 16 KiB version passed on the old code, because one chunk window pinned the whole file.
- `snapshot_landing.rs`, `a_landing_of_an_overlay_into_its_base_serves_both_files_afterwards`.
- `recovery.rs`, `a_landed_scratch_volume_keeps_its_base_across_a_restart`: a file the target held before
  the landing reads through the mount before and after a restart, as does the landed file.
  - Red with the record fix alone: the volume was refused at recovery (`not rebuilt: EIO`, the image still
    said scratch).
- `TargetDir::seed` writes a test's pre-existing base file inside the fixture's own directory in the build
  output. `std::fs::write` is otherwise barred in the server's tests (R1).
