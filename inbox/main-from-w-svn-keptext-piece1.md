# w-svn → main: KeptText move piece 1 landed; one question, one run request (2026-11-21 ~16:30 PST)

Piece 1 is on origin/main: 41ce54f67, merged as 270df0d72.
- What it adds: `migration::Standalone`, records standing alone with their texts held apart and named stored, as a
  daemon's are. `Repository::take_texts` moved from the server's landing, which now calls it.
- The migration tests run on it: svn lib 589/589; server svn_commit/svn_import 45/45.

Piece 2 is in progress: fsfs_view, propfind_allocations, replay_bench, fsfs_view_bench, and the vcs_ssh and presenter
tests. Piece 3 then removes `KeptText`, its delta chains and window reads, `TextDeltas for Repository`, and the
records' fallback in `HeadTexts`.

1. **Your call:** `examples/changeset_ab.rs` (svn's arm of the changeset A/B) reads the records' own skip-delta deltas
   (`Repository::text_delta`), which piece 3 removes. Shall I
   - (a) retire it: the A/B is done and Ada ruled changesets; or
   - (b) have it compute each text's delta itself, with svndiff's encoder against the skip-delta predecessor?

   I take (a) unless you say (b).
2. **Run request**, on a machine with stock svn 1.15 (none on this Mac), once piece 2 lands:
   `cargo test -p slates-svn --test fsfs_view stock_svnadmin_and_svnlook_read_the_view`.
   - What changes: with texts apart the view writes every text PLAIN, as the presenter's does, instead of DELTA.
   - Red: `svnadmin verify` fails, or `dump` / `dump --deltas` differ from full.dump / deltas.dump.
