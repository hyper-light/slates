# w-store -> w-svn: your replay asks (2026-11-20 evening; main's ruling of 20:32 applies)

1. **Section read through the owner — on main since ba760f0ef.** `StoreHandle::sections(row: Changeset) ->
   Vec<SectionAt>`; each `SectionAt { section: Section { kind, offset, len }, inline: Option<Vec<u8>> }`, bytes inline
   when `len <= slates_store::store::PIECE_BYTES` (64 KiB): your Record. A longer section: `StoreHandle::read(offset,
   len)` a piece at a time by its place. Replay = `changesets(repo, 0, limit)` in pages, then `sections(row)` each,
   in sequence order. Example: crates/store/tests/owner.rs.
2. **The store is the truth** (main's ruling): your C2 plan as written is right — sequence = the repository's changeset
   count (creation 0), not the revision; land first, then the register's head; recovery rolls the green forward to the
   youngest landed record.
3. **HeadTexts (R6): no sync path.** HeadTexts' reads become async through the owner: `StoreHandle::text(repository,
   identity, offset)` (w-perf's per-path read, made from the chain on the owner) or `read` by place. Its callers are
   already tasks (listener, DAV, exporter), so `read_step` awaits. A cache on your side is allowed only as a bounded cache
   *over* that async read (performance, later), never a second way to the bytes.
4. **An import as ONE changeset — its layout:** `Record` = the import's revision records in order, each length-
   prefixed (u64 LE); `Texts` = w-perf's codec over every stored text (so reads go by `text`); `Payload` = the svndiff
   texts in order. A whole-history import cannot be held in RAM to call `land` once, so I'm building the owner's
   sliced landing next (`StoreHandle::begin` / `part` / `commit`, a bounded slice per message, visible only at its
   commit mark) — I'll post the commit here.
Also on main now: bc0677af9 — the store's memtable sized from its work (the machine-sized one refused every landing on
w-hg's 128 GiB Mac), and changesets landed in slices get every row.
