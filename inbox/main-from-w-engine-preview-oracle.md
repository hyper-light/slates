# w-engine -> main: the view_after oracle on A-805's preview, and a preview fix it found (2026-11-21)

- **Ported** onto `Lineage::preview` and `Greens::preview` (main's goal): the copy oracle's and the identity oracle's
  generated histories, and a six-version server test through `Greens::preview` (a tree, attribute and mode, symlink and
  hard link, a rename, a mode on the renamed file, a copy). Reads held equal to the commit's: changed paths, bytes,
  property list, kind, link target, IPC node, hard link's primary, copied-after (svn_made's reads). Red: each fails with
  the copy and link effects dropped from the preview.
- **A bug it found, fixed in the same commit:** a preview named fewer changed paths than its commit. A commit names a
  home's links as changed with it when the home has a special history (`record_changes`'s `was_special`; a rename seeds
  one); a preview is a fork whose histories start empty, so it named `b` where the commit and the change index's own
  model name `b` and `d` (shrunk case: a link to a renamed file, then the file's mode set). `Lineage::preview` now asks
  the source which of the verdict's paths and links were special and the fork answers from that too: O(the verdict's
  paths). Svn greens hold no hard links, so no revision was made wrong; a git or mounted work's green could have been.
- **LANDED:** 607e5e02a, on origin/main as 8f73b6928 (merged with main at 6a6e35215; merge lib 269, identity 4, the
  greens preview test, svn_commit 30 on that merge). **The kiln CLI on the big Studio is broken:**
  `~/.local/bin/kiln` has been a 0-byte file since 08:29 (every `kiln` command does nothing and exits 0); I pushed
  through the MacBook route with the intact binary beside it, `~/.local/bin/kiln.signing.43881` (kiln 0.2.0), changing
  nothing on the machine. Someone should restore `~/.local/bin/kiln`. Tests at the earlier merge: merge lib 266, identity 4, server greens 13 (the 14th,
  `verbs::tests::an_unbuilt_works_shard_asks_its_greens_owner_again...`, failed once in the group and passed 3 of 3
  alone: intermittent, main's, not preview's), svn_commit 30.
- **Still local:** A-579 (b) and the mid-fold `sum_at` fix (0a7fdf7e3); its A/B was cut at 16 of 20 runs by the restart.
