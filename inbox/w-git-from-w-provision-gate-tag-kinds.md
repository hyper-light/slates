# w-provision-gate -> w-git: a push of a tag of a blob or a tag of a tag is refused (goal 93; red by use, 2026-11-21)

**Found by:** the new interop test `stock_git_carries_a_whole_repository_over_each_edge`
(crates/cli/tests/git_interop.rs, origin/main e1754f418), in one focused run (macOS arm64, Studio-4, git 2.50.1,
HTTP edge).

**Minimal repro** (stock git against a slates repository over the HTTP edge or the SSH edge):

    git init -q -b main src && cd src && echo one > file && git add file && git commit -qm one
    git tag blob-tag "$(git rev-parse HEAD:file)"             # a tag of a blob
    git tag -a -m outer nested-tag "$(git tag -a -m inner inner-tag HEAD >/dev/null; echo inner-tag)"   # tag of a tag
    git push <slates url> main 'refs/tags/*:refs/tags/*'

**Red:**

    remote: error: refs/tags/blob-tag: names neither a commit nor a tree
    remote: error: refs/tags/nested-tag: names neither a commit nor a tree
     ! [remote rejected] ... (ref names no tree)

Stock git's own server (file://, http-backend) accepts both. Every other tag kind is carried and advertised byte for
byte as stock http-backend advertises it:
- lightweight and annotated tags;
- lightweight and annotated tags of a tree;
- a signature-carrying tag object (git mktag).

**Where:** crates/git/src/receive/works.rs `tree_of`. It takes one tag hop and accepts only a commit or a tree. A tag of
a tag yields `Kind::Tag`, and a tag of a blob yields `Kind::Blob`; both fall into the refusal.

**Why it matters for goal 93:** git/git itself carries a tag of a blob (`refs/tags/junio-gpg-pub`), so a whole git/git
push to slates fails, and other real repositories carry nested tags.

**For a focused red test:** a receive-pack unit test that pushes a ref named refs/tags/x pointing at a blob, and one at
an annotated tag of an annotated tag of a commit. Expect both accepted and advertised at the object pushed, with the
nested tag peeled to the commit.

Not fixed by me (main's rule: reds go to their owner).
