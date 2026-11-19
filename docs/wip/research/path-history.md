# Path history by API: `log <path>` and `--follow` from derived data (A-666 part 2)

Status: read 2026-11-19 by g302 for A-666 part 2 (UI goal 93; A-645 "Queries by API"), the evidence behind the
`history` type of derived data (`crates/vcs/src/derived/history.rs`). Tiers as in the design: A a paper, B a standard
or the vendor's own documentation, C deployed code, D a blog (flagged). Every quotation was read on 2026-11-19 from
the revision named, at the lines given; each copy was fetched on jupiter with `curl` from `raw.githubusercontent.com`
and its sha256 (first 16 hex) is in §1.

## 1. Sources and revisions

| Source | Revision read | sha256 (first 16 hex) |
|---|---|---|
| git `Documentation/rev-list-options.adoc`, `Documentation/git-log.adoc`, `revision.c` | git `master` at `e1ec09b23c8e0deb21a0b95ac5996422277395d4` (part 1's pin) | 00a963603ae954d1, 67e2e76723555409, a68aac8493fa43c7 |
| Mononoke's unodes: `eden/mononoke/mononoke_types/src/unode.rs`, `eden/mononoke/derived_data/unodes/derive.rs` | facebook/sapling `main` at `573994010efaffe5a2b4a9fd2a87499ed7caaaf3` (part 1's pin) | 13f62b04189a84a6, b155572df1f5409d |
| Mononoke's fastlog: `eden/mononoke/mononoke_types/src/fastlog_batch.rs`, `eden/mononoke/derived_data/fastlog/fastlog_impl.rs` | the same revision | b97473ab575a1bec, 6d5366649dfaf346 |
| Mononoke's deleted manifest: `eden/mononoke/derived_data/deleted_manifest/derive.rs`, `.../ops.rs` | the same revision | b0bedede3875acd6, ac0d78eba3cfc810 |

## 2. What the server must answer, in git's words

- **Which commits `log <path>` lists** [C: `rev-list-options.adoc` 461-463]: "Suppose you specified `foo` as the
  _<paths>_. We shall call commits that modify `foo` !TREESAME, and the rest TREESAME. (In a diff filtered for `foo`,
  they look different and equal, respectively.)"
- **The default mode** (507-513): "Commits are included if they are not TREESAME to any parent (though this can be
  changed, see `--sparse` below). If the commit was a merge, and it was TREESAME to one parent, follow only that
  parent. (Even if there are several TREESAME parents, follow only one of them.) Otherwise, follow all parents." Its
  short form (422-426): "Simplifies the history to the simplest history explaining the final state of the tree.
  Simplest because it prunes some side branches if the end result is the same". Roots (525-526): "Root commits are
  compared to an empty tree, so `I` is !TREESAME."
- **Which TREESAME parent** [C: `revision.c` 1001-1003, 1039-1066]: `try_to_simplify_commit` walks the parents in order
  and, at the first compared `REV_TREE_SAME` (with history simplification on), frees the rest and sets
  `commit->parents = parent`, marking the commit `TREESAME`. So the parent followed is the first TREESAME one.
- **`--follow`** [C: `git-log.adoc` 30-32]: "Continue listing the history of a file beyond renames (works only for a
  single file)." git finds the renames by comparing contents when it lists them. slates never does: "Composition of
  declared operations is arithmetic on facts; comparing file states to reconstruct operations is inference and does
  not exist in slates" (§4.16 "Composition at seal"). Renames and copies are what the works declared (`Rename`,
  `Copy` in the ops document, §4.16 "Data model").
