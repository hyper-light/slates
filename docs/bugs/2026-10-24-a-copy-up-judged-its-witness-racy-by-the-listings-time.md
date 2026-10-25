# A copy-up judged its witness racy by the listing's read time: a file edited after its listing was racy for good (§4.5 copy-up, A-162)

- **Date:** 2026-10-24
- **Subsystem:** the base plane's copy-up (`crates/vfs/src/base.rs`, `copy_up`).
- **Severity:** performance, not correctness. A racy witness's fingerprint is not trusted alone, so every drift check of
  it hashes the whole file through its descriptor (`check_drift` step 1, `identity_of`): every read of an unpinned
  range, every `status`, every landing verdict. Drift was still caught, because the hash is the conservative branch.
- **Found by:** the Shards route's refill (Shards part 3, step 4). In
  `a_writable_ask_after_an_outsiders_edit_finds_the_map_refilled_and_writes_back_only_the_guests_store`
  (`crates/server/tests/shards.rs`), the host edits a 3 MiB base file in place after its directory was listed, and the
  guest's writable ask copies it up. The refill then read the file's first range in 76 slices, each hashing all 3 MiB,
  and the ask was not answered within the test's 5 s wait in a debug build. Proven by
  `a_witness_taken_after_an_edit_made_since_the_listing_is_racy_only_while_the_edits_tick_is_open`
  (`crates/vfs/tests/base.rs`), which failed before the fix ("an edit a second before the copy-up, at a microsecond
  granularity, is not racy") and passes after.

## Symptom

A file edited on the host after the overlay listed its directory was copied up with `Witness.racy = true`, however long
ago the edit's timestamp tick had closed. Nothing visible to a client but time: in debug builds, a read of a 2 MiB range
in copy-slice pieces took seconds.

## Root cause

`copy_up` computed `racy = listing.read_at_ns - fp.mtime_ns <= granularity` with a saturating subtraction. Both readings
have been on the host's clock since 2026-09-14
(`docs/bugs/2026-09-14-racy-rule-compares-monotonic-with-wall-clock.md`), but the reference point was the listing's read
time, not the copy-up's own `fstat`. A file edited after the listing has `mtime > read_at`, so the difference saturated
to zero, which is at most any granularity: racy. The flag is recorded in the witness and never cleared.

The design's sentence said the same thing (§4.5: "the file's mtime is not older than the listing's read time by more
than the filesystem's timestamp granularity"). That is the right question for trusting the listing's fingerprint, but
it is the wrong one for the witness: the witness's fingerprint is the copy-up's `fstat`, and what a later same-tick edit
could hide from is that `fstat`.

## Fix

`copy_up` asks the host's clock after its `fstat`: `racy = host.now_ns() - fp.mtime_ns <= granularity`, saturating, so a
timestamp in the future is racy. §4.5's sentence is amended (A-162).

## Sibling sweep

- `rewitness` records every witness `racy: false`. A file edited inside the rewitness's own tick is then caught only by a
  later fingerprint change, which a coarse-granularity filesystem may not show. That is correctness, not cost; reported,
  not changed here (it needs its own failing test).
- A racy witness stays racy after a drift check has re-hashed it at a time past its tick and found it equal. From then
  on, any edit moves the mtime to a later tick, so the fingerprint alone would do (git clears such entries when it
  rewrites its index). Until then, a genuinely racy large file hashes whole on every check. Reported, not changed here.
- The digest cache's racy rule (`keep_digest`) asks `HostFs::now_ns` already and was right.
- The listing racy rule (directories) is about the listing's own read and is unchanged.
