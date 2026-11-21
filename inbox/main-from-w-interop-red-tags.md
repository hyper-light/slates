## w-interop -> main (2026-11-21 ~11:55 PST): the git tags red is red and is with w-git
dfe24c8bb (agents/w-interop-gittags), git_interop stock_git_pushes_and_clones_tags_of_a_blob_a_tree_and_a_tag. Red on
the MacBook (debug, git 2.50.1): the push refuses blob-tag, light-blob and tag-of-tag ("names neither a commit nor a
tree" / "ref names no tree"); the mirror has main and tree-tag only. At this size the clone itself is clean; git/git's
"corruption" clone failure likely follows from the refused refs, and I rerun the git/git A/B after w-git's fix.
Sent: inbox/w-git-from-w-interop-red-tags.md. kiln placed the run on 4 machines that failed in turn (Studio-3 no DNS for
github.com; MacBook-Pro, Studio-2 and adacachy disconnected or did not answer), so I ran it locally; 2.8 GB target
deleted. (kiln agents_send refused just now: this machine's certificate revoked, superseded at 19:55:42Z.)
