# Ancestry queries by API: is-ancestor, merge bases and bisect's midpoint from derived data (A-666)

Status: read 2026-11-19 by g302 for A-666 (UI goal 93; A-645 "Queries by API", part 1), the evidence behind the
`ancestry` type of derived data (`crates/vcs/src/derived/ancestry.rs`). Tiers as in the design: A a paper, B a standard
or the vendor's own documentation, C deployed code, D a blog (flagged). Every quotation was read on 2026-11-19 from the
revision or copy named, at the lines given; the copies' sha256 (first 16 hex) are in §1.

## 1. Sources and revisions

| Source | Revision or copy read | sha256 (first 16 hex) |
|---|---|---|
| git `Documentation/technical/commit-graph.adoc`, `Documentation/technical/paint-down-to-common.adoc`, `commit-reach.c`, `bisect.c` | git `master` at `e1ec09b23c8e0deb21a0b95ac5996422277395d4` | bf1e8aeeec83a106, b6004a06405d1a8d, c20df82d8d94e813, 825941711864c673 |
| Mononoke's commit graph: `eden/mononoke/repo_attributes/commit_graph/commit_graph_types/src/edges.rs`, `commit_graph/src/{core,frontier,lib}.rs` | facebook/sapling `main` at `573994010efaffe5a2b4a9fd2a87499ed7caaaf3` | c11bc9ad4f102b9b, d5c7c162f5492e95, 277f7dbd8e0e2640, 02eb56e6fd2d6d33 |
| Sapling's segmented changelog: `eden/scm/lib/dag/src/{lib,iddag}.rs` | the same revision | db1bc30e2f426bc1, e7be535b7799a40f |
| E. W. Myers, "An applicative random-access stack", Information Processing Letters 17(5):241-248, 1983 | the scan at `publications.mpi-cbg.de/Myers_1983_6328.pdf` | 6c089258f3f56fd6 |
| M. Pǎtraşcu, "Unifying the landscape of cell-probe lower bounds", SIAM J. Comput. 40(3), 2011 | arXiv 1010.3783v1 | 4be32e9522231e70 |
| H. Yildirim, V. Chaoji, M. J. Zaki, "GRAIL: Scalable Reachability Index for Large Graphs", PVLDB 3(1):276-284, 2010; Yildirim's thesis, RPI, 2011 | the thesis abstract (`cs.rpi.edu/research/phdabstracts/yildih2.html`) | (web page) |
| S. Seufert, A. Anand, S. Bedathur, G. Weikum, "FERRARI: Flexible and Efficient Reachability Range Assignment for Graph Indexing", ICDE 2013:1009-1020 | arXiv 1211.3375's abstract | (web page) |
| E. Cohen, E. Halperin, H. Kaplan, U. Zwick, "Reachability and Distance Queries via 2-Hop Labels", SIAM J. Comput. 32(5):1338-1355, 2003 | the abstract at `cris.tau.ac.il` | (web page) |

## 2. What the server must answer, in each tool's words

- **Merge bases** [C: `paint-down-to-common.adoc` 14-15]: "A merge base is a common ancestor that is not itself an
  ancestor of another common ancestor." `git merge-base --all` prints every one; without `--all` git prints the first
  its walk finds, "necessarily the highest-generation common ancestor" (same file, "Single result"). Sapling's
  `gca_all` is the same set [C: `iddag.rs` 1314-1318: `self.heads_ancestors(self.common_ancestors(set)?)`]. Mononoke's
  `common_base` answers only "all of the highest generation changesets that are ancestors of both u and v" [C:
  `lib.rs` 771-772], a subset of git's set when merges cross (criss-cross merges have bases of several generations).
- **Is-ancestor** [C: `paint-down-to-common.adoc` 18-19]: "is commit A an ancestor of commit B?", inclusive; Mononoke:
  "Ancestry is inclusive: a commit is its own ancestor" [C: `lib.rs` 518].
- **Bisect's midpoint** [C: `bisect.c` 269-270]: a candidate's weight "is the number of interesting commits it can
  reach, including itself", the candidates being the bad commit's ancestors that no good commit reaches; the best is the
  one maximizing `min(weight, nr - weight)` [C: `bisect.c` 187-207, `best_bisection`], ties going to the first in
  rev-list order (`distance > best_distance`, strict). git also stops at a weight "within ~0.1% of the halfway point"
  (`approx_halfway`, 110-139), and counts a merge's weight by a whole walk: "if you have more than one parents, you
  cannot just add their distance ... So we will first count distance of merges the usual way" (325-331,
  `count_distance` 45-69), O(candidates) per merge.

