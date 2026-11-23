# w-git -> main: the verbs round that finds the shallow fetch's path (r18's `Pack(Store(Unreadable 2a3c9b98))`)

Run it after r89, on the big Studio, so the two don't overlap. Nothing here lands on main: the lift is turned on in
the round's own worktree only.

## What it is for

r18's verbs over git/git with the lift on: the `shallow` row's clone over HTTP was answered refused
`Pack(Store(Unreadable { id: sha1:2a3c9b98... }))`. On a six-commit dataset the same row passes, so the path needs
git/git's size. This round records which object it is, git's exact request, and the server's counters for that fetch,
so I can make the unit red and find the read that refuses without bringing the window back.

## The command

At main 1d7fd0ec4 or later (it holds the tag-peel fix, 6eae9f8bb, so push-tag-of-a-tag should pass with the lift on):

```sh
unset ANTHROPIC_API_KEY
W=~/slates-work/wgit-shallow-lift            # a fresh worktree of main, the round's own
git -C ~/slates-work/<your main checkout> worktree add --detach "$W" origin/main
cd "$W"
# The lift, in this worktree only:
sed -i '' 's/^pub(crate) const LANDED_LET_GO: bool = false;/pub(crate) const LANDED_LET_GO: bool = true;/' \
  crates/server/src/vcs_store.rs
grep -n '^pub(crate) const LANDED_LET_GO:' crates/server/src/vcs_store.rs   # must say true
mkdir -p ~/.cache/main-rounds/shallow-lift
CARGO_INCREMENTAL=0 \
SLATES_TEST_GIT="$(which git)" \
SLATES_TEST_GIT_DATASET=~/.cache/w-interop/git-git.git \
GIT_TRACE_PACKET=~/.cache/main-rounds/shallow-lift/trace.txt \
cargo test --release -p slates-cli --test git_correctness -- --ignored --exact \
  stock_git_verbs_on_a_real_repository_match_stock_git --nocapture \
  > ~/.cache/main-rounds/shallow-lift/verbs.log 2>&1
echo "exit $?"
```

`GIT_TRACE_PACKET` makes every stock git command of the round write its packet lines to one file. It is large (every
verb's), but only the `shallow` row's fetch matters. If you'd rather not write it, drop that line: the server's log
still names the object.

## What I need back

1. `grep -E '^(PASS|FAIL)' verbs.log`: every row's outcome.
2. `grep -n 'answered refused' verbs.log`: the refusal, with the repository and the object id.
3. The `shallow` row's fetch request from the trace:
   `grep -n 'deepen 1' trace.txt`, then the 40 lines before and 40 after that line.
4. Which object 2a3c9b98 (or whatever id the refusal names this time) is:
   `git -C ~/.cache/w-interop/git-git.git cat-file -t <id>` and `git -C ... cat-file -s <id>`.

## Green and red

- Red (expected): the `shallow` row FAILs with `remote: aborting due to possible repository corruption on the remote
  side`, and the log has `answered refused: Pack(Store(Unreadable { id: ... }))`. That's the finding I need.
- Green: every row PASSes, including `shallow`, `push-tag-of-a-tag` and `delete-tags`. That would mean the fetch-side
  stops already landed cover it at this main (0c3f42cea, 7f7793c58, 56fbf0381), and the shallow item closes.
- Either way, the HTTP stall you're bisecting (r108) can also show here as a step past PATIENCE. If it does, the row's
  failure is the stall, not this, and I'd want r108's answer first.
