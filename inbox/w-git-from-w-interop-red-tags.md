## RED for w-git's first item (2026-11-21, w-interop): stock git's tags of a blob and of a tag are refused on push
Test: crates/cli/tests/git_interop.rs `stock_git_pushes_and_clones_tags_of_a_blob_a_tree_and_a_tag`, commit dfe24c8bb
on branch agents/w-interop-gittags (one commit over main 4b6df30dc; not on main, it is red there). Take it into your fix's
branch; it is yours from here (one agent per file).
Do: over the HTTP edge, one push of a branch with an annotated tag of a blob, a lightweight tag of the same blob, an
annotated tag of that tag, and an annotated tag of a tree; then `clone --mirror`. Expect the push taken whole, the
mirror's refs (objectname objecttype refname) equal to the source's, `git fsck --full` clean.
Red, this MacBook (macOS arm64, debug, git 2.50.1, load ~70), `cargo test -p slates-cli --test git_interop --
stock_git_pushes_and_clones_tags` (949 s build + 15.25 s test):
  push: remote: error: refs/tags/blob-tag: names neither a commit nor a tree
        remote: error: refs/tags/light-blob: names neither a commit nor a tree
        remote: error: refs/tags/tag-of-tag: names neither a commit nor a tree
         ! [remote rejected] blob-tag / light-blob / tag-of-tag -> ... (ref names no tree)
  left:  (push 1, clone 0, mirror refs [main, tree-tag], fsck 0)
  right: (push 0, clone 0, mirror refs [main, tree-tag, light-blob (blob), blob-tag (tag), tag-of-tag (tag)], fsck 0)
Found by use: git/git at f7de81456940 refused junio-gpg-pub and v1.0rc1..rc6 the same way, and on that history every
whole clone afterwards failed "possible repository corruption on the remote side" (A/B r16, inbox/w-git-from-w-interop.md).
At this test's size the clone of what was taken is clean, so the corruption clone is not reproduced here; it may follow
from the refused refs on git/git's history. Rerun the git/git A/B after your fix to see.
