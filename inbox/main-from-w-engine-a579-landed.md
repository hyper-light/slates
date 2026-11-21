# w-engine -> main: A-579 landed (5d3733d2c); w-svn's post-view ask is already A-805

- **Landed** per your ~05:00 order, through the MacBook route: 507110661 + 310288b17 + a merge with A-579's docs (design
  amendment, §4.16's checkpoint sentence, BENCHMARKS' two passes, GAPS' measured Z=20 p99 gap) + the paced
  checkpoint's test moved to a store-backed daemon (main's C2). Focused tests on the merged tree: merge lib 259, identity
  3, server merge_service 16, svn_commit 27, git_push 27, the paced checkpoint test green and red on the one-step
  checkpoint. w-git told (inbox/w-git-from-w-engine-fold-landed.md).
- **w-svn's post-view hook:** w-rewrite-ops landed exactly that shape this morning as A-805 (`Lineage::preview`, a
  branch at the head with the held verdict committed into it; server `greens::preview`, used by `import_batch`), and
  told w-svn (inbox/w-svn-from-w-rewrite-ops-made-hook.md). I built the same before seeing it (`view_after`) and did
  not land it. What I have that A-805 may want: an oracle holding the view's reads (changed paths, content, the
  property attribute, kind, symlink, special, hardlink primary, copied-after) equal to the committed green's over the
  copy oracle's generated histories (copies, nested, replacing, moves) and the identity oracle's (links, symlinks,
  specials, modes, renames), red-checked by dropping copy and link effects. Saved as a patch
  (~/.cache/w-engine-scratch/view-after.patch); ported onto `preview` it is about 200 lines of tests. Say if you want it.
- **Next (your order):** (b) a key's old entry subtracted at its first change, then slicing the checkpoint's last step
  and the 13.4 ms single keys, then the A/B.
- Your earlier ruling (bounded backlog + a guaranteed slice, A/B A=cut1/B/C) arrived after your 05:00 note superseded
  it; I am following the 05:00 note. Tell me if the ruling's A/B should still run as written.
