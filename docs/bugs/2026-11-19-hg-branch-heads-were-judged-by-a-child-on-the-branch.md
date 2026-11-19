# Mercurial's branch heads were judged by a child on the branch (2026-11-19, A-667)

## Description

Found by g304 against stock Mercurial 7.2 on the Mac Studio while writing A-657; fixed by g300 (UI goal 93).
`slates-hg`'s `serve::discovery::branch_head_revs` took a revision for its branch's head when no child of it was on its
branch. Stock 7.2 drops every revision that a later revision on its branch descends from, whatever branches the path
between them crosses. For `default` 0, `stable` 1 on 0 and `default` 2 on 1, `hg heads default` answers 2 alone;
slates answered 0 and 2, in `branchmap` and in `check:updated-heads`.

## The rule, from 7.2's source

Read from Homebrew's Mercurial 7.2 (`mercurial/__version__.py`: `7.2`), on 2026-11-19:

- `branchmap.py` lines 422-424 (`_BaseBranchCache._process_new`): "A branchhead is a revision such that no descendant
  is on the same branch." Each branch keeps the parents from other branches of its revisions (`uncertain`, lines
  466-483) and drops their ancestors from its heads (`cl.ancestors(uncertain, floorrev)`, lines 487-494), skipping the
  walk where every head is a topological one or no such parent lies above the lowest head.
- Closed heads stay in the cache (`_closednodes`, line 304). `branchheads(branch, closed=False)` (lines 316-325) leaves
  them out only when asked; the wire's `branchmap` asks `closed=True` (`wireprotov1server.py` lines 258-267), and
  `check:updated-heads` reads every head, closed ones too (`bundle2_part_handlers.py` lines 303-327, calling
  `all_nodes_are_heads`, `branchmap.py` lines 888-894).
- Obsolete changesets are never heads (`update`, lines 351-358), and an obsolete parent is walked as another branch's
  (lines 469-476).
- 7.2's two cache formats run this one `_process_new`: `BranchCacheV3`'s (line 1030 on) clears its own data and calls
  it, and V2 is the default (`experimental.branch-cache-v3`).

## Root cause

A-619 wrote the rule as "no child on its branch" and held it to an oracle that stated the same rule
(`crates/hg/src/serve/tests.rs`, `oracle_branchmap`), so the generated test certified the implementation's rule rather
than 7.2's: the CLAUDE.md gotcha about a model that states the implementation's rule. A-641's push checks took their
branch heads from the same function and stated the same rule in their own oracle (`crates/hg/src/push/tests.rs`,
`oracle_updated`), found by the fix's first green run, where it alone failed.

## Impact

Every repository where a branch is left and come back to (a stable branch merged forward, a feature branched off and
back) answered a head 7.2 does not have. A stock client's `branchmap` read it as a head of the remote, which
`hg push`'s new-head check (`discovery.checkheads`) counts, and `check:updated-heads` held such a node to be a branch's
head where 7.2 refuses it as raced. What a client did with the extra head was not measured; the answers themselves were
(below).

## Edits

- `crates/hg/src/serve/discovery.rs`: `branch_head_revs` by 7.2's rule. One walk of the parents finds the revisions
  continued on their branch, the revisions with any child, and each branch's parents from other branches (`Edges`).
  A revision continued on its branch is no head; one with no child at all is a head. A branch whose remaining
  candidates have children is walked from its highest parent from another branch at or above its lowest such
  candidate down to that candidate (`end_reached`, `mark_ancestors`), as 7.2 walks `uncertain`. Every set and list is
  charged to the request's admission first. `branchmap`'s doc states the rule.
- `crates/hg/src/serve/mod.rs`: `RevSet::remove`, so the walk's set is left empty after each branch.
- `crates/hg/src/serve/tests.rs`: `oracle_branchmap` states 7.2's rule: no later revision on the branch has the
  revision as an ancestor.
- `crates/hg/src/push/tests.rs`: `oracle_updated` (`check:updated-heads`) states the same rule (`descends`, every path
  walked).
