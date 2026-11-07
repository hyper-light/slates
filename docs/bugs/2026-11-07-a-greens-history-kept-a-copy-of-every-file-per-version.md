# A green's history kept a copy of every file per version (2026-11-07, §4.16 "Splice" and "delta retention before folding", §4.2 all-cost admission)

Found by agents/loadtest-scale attributing the load test's memory per version (GAPS, "Memory per client,
attributed") and its `BudgetExceeded` refusals. W2 of the load test's walls, assigned by the supervisor on
2026-11-06. Amendment: AMENDMENT-W2.

## Description

The merge engine keeps every file's value at every version a live reader can still name, so a lagging work reads
its base and the verdict maps through the versions since. It kept them as whole copies:
- `Green::commit` pushed a copy of each changed file's new value into `content_history`, beside the head copy in
  `content`;
- `Green::history_reservation` charged the whole previous file to every submit that touched it.

So a version that overwrote one byte of an F-byte file retained F bytes. Behind a work holding the reachable floor
(any work not yet resubmitted holds its base), the retention cap then refused submits after cap / F versions, however
little each changed. The engine's own docs called it "the measured optimization owed": "the design's chain shares
these histories copy-on-write; here they are full copies". §4.16 states the rule: the merge task "splices accepted
ops into a new version by extent-list surgery without copying bytes".

## Measured

- The 3,000-client rung over `kernel/` on the quiet Studio (32 cpus, `--think-us 0`, 3 rounds, 2026-11-07, three runs
  each of main 4326a9d8 and of W3): 2,410 to 2,456 `SubmitAccepted: BudgetExceeded` in every run of both. The refusals
  were the history, not the chain.
- `crates/server/tests/green_retention.rs` (`a_green_behind_a_held_floor_retains_what_its_versions_changed_not_the_file`):
  a 1 MiB file behind a work held at its creation, then 64 versions each overwriting 64 bytes, under a 16 MiB
  retention cap.
  - Red on CI (run 42358895080, ubuntu-latest): 16 of 64 accepted, then `BudgetExceeded { available: 0 }`.
  - Green: see the diagnostics lane below.
- `crates/merge/tests/engine.rs` measured the amplification as a passing test: 8 four-byte edits of a 64 KiB file
  retained 8 × 64 KiB. It now asserts less than one copy for all eight.

## Root cause

The content history's values were `Vec<u8>` copies, and no representation shared bytes between versions.

## Fix

- **`crates/merge/src/history.rs` (new).** An arena of immutable chunks with a reference count each (handles and
  counts, no `Arc`). A value is a list of pieces (chunk, offset, length).
  - `Value::spliced` builds a version from the one before it by the same walk as the engine's `apply`: every run the
    ops keep is shared, and the bytes they add are copied into one new chunk.
  - The result is checked against the version's bytes. One that does not reproduce them (a post-state shorter than
    its ops declare, which `apply` clamps) is stored whole.
  - A value whose piece list would outweigh its bytes is stored as one chunk: no version costs more than a copy, which
    is what every version cost before.
  - A proptest oracle holds every version to `apply`'s bytes over generated chains, the arena to exactly what the
    kept values name, and each version's growth to the bytes its ops added.
- **The engine.**
  - `content_history` holds those values. The head stays a contiguous `Vec<u8>`, so head reads, the verdict's
    memcmp, identity hashing and checkpoints are unchanged.
  - Retention is the chunks and piece lists less the current values, the same meaning as before ("what the history
    holds beyond the current values"). The recount the running totals are held to counts the distinct chunks the
    values name.
  - Folding releases the dropped values' chunks.
  - `history_reservation` charges, per path:
    - a content edit's overwritten, deleted and truncated bytes and its new list, or the old value when it will be
      stored whole;
    - a replaced path's old value, plus a piece for one given new content;
    - an IPC node.
- **The reads.**
  - `content_ref_at` and `files_at` return `Cow<[u8]>`, borrowed from the head when the file is unchanged since the
    version, copied out of the pieces when it changed after.
  - New `content_len_at`, `file_lens_at` and `content_page_at` read lengths and pages without copying the whole file.
    A work's listing and its pages use them (`WorkFiles::sizes`, `WorkFiles::page`), so a lagging work's paged read
    copies only the page.

## Impact

Before the fix, a version's retained cost was the size of every file it touched. A lagging reader turned many small
edits into the cap's refusal, and memory grew by whole files per version. After it, a version costs the bytes it wrote
and its piece lists. The head is still held twice (its contiguous copy and the history's chunks), as before.

## Siblings, reported, not fixed here

- A rename copies the source's bytes into the destination's history as one chunk rather than sharing its pieces.
- The origin of a base-seeded green is held as chunks and as the head copy, so twice (it was before as well).
