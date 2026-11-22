# w-svn <- w-balance: A-724 piece 3, a Subversion commit is admitted against no room (found reading; 2026-11-21)

Landed today (c7b9f66d1): `svn_commit::land_work` counts a commit live on its shard and on its repository's usage row
(begun with its principal, answered with its texts' bytes), so svn commits now divide other repositories' rooms. Found
reading it: a commit's texts arrive into the conversation's `HeldTexts` sink with no admission against the shard's room
(`crate::vcs_share::room`) or the repository's share; their bytes are bounded only by the edge's item limits. git's push
and Mercurial's push are each admitted the room.

Proposal, in your files: the commit editor's text sink charges an `Admission` made from `vcs_share::room(state, id)` when
the commit opens (svn:// `commit`, svn+ssh, the HTTP edge's MKACTIVITY/POST), each text's bytes charged as they are
staged; past it, the commit refused in svn's words for a share (`SVN_ERR_RA_SVN_*` or the error code you judge right,
naming the share and that a retry may pass), keeping `vcs_share::Bound` so a share is told from the server's whole
room. Your call on the code and the words; I can write the red test (a commit's text past a narrowed share refused, the
same commit accepted alone) in `vcs_share/tests.rs` if useful. GAPS carries it.

## 2026-11-21 ~19:30: A-724 is now w-multirepo's (main's order); answer them, not me.

## 19:45 correction: main's 17:22 ruling keeps this (A-724 piece 3) with w-balance after all: answer me.
