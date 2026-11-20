# A version a work submitted to a Subversion repository's green made no revision (2026-11-20, w-shared, A-664)

## Description
A work submitted to a Subversion repository's green was accepted as the green's next version, and nothing made the
Subversion revision of it. The repository's records stayed at their youngest revision while the green moved past it.

## Root cause
`verbs::judge_submission` makes git's commit of a submitted version (`vcs_store::commits::make`, A-664 part 3) and
nothing for a Subversion repository. §4.17.4 item 1 holds version N of the one green to be revision N; the owner's
step (`repository::svn_commit::commit`) lands a client's commit only while the green is at the records' youngest
revision.

## Impact
After one submit, every later `svn commit` to that repository was refused (the green not at its youngest revision,
`SVN_ERR_FS_CORRUPT` 160004 over ra_svn), and no Subversion client saw the submitted version. Found by reading; the
failing test reproduces it through the verbs (a submit, then a client's revision through the owner's step).

## Edits
- `crates/server/src/vcs_store/svn_made.rs` (new): the revision of a submitted version, its rows from the change
  index, read before from the records and after from the green.
- `crates/server/src/repository/svn_commit.rs`: `land_made`, the revision checked and its record appended in the
  submit's transaction in an entry holding it alone, then applied.
- `crates/server/src/verbs.rs`: `judge_submission` calls `svn_made::make` after git's made commit.
- `crates/svn/src/commit/drive.rs`: `text_of`, a text's identity and digests for a row.
- Test: `repository::svn_commit::tests::t_a664_svn_a_submitted_version_is_the_revision_the_server_makes`.
