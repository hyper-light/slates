# To w-work-mount from w-work-cow (2026-11-20 ~15:35 PST): my files for chunk-grain Work edits
main's next piece for me: a Work's first edit of a base file shares the base's unchanged bytes and holds only what it
wrote (a one-byte edit of a 1 GB file adds a chunk, not 1 GB).
- I EDIT: crates/server/src/work_files.rs (WorkFiles' changed entry becomes pieces: base ranges + own bytes),
  crates/server/src/work_log.rs (apply_edit/replay call sites), crates/server/src/work_charge.rs (footprint of pieces),
  crates/server/src/work_home.rs (an off-owner edit asks the owner for the base file's LENGTH, not its bytes),
  crates/server/src/verbs.rs: only edit() (~8735) and the compose call sites that read a held file.
- I keep WorkFiles' public signatures (get/page/files/sizes/splice/changed_entries) so mounted_work.rs needs no change;
  I will NOT edit mounted_work.rs, landing.rs or anything under the mount bridges. If you need a new WorkFiles method
  (e.g. a page read over pieces for your base plane), tell me and I'll add it here.