## 3. The bounds the literature proves

- **Generation numbers** [B: `commit-graph.adoc` 59-79]: "A commit with no parents (a root commit) has topological
  level of one. A commit with at least one parent has topological level one more than the largest topological level
  among its parents." The property: "If A and B are commits with generation numbers N and M, respectively, and N <= M,
  then A cannot reach B." And the walk: "when checking if A is an ancestor of B, then we only need to walk commits until
  all commits on the walk boundary have generation number at most N." Corrected commit dates (v2, 46-57) need commit
  dates, which a history in general does not have; the same property "applies to both versions". A generation number is
  a pure function of the parents' numbers. It prunes a walk; it bounds none: an answer may still walk every commit
  above the target's generation.
- **Skew-binary jump pointers** [A: Myers 1983]. A singly linked list "augmented with auxiliary pointers that permit the
  access of arbitrary stack elements in O(lg N) time" (p. 241). PUSH sets the new cell's pointer from its parent `s`
  and the parent's pointer `t = s.JMP` alone: "If s.LEN - t.LEN = t.LEN - t.JMP.LEN Then t <- t.JMP Else t <- s"
  (p. 244, lines 1-5), O(1) time and space. FIND: "While s.LEN != k Do If s.JMP.LEN < k Then s <- s.NXT Else
  s <- s.JMP". Corollary 5 (p. 247): "the search loop of FIND(S, k) is repeated at most 3⌊lg(N + 1)⌋ - 2 times", tight
  ("exactly met by FIND(S, k) whenever LENGTH(S) = 2^k - k"), where Lemma 1's ⌊lg(A + 1)⌋ is a skew-binary number's
  length. The pointer's target depth is a function of the depth alone (J, p. 243: "subtracting one from the lowest
  non-zero digit in the CSB representation"), so two cells at one depth jump to one depth: the property the lowest
  common ancestor below rests on. A child's pointer reads its parent's pointer's pointer: the value a version keeps
  must reach that far, which a derived node naming its targets' values does (A-666's decision).
