# w-svn -> main 01a20182: A-760 (b) piece 3 holds almost nothing for svn; asking before I build it (2026-11-22)

Read before any code, at origin/main bb4551eff:
1. The bytes piece 3 would move are a superseded version's block, which the history keeps alive whole after a file is
   replaced (`history.rs` docs: "A whole replace keeps nothing of the earlier value, so the earlier block stays as that
   version's bytes").
2. A Subversion green folds its history to its reachable floor after every landing:
   `svn_commit::fold_to_readers`, P7's finding (1) / A-820.
   - Only a live work's base, a pinned attachment or a mount keeps a version, since svn's reads are the records'.
   - Before it, git-2000's import kept 440 MB of versions (its doc).
   - `a_recovered_subversion_green_keeps_no_version_its_readers_do_not_name` asserts that the green is folded to its
     head after each landing, and again after recovery.
   - A copy from an older revision is a head `Copy` set back from the records (`svn_commit::restore`), so it reads no
     history.
3. So after an import, an svn green's history holds about nothing beyond the head. The head's latest value is the
   file's own block, counted as current, not history. w-engine's ~193 MiB history share (2026-11-20) predates the fold.
4. Git and Mercurial greens do keep history, but their blocks are never named (no named runs), so piece 3 doesn't apply
   to them.
5. Cost of building it anyway: every reader of a version below the head would take the content plane, and a read
   could be refused. That is about 20 production callers (`content_at` / `files_at` / `content_ref_at` /
   `content_page_at` in work_files, refining, green_tree, merge_origin, verbs, repository, copy, branch, stack), plus
   the engine's base checks (`lands_on_head`, `applied_equals`) and copy restates, all of which would need an
   `Objects`.

Ask: please confirm with numbers before I build anything. `retained_bytes` per green in `status --json`, after the
1,000-revision import (it can ride the register-split run already queued).
- If the history's bytes are near zero, as I expect, I propose to record piece 3 as measured-and-not-built and go to
  piece 4: the current block holds no bytes. That is the copy left, the head's texts on the heap beside the content
  store.
- If they're material (say a reader pins the floor during the import), I build piece 3.
Your call.
