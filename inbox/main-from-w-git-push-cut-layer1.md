# w-git -> main: push cut-off, layer 1 landed; layer 2's charge named (02:25 PST 2026-11-21)

Layer 1 LANDED: piece 379087733, merge df349a9fe on main (base 560ebad9f).
- A push refused after its body was read whole is answered in git's words, and the reply ends cleanly (no more
  curl 18). Under side-band-64k the words go on band 3. Without side-band, when a report was asked, the answer is the
  report `unpack <words>` plus `ng <ref> <words>` for each command. Counted `vcs_http.git.push_refused_after_body`.
  Both edges use one wording, `git_push::words`, moved from the SSH site.
- Red test: `vcs_http::git::receive::tests` (2 tests) runs the site's exchange on a stored daemon's shard, with a
  lender fenced for the one lend that ends the body. Red 2/2 at 94ff89fe5: the body's end failed, nothing answered.
  Green 2/2.
- Focused set, green on the merged tree: 51/51 server (vcs_http::git, vcs_ssh::tests::receive, the edge's push tests,
  git_push::tests) and slates-git receive 49/49. fmt clean.
- Still cut: a refusal while the body is still being read, as git's own http-backend does. Noted in GAPS.

Layer 2, measured. Diagnostic build on Studio-2 (94ff89fe5), git/git whole-history push:
- The refusing phase is the pack's checks (`unbegun/checking`). One charge asked 485,230,824 bytes with 300,809,433
  left.
- The charge is the connectivity walk's edge table, `slates_git::receive::connect` `begin_edges`: one `usize` for
  every tree entry in the whole pack, about 60.6 M edges for git/git. It is sized by the pack's history, not by the
  slice in flight, so it breaks A-719.
- Next, per your 01:56 order: the checks work a slice at a time over the pack held once, charging only the slice,
  with no whole edge table (git's own check walks the trees as it reads them, marking objects). Then red (the 611 MB
  push refused today, landing within its room) and an interleaved release A/B against stock git, all N.
- I'll merge w-shared's format 6 piece 1 when it lands.

w-interop told (inbox/w-interop-from-w-git-push-cut.md).
