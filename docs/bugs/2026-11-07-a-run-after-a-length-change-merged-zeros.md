# A run after a length change in its file merged zeros, and every increment carried its files' whole regions (2026-11-07, §4.16 "Submission", "the sealed post-state")

Found by agents/loadtest-scale while proving W2. The green-retention test was refused `TableAtCapacity` at 44 of 64
versions of 64-byte edits, which the edits' size could not explain. Amendment: AMENDMENT-W2 (with the history's
copy-on-write).

## Description

An increment names the bytes its content ops add by their offset (`src`) in its post-state. Two parts of the
system disagreed about what `src` means.

- **The deriver** (`slates_merge::derive`, `run_src`; `increment::seal`) gave each run `src` = its offset in the
  work's final file plus the start of that file's region. The regions were laid out by each edited file's whole
  final length.
- **The seal** (`verbs::assemble_post_state`) sized the post-state to the largest `src + len`, zero-filled it, and
  copied each run from the work's file at `op.at`. That is the op's offset in base coordinates, not where the work's
  file holds the run.

So:
- **Corruption.** A run after an earlier length change in the same file was read from the wrong place. A work over
  `abcdef` that deleted its first two bytes and then overwrote its last two with `XY` read the run at base offset 4
  of its 4-byte file, past the end. The run stayed zero, and the green merged `cd\0\0` where the work held `cdXY`.
  Every accepted increment whose file had an insert or delete before a later run in it was at risk.
- **Bloat.** An increment carried every edited file's region up to its last run, zero-filled. A 64-byte overwrite
  at offset 640 KiB of a file carried 640 KiB into the green's chain, its identity hash and its replication. The
  chain grew with where edits landed, not with what they wrote.

## Measured

- `crates/server/tests/green_post_state.rs` (`a_run_after_a_length_change_in_its_file_merges_its_own_bytes`):
  - red on CI run 42365926059 (ubuntu-latest): the green's head was `[99, 100, 0, 0]`, the work's file
    `[99, 100, 88, 89]`;
  - green: the W2 lane.
- `crates/server/tests/green_retention.rs`, with the history fixed but the post-state not: refused `TableAtCapacity
  { table: "green_chains" }` at 44 of 64 under a 16 MiB budget (CI run 42362505313). That is about 357 KB of chain
  per 64-byte version, the regions' zeros.

## Since when

The `op.at` read is in `assemble_post_state` as first written: 34548a59 (2026-09-06, "merge: wire the submit flow",
§4.16 Phase 6 Task 6). 60460a8c (2026-10-06) only made its slices checked, and A-202 (020d59c6) kept it. Every
version of the seal has had it.

## Fix

- `increment::compose_volume_planned` lays the post-state out compactly: each added run at the next offset in the
  document's file order, then the xattr values, then the special nodes.
- It returns a plan: for each run's `src`, the run's offset in the work's file at the seal.
- `assemble_post_state` copies each run from there. `compose_volume` is the same document without the plan.
- The encoding is unchanged; only the `src` values are. A chain written before stays replayable, since `apply` takes
  each op's bytes from whatever `src` names.
- An increment now carries exactly the bytes its ops add, so a 64-byte overwrite anywhere in a file costs 64 bytes.

## Impact

Before the fix, any accepted submit whose work changed a file's length before a later run in that file could merge
wrong bytes (zeros) into the green, silently. The work and the green then disagreed, and nothing refused. Every
increment also carried zeros proportional to its edits' offsets into the durable chain, replication and the identity
hash.

## Siblings, reported, not fixed here

- Increments accepted before the fix may have merged wrong bytes into existing greens. A green's history cannot tell
  them apart, since the post-state holds the zeros the verdict applied. Re-running the affected works' edits is the
  only way to find them.