- **The other modes** (`--full-history` 532-534: "always follow all parents of a merge, even if it is TREESAME to one
  of them"; `--simplify-merges`; `--show-pulls`) list other sets. This type answers the default mode only; each other
  mode is a type of its own (owed).

## 3. What Mononoke deploys

- **Unodes** [C: `unode.rs` 31-32]: "Unode is a filenode with fixed linknodes. They are designed to find file or
  directory history quickly which can be used to answer "log" or "blame" requests." A file unode holds "parent unode
  hashes", the content, the file type, a hash of the path and "linknode: the changeset that introduced this unode"
  (34-39); a tree unode holds its parents, "subentries: a map from base name to (unode hash, type)" and its linknode
  (44-47). The derivation is inductive (53-57): "For each changed file, create a new unode with parents set to their
  corresponding unodes and content set to the hash. Create new manifest nodes by applying changes and deletes
  recursively."
- **Reuse at merges, unodes v2** [C: `derive.rs` 360-388]: v1 gave a merge that "didn't touch "file.txt" at all" a
  unode of its own, so "file.txt"'s history "would contain a merge commit". v2's `reuse_file_parent` (410-430) reuses
  the first parent unode whose content and file type match; `reuse_manifest_parent` (390-408) reuses a parent tree
  unode whose `subentries()` equal the new ones. Their own caveat (378-388): when both sides modify a file to the same
  content, "it would be ideal to preserve that "file.txt" was modified in both commits. But we consider this case is
  rare enough to not worry about it." That case is exactly git's "follow only one of them": the default mode lists one
  side, as the reuse does.
- **Copy-from is outside the unode** [C: `unode.rs` 41-42]: "NOTE: copy-from is NOT part of the hash. The goal is to
  make copy information mutable so that we can retroactively change it". `derive.rs` 317-320: "in unodes copy
  information is ignored".
- **Deleted paths are a second structure** [C: `deleted_manifest/derive.rs` 60-99]: "Each manifest has an optional
  linknode. The initialized linknode points to the changeset where the path was deleted", with merge rules of its
  own (87-99); `ops.rs` 56-62 answers "if it doesn't [exist]: the unodes and changeset where the path last existed
  (there might be more than one if deletion happened in separate merge branches)".
- **Fastlog batches** [C: `fastlog_batch.rs` 51-55]: `MAX_LATEST_LEN = 10`, `MAX_BATCHES = 10`, so at most
  `MAX_BATCHES * MAX_LATEST_LEN + MAX_LATEST_LEN` = 110 entries a batch; `prepend_child_with_single_parent` (89-104)
  moves a full latest list into a new batch and drops the oldest past ten (`pop_back`). A merge's batch flattens its
  parents' (`fastlog_impl.rs`, `create_merged_list`). The batches spare the blobstore a round trip a unode.

## 4. The design, and the bound

- **The value.** A version's value names its map of paths to contents (A-647's path map, its value for the version)
  and a history tree, a path map whose files are each path's node, the node of `p` kept at `p/\0` and the root's at
  `\0` (a NUL is no name in git, Mercurial, Subversion or POSIX, and the type refuses one). A file and a directory once
  at one path therefore share one history, as in git.
- **The rule.** A version keeps a path's node from the first parent the path is TREESAME to (none when that parent
  had none), else makes a node naming the parents' nodes in parent order, each once. A path is TREESAME when it holds
  nothing in both, the same file (class and content), or a directory of the same files: the directory's trie root in
  the path map, a function of its files alone (A-647 part 1). By induction over the history the nodes reachable from
  a path's node are git's default-mode list: a version TREESAME to a parent has the first such parent's node and its
  list; a version TREESAME to none lists itself and every parent's list; a root lists itself when the path stands. A
  removal is a node of no file, so the history before it stays reachable and the removal is listed, as git lists it.
- **What differs from Mononoke, and why.**
  - A directory is compared by its files, not by its children's unodes. Where a merge takes one side's `d/x` and the
    other side's `d` whole, git lists `d`'s history through the second parent and `d/x`'s through the first; v2's
    subentries differ from both parents' there, so Mononoke would make a tree unode for the merge and list it. The
    test `t_a666_p2_a_directory_and_a_file_beneath_it_follow_their_own_parents` holds the type to git.
  - Removals are nodes of the one tree, not a second manifest: one lookup answers a path that stands or one removed,
    and the merge rule is git's for both.
  - A source (a rename's or a copy's) is part of the node: it is what the work declared, never revised, and a value
    never changes (A-647's `Diverged`).
  - No batches: the store is in memory, so each node is one read. Batching is owed when values travel between tiers.
- **Copies from older versions.** A copy names any earlier version (Subversion's copy-from revision). A derivation
  reads that version's value, so the mechanism lets a type name inputs beyond its parents (`DerivedType::inputs`), and
  a fill derives them first. A source must be older than the version (its generation less), stand as the same kind as
  the destination, and the destination must stand; otherwise `BadSource`.
- **The question's cost.** `log` looks the path's node up in the history tree, then lists nodes newest first (the
  greatest generation, then the version's name, then the path), putting each listed node's parents and, with
  `--follow`, its source on the frontier, each node once a path. A full answer reads the value, the trie nodes on the
  path's key (one a name while each directory fits a leaf; at most the trie's depth a name otherwise), and at most one
  node for each version it lists. One cut at a limit of n reads at most 2 + n(1 + P), P the most parents a node has.
  Neither depends on the history's length.
- **Measured** (Windows x86_64, jupiter, 2026-11-19; the macOS run owed): a line of 20,000 versions answers `log hot`
  (1,000 changes) in 1,001 reads where git's walk visits all 20,000 versions, `log cold` (6) in 7, and `log hot` cut at
  10 in 12. A mutation that lists a path's whole history and then cuts it fails the bound: 122 reads against 10 for a
  cut question on a file of 121 changes.

## 5. Rejected

- **Walking the versions**, git's own way: a question costs the history's length. The oracle's model is that walk;
  136 of its 770 answers cost it four times the type's reads or more, and the long history twenty times.
- **Inferring renames by content**: banned (§4.16; D-27).
- **Mononoke's tree-unode reuse by subentries**: compares history, not content, and lists merges git does not (§4).
- **A deleted manifest beside the unodes**: two structures and two merge rules for one question (§4).
- **Copy-from outside the node**: makes a value mutable, which A-647 refuses (§4).