- `crates/hg/src/serve/discovery_tests.rs` (new): g304's history; the two histories below; a closed head kept and
  ending a head through it; a generated oracle over histories moving between two branches with merges of any earlier
  revisions; the oracle's non-vacuity, histories from fixed seeds where the old rule and 7.2's differ.

## Evidence

Windows x86_64 (jupiter), 2026-11-19, kiln, debug, rustc 1.98.1, each run in a clone and target of its own (the Mac
Studio was offline): `cargo test --locked -p slates-hg --lib`. Red, the tests alone against the old rule
(`agents/g300-a667-branch-heads-red`, 5c88fd3e9): 276 passed, 6 failed, the five new tests and A-619's corrected
oracle, each answering the old rule's extra heads (g304's history `[0, 1, 2]` where 7.2's heads are `[1, 2]`). First
green (e4bd2f3a1): 283 of 284, A-641's push-check oracle failing alone. Green (87690ec64): 284 of 284. The macOS run is
owed.

## Apart from 7.2, on purpose

7.2's walk takes a shortcut where one head is known and one parent is on the branch (`branchmap.py` lines 478-483:
`if not (len(bheadset) == len(samebranch) == 1)`): it never looks at the other parent. When the parent on the branch is
an ancestor of the known head, a merge reaching that head through another branch leaves it a head. And whether the
shortcut fires depends on what the cache already held, so 7.2's answer for one repository depends on how its cache was
updated. Measured with Homebrew's 7.2 on 2026-11-19 (`HGRCPATH=/dev/null HGPLAIN=1`, scratch repositories under
`~/.cache/g300-scratch/hg-heads`, parents set with `hg debugsetparents`):

| History | `hg heads default` (7.2) | the rule |
|---|---|---|
| `default` 0, `stable` 1 on 0, `default` 2 on 1 | 2 | 2 |
| `default` 0, `default` 1 on 0, `other` 2 on 1, `default` 3 on 2 and 0 | 3 1 (cache kept or computed whole) | 3 |
| `default` 0, `other` 1 on 0, `default` 2 on 1, `other` 3 on 2, `default` 4 on 3 and 0 | 4 2 kept commit by commit; 4 computed whole (`.hg/cache/branch2*` removed) | 4 |

A server cannot answer as one cache's history did, so slates answers the rule, which 7.2's own comment states and which
7.2 answers wherever its shortcut does not fire. `hg merge` refuses to make such a merge ("merging with a working
directory ancestor has no effect"); a pull, an import or `hg debugsetparents` can.

## Siblings swept

| Where | What 7.2 judges there | Here |
|---|---|---|
| `branchmap` (`serve::discovery`) | branch heads, closed ones kept | this bug, fixed |
| `check:updated-heads` (`push::check::updated_heads`) | the same heads (`all_nodes_are_heads`) | read `branch_head_revs`, fixed with it; its test oracle stated the old rule too, corrected |
| `check:heads` (`push::check::all_heads`) | the repository's topological heads (`repo.heads()`) | `head_revs`, the same rule; not a sibling |
| `reject_new_heads` (`push::reply::multiple_heads`'s caller) | `head() and not closed() and branch(B)`: the open topological heads on the branch (`hgext/hooklib/reject_new_heads.py`) | no caller yet: the landing that decides it is owed, and must count those, not branch heads |
| the push's head count (`push::reply::heads_changed`'s caller) | topological heads added and removed, the added ones that close a branch apart (`changegroup.py` lines 879-886) | no caller yet: owed with the landing |
| the push classifier (§4.17.3 piece 5, decisions 1 and 2) | branch heads | not built |

## Owed

An obsolete changeset the served view shows (an ancestor of a visible orphan) is never a head in 7.2 (`branchmap.py`
lines 351-358), and its ancestors are ended through it (lines 469-476). The served view (`serve::Repository`) carries no
obsolescence yet; A-649 part 2's marker rows are what it will be read from.
