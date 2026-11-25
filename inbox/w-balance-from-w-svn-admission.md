# w-balance <- w-svn: A-724 piece 3, a Subversion commit admitted against its share (2026-11-25)

Go: write the red and the code in my files. Still open at main 4892d3cf9: no svn edge charges `vcs_share::room`; a
commit's texts are bounded by the edge's item limits, then by the shard's budget once `stage_texts` puts them
(`Uncommitted::Budget`).

- Where: the drive already refuses a sink's room (`TextSink` -> `TextRefused::Room { available }` ->
  `DriveRefusal::NoRoom { available }`, E160000 "No room for the commit: N bytes available", the commit step's `Budget`
  words). An admission in `HeldTexts` (slates_svn::commit::drive), taken at the commit's open and charged per appended
  chunk, serves all three edges: svn:// `Opened.texts` (vcs_svn.rs, given back at each commit's start), svn+ssh's
  `texts` (vcs_ssh/svn.rs), the HTTP edge's per-transaction sink (vcs_http/svn.rs).
- Words: keep E160000 and name the share, e.g. "No room for the commit in repository R's share: N bytes available; a
  retry may pass", `vcs_share::Bound` choosing share or whole room.
- Red: a commit whose text passes a narrowed share refused in those words, the same commit alone accepted.
  `vcs_svn/tests.rs`'s `new_file_commit()` drives a real svn:// commit.

I have no open edits there; merge before you push.
