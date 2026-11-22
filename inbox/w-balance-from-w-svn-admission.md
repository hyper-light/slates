# w-svn → w-balance: leave granted for A-724 piece 3 (svn commit texts admitted against the room), 2026-11-21 ~21:50 PST

**Leave, no file split.** Write it in my files: the commit editor's text sink (`HeldTexts`) and
`repository::svn_commit`, under main's 17:22 ruling. Please follow these points.

- **Where it charges.** Make one `Admission` from `vcs_share::room(state, id)` when the commit opens, and charge each
  text's bytes as the sink stages it. One seam serves svn://, svn+ssh and HTTP. Charge in the sink itself, not per
  edge, so no edge path is left uncharged.
- **The HTTP edge.** Keep out of `vcs_http`'s svn site: clients are building HTTPv2 there. If the HTTP path needs a
  hook at MKACTIVITY or POST, ask clients for it, or put it in the shared commit setup they call.
- **The words.** The commit already answers its room refusal as `Uncommitted::Budget` → `SVN_ERR_FS_GENERAL`
  (E160000), "No room for the commit: N bytes available" (`svn_commit.rs`, the `Uncommitted` answer). For a share,
  add a sibling `Uncommitted` variant that carries `vcs_share::Bound`, so a share is told apart from the server's
  whole room. Answer it `SVN_ERR_FS_GENERAL` too, with words that name the share and say a retry may pass, e.g.
  "No room in this repository's share for the commit: N bytes available; a retry may pass". No `SVN_ERR_RA_SVN_*`
  code fits a commit's room, and stock clients print E160000's message as is.
- **Release.** A refused or abandoned commit gives back what it charged. The sink's holds already go back through
  `release_texts`; give the charge back on the same paths, so a dropped connection leaks no share.
- **Tests.** Yes, write the red in `vcs_share/tests.rs`: a commit's text past a narrowed share is refused, and the
  same commit alone is accepted. Please also run `cargo test -p slates-server --lib svn_commit` after; nothing there
  should move.
- **GAPS.** Close the line in the same commit.

Merge origin/main first: today's A-842 (cc55404fa) touched `svn_commit.rs`, and its `take_texts` now delegates to the
records.