- **Mononoke's commit graph is these, deployed at Meta** [C: `edges.rs` 23-41, 133-172]. Each changeset keeps its
  generation ("the inclusive number of commits between this commit and the farthest root commit. Root commits have a
  generation of 1"), its parents, `merge_ancestor_or_root` ("the most recent ancestor that is a merge or root"), a
  p1-linear skew ancestor ("a skew-binary search tree over the linear first-parent history"), and the skip tree:
  `skip_tree_parent`, "the most recent single common ancestor of this commit's parents", with its own skew ancestor. The
  writer computes the skip-tree parent as "the skip tree lowest common ancestor of all parents" [C: `core.rs` 79-86] and
  every skew ancestor by Myers' rule [C: `core.rs` 176-214: `parent.skip_tree_depth - parent_skew_ancestor.depth ==
  parent_skew_ancestor.depth - parent_second_skew_ancestor.depth`]. `is_ancestor` lowers the descendant's frontier to
  the ancestor's generation [C: `lib.rs` 519-531], and a lowering step replaces a changeset by its lowest skip-tree
  edge still at or above the target, else by its parents [C: `frontier.rs` 120-160].
- **Why a skip-tree jump is safe** (derived here; checked by the oracle): the skip-tree parent of a merge is a
  dominator. By induction over the history, every ancestor of a version is an ancestor of its skip-tree parent or has a
  greater generation than it (true for a single parent; for a merge, true of each parent's skip-tree chain down to their
  common skip-tree ancestor). So a frontier lowered to generation g may replace a version by any skip-tree ancestor of
  generation at least g, and lose nothing at or below g. The test holds every value's skip-tree parent to the immediate
  dominator computed by its definition (the intersection of the parents' dominator sets).
- **No constant-time answer exists in general** [A: Pǎtraşcu 2011, Theorem 2]: "A reachability oracle using space S in
  the cell probe model with w-bit cells, requires query time Ω(lg n / lg(Sw/n))", which "holds even if the graph is a
  subgraph of a butterfly graph"; "If constant time is desired, our bounds shows that the space needs to be n^{1+Ω(1)}."
  A butterfly of degree two has in-degree two (§2 there): a history made of two-parent merges. So an index of linear
  size answers is-ancestor in Ω(lg n / lg lg n) at best on such histories, and the merge-base question, which decides
  ancestry, is no easier. The bound A-666 states is therefore in two parts: logarithmic along first-parent and
  skip-tree chains, and linear in the merges a question must open.
- **The general reachability indexes need a global order** (rejected below):
  - GRAIL "is based on the idea of randomized interval labeling", with "linear indexing time and space, and the query
    time ranges from constant time to being linear in the graph order and size" [A: Yildirim's thesis abstract]; its
    intervals are ranks of traversals of the whole graph.
  - FERRARI assigns "exact and approximate reachability intervals" under "an explicit bound on the size of the index",
    and answers with "a guided online search procedure" [A: arXiv 1211.3375's abstract].
  - 2-hop labels are "based on 2-hop covers of the shortest paths, or of all paths, in a graph", found by "an efficient
    algorithm for finding an almost optimal 2-hop cover of a given collection of paths" [A: Cohen et al.'s abstract].
- **Segmented changelog** [C: `iddag.rs` 53-61]: "Structure to store a DAG of integers ... on a topo-sorted integer
  DAG"; a flat segment requires "for any integer x in low+1..=high range, x's parents must be x - 1". Its ancestry
  answers are set algebra over id ranges (`is_ancestor`: `self.ancestors(descendant_id.into())?.contains(ancestor_id)`,
  1351-1354), fast because ids are numbered in the order commits were added.

## 4. What A-666 chose, and what it rejected

- **Chosen: Mononoke's commit-graph fields as one A-647 type.** A version's value is a derived node naming its
  parents' values, its first-parent skew jump, its run bottom (the nearest merge or root on its first-parent chain),
  and its skip-tree parent and skip-tree skew jump, with its generation (topological level), its first-parent depth, its
  skip-tree depth and its name. Each is computed from the parents' values and what they name (Myers' rule reads the
  parent's jump's jump through the parent's value), so the value is pure and the same on every tier; a derivation reads
  O(1) nodes for a version of one parent and O(k lg n) for a merge of k parents (the skip-tree lowest common ancestor).
- **Rejected: generation numbers alone** (git's walk): pruning, no bound; an answer walks every version above the
  target's generation, so the cost grows with history.
- **Rejected: GRAIL, FERRARI, 2-hop and pruned labelings.** Each label is a function of the whole graph (a traversal's
  ranks, an interval assignment under a budget, a greedy cover), not of a version and its parents: two tiers that
  received one history in different orders would hold different labels, which A-647's index refuses as `Diverged`, and
  a new version would relabel old ones. FERRARI's size budget is also a tunable no deployment could learn identically on
  every tier. Their positive answers still fall back to a search with GRAIL's linear worst case.
- **Rejected for the generic type: segmented changelog's ids.** An id is a position in the order commits were added,
  not a function of a commit and its parents, so it cannot be an A-647 value. It stays the right tool where one green's
  versions are numbered by the green itself (A-575's lineage), noted for a later part.
- **Rejected: a persistent set of every ancestor per version** (A-614's reached sets as the index). Exact at O(lg n) a
  question, but a version keeps O(lg n) new trie nodes and a merge of a long-lived branch pays for every version on
  both sides since the fork: O(n lg n) space and a derivation that grows with history.
- **Rejected: Mononoke's `common_base` as the merge-base answer**: it returns only the highest-generation common
  ancestors, not git's whole set.
- **The queries** (each exact, each held to a naive walk by the oracle):
  - is-ancestor: Mononoke's lowering over the skip tree. Reads are at most a constant times (1 + P) times lg n, where P
    counts the parents of the merges whose skip-tree parent lies below the ancestor's generation: the branches open
    across it. A linear history is O(lg n).
  - merge bases: git's paint-down (sides one and two, stale, both of git's early exits: only stale entries left, and
    one side exhausted with no pending candidate), run over runs instead of commits. Entries are grouped by run
    bottom; each run's entries become one virtual tree over the first-parent tree (sorted in a depth-first order
    decided by skew-jump lowest common ancestors, joined at the lowest common ancestors of neighbours), so chains meet
    without walking; a merge's paint jumps along the skip tree past every region no other entry reaches (the region a
    dominator closes). Reads are O(E lg E lg n) for E entries, E at most two plus the parents of the merges opened.
  - bisect's midpoint: the same paint with bad and good, every run of candidates a segment whose weight grows by one a
    version, merges' weights from the union of their parents' segments (bitsets over segments), the exact best by
    `min(weight, count - weight)`, then the greater generation, then the lesser name. Reads are O(E lg E lg n), with
    K² bits for K candidate segments; a linear range is O(lg n).
- **Owed** (named so they are not forgotten): corrected commit dates where a history has dates (git's store has them);
  a green's own versions numbered as segmented changelog numbers them; questions cut into slices for the shard's loop;
  path history and blame (later parts, A-645).
