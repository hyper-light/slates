# main-from-w-hg: C1's import, scope to rule before I build (2026-11-22 ~00:05 PST, after the fixture lane's 98/98)

**Where it stands** (GAPS 458, 3277, 3411):
- Step 3 is CLOSED: the import lands as segment changesets, and restart replays their records by identity with no payload read.
- Steps 1a and 1b are BUILT: clone and pull file revisions come from the changeset store.
- **Left:** the import keeps every revision's text whole in the content store (`crates/hg/src/import.rs` `held.put(store, &text)`). That's R2's 5.73 GB.

**Readers still using those content-store texts:**
- (a) The served view's changelog and manifest texts (`Indexed::changeset_text`, `manifest`, `.hgtags` via `text_of`).
- (b) A push's verify, reading a held file text (`push/verify.rs` `held_text` → `file_text`).
- The intake's bases already read through the store (BaseCache, b2).

**Proposed piece (step 2 for the import, red first):**
1. The import keeps no FILE text in the content store. Each file revision's row names its text by identity, and its bytes by the segment's Texts row, as a push's do.
2. Reader (b) goes through the store's StoreWants, as the intake's bases do: parked between turns and asked, never read whole in a step.
3. Changelog and manifest texts stay in the content store for now. The served view reads them per request, and they're a small share of R2. Moving them is a later piece.
4. Red: after importing a fixture, `ContentStore::used` equals the changelog and manifest bytes alone, where today it counts every file text. A clone and a push over the import still verify clean against stock 7.2.

**Q1.** Is that the scope, or do you want changelog and manifests off the content store in the same piece?
**Q2.** Does anyone else hold `import.rs` or `push/verify.rs` now?
