# w-svn → main: the KeptText move is done (A-842); r2 still owed to me

All three pieces are on origin/main:
- 41ce54f67, merged 270df0d72: `migration::Standalone`.
- e465b0e0c, merged dba4ae793: the integration tests, benches, and the ssh and presenter tests moved onto it.
- cc55404fa, merged 2ed0eec43: A-842. The Subversion records keep no text. `KeptText`, its skip-delta chains and
  window reads are removed, and a text a revision carries inline is refused `Corrupt` (E160004).

Tests:
- svn lib 584/584, with the 6 tests of the kept deltas removed and an inline-refusal test added.
- `fsfs_view` 5/5, `propfind_allocations` 1/1.
- server `--lib svn` 113/113.

Also changed:
- A replay is two steps (`replay_step` lost its row count; the vcs_store caller is updated).
- `examples/changeset_ab.rs` is retired, my default (a), since you did not answer. BENCHMARKS keeps its numbers at
  b51f908f2.
- The ignored git-2000 ssh measurement now prints `records_stored_text_bytes` where it printed `records_text_bytes`.

For you:
1. **Run request:** `cargo test -p slates-svn --test fsfs_view stock_svnadmin_and_svnlook_read_the_view` on a host
   with svn 1.15. The view is all PLAIN now. Red would be `svnadmin verify` failing, or `dump` / `dump --deltas`
   differing from full.dump / deltas.dump.
2. **Your call:** the FSFS view's DELTA writer has no source now; `TextDeltas` answers none everywhere. Remove it, or
   keep it for per-file deltas derived from the changeset log (Ada's 2026-11-20 13:35 ruling)? It is in GAPS.
3. **Still waiting:** your r2 git-2000 re-measure of A-835 (`~/.cache/main-svn-attr/r2/`), for finding (2).
