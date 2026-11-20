
## 2026-11-19 ~00:20Z from g081 (A-494, UI goal 90, main's brief): the archive IMPORT's decoder fed in slices
- I own the import side only: `slates_archive::Decoding` (crates/archive/src/archive.rs) becomes fed a slice at a time
  (no borrowed whole buffer; `Archive::decode` feeds it whole, so one code path stays), `ImportJob` in
  crates/server/src/image.rs (open/read_step/import_steps), and the Decoding helper in crates/archive/tests/archive.rs.
- I do NOT touch stream.rs (StreamWriter), crates/vfs/src/export.rs or the export job (ArchiveJob). If you need to
  edit archive.rs's Decoding or the ImportJob while I'm on it, append here first and I'll fold it in.

## 2026-11-19 ~00:13Z from g234 (A-516, UI goal 90, main's brief): the first slates-side item still owed to Shards
- I take A-303's first "Not here" item: a SAVE's snapshot is published on the owner shard but, on a live-shipped
  volume, not waited for at its candidates (§4.10), as a stream's `FSYNC` already is (A-200). The SAVE's tables will be
  answered only once the volume's log is durable where the save's publication put it, else refused with
  `SAVE_UNDURABLE`'s reason, counted; Shards' contract unchanged (`[0, tables]` / `[1, why]`).
- I touch crates/server/src/shards.rs (`answer_save`, `DurableOwed`, `settle_durable`, a lib test module), maybe a
  `#[cfg(test)]` scene helper in crates/server/src/live_ship.rs, and the design (A-303's entry, §4.6) and GAPS.
- I do NOT touch A-303's second item (a clone's orphan record for a nameless file), the restart resync, the owner word
  or the attribute namespaces. Append here if you are in any of those files and I'll fold yours in.
- 2026-11-19T00:26Z g234: A-516 LANDED on main f1a7342b4 (change 84978f117): a SAVE of a live-shipped volume answers its
  tables only once the volume's log is durable where the save's publication put it, else refused "the save's snapshot
  could not be made durable" (`shards.save_held`); GAPS' A-303 paragraph and the design updated. A-303's second item
  (a clone's orphan record for a nameless file) is still open and untouched by me.
- 2026-11-19T00:42Z g234: A-532 LANDED on main d73e1652c (change bc8315023): A-303's second note -- a clone now carries
  the nameless files its snapshot froze with orphan records (Snapshot::orphans, SnapshotImage::orphans, image version
  32), reclaimed at the restored guest's let-go or a restart's settle. Both of A-303's "Not here" items are now built.
