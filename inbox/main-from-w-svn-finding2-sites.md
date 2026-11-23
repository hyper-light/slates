# w-svn → main 01a20182: finding (2), the churn attributed by allocation site (2026-11-22 ~02:00 PST)

## Method

Local and not landed: a test-only global allocator records a backtrace for every allocation of 16 KiB to 1 MiB, the
sub-floor band, during an in-process import of 100 revisions of one 256 KiB file (25.6 MB of text). Bytes are totalled
by the first slates frames.

## Result

About 440 MB was made and freed in the band, **17 times the text**. Each text is copied about 15 times on the heap:

| bytes | count | site | owner |
|---|---|---|---|
| 92.5 MB | 400 | `ContentStore::put_window` ← `svn_commit::store_step_from` (the test's content store; heap windows) | content store |
| 70.4 MB | 379 | `Importer::text_piece`: the dump's text assembled in pieces | **w-svn** |
| 26.2 MB | 100 | `chain_entry::encode_carrying` ← `push::append_versions` | merge, push |
| 26.2 MB | 100 | `Increment::encode` ← `push::append_versions` | merge, push |
| 26.2 MB | 100 | `Green::judge_held` ← `ImportBatch::stage` | merge engine |
| 26.2 MB | 100 | `ContentStore::object_bytes` ← `Building::text_bytes` (the text read back whole) | **w-svn** |
| 26.2 MB | 100 | `ContentStore::release_one` (a copy made when releasing) | content store |
| 26.2 MB | 100 | engine `push_slice` ← `apply_recorded` | merge engine |
| 26.2 MB | 100 | engine `push_slice` ← `commit_held` (the scratch member) | merge engine |
| 26.2 MB | 400 | `landing::content_piece` (changeset payload pieces) | **w-svn** |
| 26.0 MB | 99 | `Building::insert` ← `replace_text` | **w-svn** |
| 24.7 MB | 400 | `stage_texts` piece copies | **w-svn** |

## Plan, under your ruling

Red first, then remove by design.

**w-svn's five copies, now (~160 MB of the 440, about 6 copies of each text):**
- the importer streams a text into its store, in place, rather than assembling a `Vec`;
- `stage_texts` and `content_piece` lend slices instead of copying pieces;
- `Building` takes the bytes the landing already holds, rather than reading them back from the store;
- the increment's content is the lent text where the engine's type allows it.

**The red:** `an_import_measured` gains an assertion on footprint after idle against a derived multiple of what the
import still holds (heap growth plus arena plus content resident), with its time within A's. Plus an in-process
attribution test in the svn import tests. It's the band's bytes per text byte, so it is a measure of what is removed,
not a gate.

**Not mine; for you to route:**
- the content store's `put_window` and `release_one` copies (w-multirepo's A-851 module);
- the engine's `judge_held` and two `push_slice` copies, `Increment::encode` and `chain_entry::encode_carrying`
  (w-engine, or w-rewrite-ops for push and A-814).

Each owner can reuse the local attribution recipe; I'll put it in a test file they can run. Tell me if you route
differently.
