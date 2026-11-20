# g261-git-mercurial-subversion progress (rewritten 2026-11-18 ~23:40 UTC from the agent's context after the wipe)

Role: Claude subagent (implementer) of the slates supervisor ("main").

## Goal now: UI 91 (kiln 51cef9f7dd5985fda2de4235a5f32653): slates as INDUSTRIAL-GRADE git, Mercurial and Subversion servers
Ada's words via main: built on the operations-based merge (greens, works, accept/identical/conflict); everything such
servers need (branches, rebasing, stacked changes, PR trees and review, protected branches, hooks, pack/bundle/delta
transfer, shallow and partial clone, large files, access control); stock git/hg/svn clients over their own protocols;
ZERO arbitrary constants (every tunable learned at runtime from the deployment's own data, never one machine's
measurement frozen); at least an order of magnitude faster than the competitors, measured and recorded.
Split (main, 2026-11-18): g261 = git server; g291 = Mercurial; g292 = Subversion; all three on UI 91.

### Piece A-497 (assigned to g261 in amendments.tsv): the design
1. FIRST landing: the SHARED mapping onto greens and works, its own subsection the other two build on (branch = green;
   push = work submitted, verdict = push answer; rebase/stack = chains of works; PR tree = works over works; object
   storage deduplicated by content; refs and history = the green chain).
2. SECOND landing: the git server's design (protocol v2 over smart HTTP and SSH; pack, index, commit-graph; fetch
   negotiation; shallow and partial clone; push/receive-pack with hooks; protected branches; LFS-style large files;
   access control; every tunable learned; the 10x target against git-http-backend and Gitaly on the Linux kernel tree).
Primary sources only; reading notes in docs/wip/research/. No code unless a pure core provable at once.

### State
- Worktree ~/slates-work/g261-a497, branch agents/g261-a497-vcs-servers-design (from origin/main 5ceda4847).
- Sources downloaded (outside the repo) in ~/.local/state/g261-sources/: git Documentation at git master e1ec09b23c8e
  (gitprotocol-v2/-pack/-http/-capabilities/-common, gitformat-pack/-commit-graph/-index, githooks, git-receive-pack,
  git-upload-pack, git-http-backend; technical/partial-clone, shallow, bitmap-format, hash-function-transition,
  packfile-uri, bundle-uri, multi-pack-index, pack-heuristics, long-running-process-protocol); Mercurial 7.2.4 sdist
  help texts (sha256 85839e0f…71a2; repo.mercurial-scm.org and heptapod sit behind a bot check); Subversion trunk
  fd4e82b00cbe (libsvn_ra_svn/protocol, svn_fs.h, svn_delta.h, http-protocol-v2.txt).
- Rule: ONE piece; any code's --lib tests, cargo fmt, one-crate clippy, cargo xtask check (on the Studio, never this
  Mac); merge, read diff stat, push as its own command; END with a few-line report.

## Earlier goal (#71, CLOSED 2026-11-18): git, Mercurial, Subversion clients on slates volumes, every route
Landed on main: A-328 harness (9d530845, 7479d02c via 10d4a2e4f); A-327 correction (MSYS2_ARG_CONV_EXCL=^/; same
landing); A-350 (9127be60d, 930064322 via b4089697c, b3d72df70); A-368 + A-369 (1f733b895 via cf6d2f6e1, 79d6a2cb5,
5f5ff95ac, 567c521d1; pushed 61763ff41..567c521d1). Windows mounts need MountUseMountmgrFromFSD=1 (one-time admin).
Temporary GitHub branches of mine to delete (main's call): agents/g261-a369-tmp-1/2/3, agents/g261-matrix-1,
diag-g261-a369-1/2/3, diag-g261-owner-1, diag-g261-routes-1, agents/g261-git-mercurial-subversion-workloads.

## 2026-11-18 ~00:30 UTC (11-19): A-497 part 1 LANDED: §4.17 + §4.17.1 shared mapping, D-28, research/vcs-servers.md
- Commit 262dc8a45; merges 10af7200b and the second merge; pushed 485a0b6bf..a157b075c (main).
- Checks (Studio, kiln CLI, clone of agents/g261-a497-tmp-1 @ 10af7200b): cargo fmt clean; cargo xtask check exit 1 on
  6 R3 literals in files not mine (crates/anchor/src/handles.rs:69; crates/base/src/s3/list.rs:35,388,478,483,484) —
  reported to main. Docs-only change; no crate touched, so no clippy.
- Told main (SendMessage). Next: §4.17.2 git server design (A-497 part 2), same worktree/branch.
- Temporary branch: agents/g261-a497-tmp-1 (delete when convenient).

## A-497 part 2 LANDED (2026-11-18 PST): §4.17.2 the git server design
- Commit 4d2176fce; merge 5aec427cb; pushed 14bf90448..5aec427cb (main). Docs only: GAPS §4.17 entry, design §4.17.2
  (+ §4.17 status, §4.17.1 method's log/blame correction, A-497 second-part bullets), research/vcs-servers.md §5.
- Checks: none compiled, per main's correction of 2026-11-18 ("Why are ANY of these compiling?"): a docs-only piece
  runs nothing (no Rust changed, so no --lib tests and no fmt). No heavier run filed: nothing in this piece needs one.
- Ledger: A-497 landed (262dc8a45/a157b075c + 4d2176fce/5aec427cb).
- What is left (main starts the next piece): §4.17.1's seven shared pieces, then §4.17.2's fifteen git pieces, starting
  with piece 1, the pkt-line codec (pure core; golden vectors from gitprotocol-common; hostile inputs).
- Open for main: cargo xtask check on main failed (2026-11-18 ~00:20 UTC run) on 6 R3 literals in files not mine
  (crates/anchor/src/handles.rs:69; crates/base/src/s3/list.rs:35, 388, 478, 483, 484).
- Temporary branch: agents/g261-a497-tmp-1. Worktree ~/slates-work/g261-a497 (branch
  agents/g261-a497-vcs-servers-design) can be removed once main confirms.

## A-515 LANDED (2026-11-19 ~01:40 UTC): §4.17.1 shared piece 1's first half, the repository record and namespace
- Worktree ~/slates-work/g261-a515, branch agents/g261-a515-repository-record. Commits d0779121b (code + docs), merges
  e16133c66 and the second, pushed 1d2723e51..7d4b15158; then f46b35bdc (docs: §4.17.1's Subversion answer points to
  §4.17.4 item 3, g292's ask), pushed 7d4b15158..f46b35bdc.
- Built: crates/db/src/repository.rs (Repositories: check/apply/restore, model test), catalog RepositoryRecord,
  BranchRecord, RepositoryId, VcsTool, ObjectFormat, VolumeId::partition; Op 83-87 (after main's 81-82); DbError
  NotGreen/InvalidName (+ server mapping); snapshot fields repositories, repository_branches; wire_order hand-published.
- Tests (Studio): slates-db --lib 109/109; slates-server --lib error 3/3; fmt clean; two red-checks.
- Filed in run-requests.md: wire_order test, clippy db+server, xtask check, slates-db integration + recovery oracle.
- g291 answered (asks 1-4 + hook question) in design + their inbox; g292's proposal done + their inbox.
- Owed (main starts next): piece 1's second half (a green at another green's version without a walk: persistent
  engine maps); repository verbs; fleet replication of the record (with piece 2).
- Temporary branch: agents/g261-a515-tmp-1.

## A-534 (started 2026-11-19 ~02:00 UTC): shared piece 1's second half -- a green made at another green's version without a walk
Worktree ~/slates-work/g261-a534, branch agents/g261-a534-green-at-version (from origin/main ff94f8848).
Requirements (main): persistent maps in the merge engine; O(1) or O(changed paths) per branch, bound stated in the
design; oracle vs a copying reference (every branch made both ways reads the same at every version); a non-vacuity
counter proving no tree walk ran; failing first. Only `cargo test -p slates-merge --lib <filter>` + fmt, on the Studio.
Design decided (why: R2 forbids Rc/Arc, so sharing needs one owner of the nodes -- an arena -- lent to each green):
1. `crates/merge/src/pmap.rs`: persistent AVL map; nodes in an arena `Nodes<K,V>` with a refcount each; a node with one
   holder is mutated in place (so an unforked green costs what a BTreeMap did), a shared one is copied on the path
   (O(log n)); fork = root refcount +1 (O(1)); hooks on value copy/free (content blocks need refcounts).
2. Green's 8 current-value maps (content, specials, dirs, modes, symlinks, hardlinks, xattrs, links_to) become Pmaps;
   a `nodes` field holds the arenas; content blocks get an extra-holder count (release only at the last holder).
3. A branch's histories start empty: a key with no history has held its current value since version 0 (lazy origin
   entry recorded at the key's first change); every history read falls back to the current value when a key has none
   (a no-op for every existing green, where every key has history).
4. `Lineage` (store swap): owns the shared store (chunks + nodes + block counts); `branch(from, version)` forks the
   source's roots and reverts the keys changed in (version, head] from the source's histories (O(changed paths));
   identity at the version = head identity with those keys' entries reverted (LtHash add/remove).
5. Replicas refused while branched (and branch refused while replicas registered); checkpoints of a branch owed.
Copying reference for the oracle: Green::with_origin(origin built from base_at(V) + content_at).
- 2026-11-19 ~03:30 UTC: switched the structure to a persistent RADIX TREE over key bytes (g292 asked: its Subversion
  Copy needs a shared subtree; a byte radix tree keeps BTreeMap<String> byte order AND makes a prefix one node).
  `crates/merge/src/ptrie.rs` built and green on the Studio: 3 tests (oracle over forks+grafts vs BTreeMap copies, 400
  cases; a fork makes no node and a change copies <= key length nodes; a graft of 10k keys makes <= 4 nodes, walks none).
  API: Nodes<V> arena (copied/visited/missing counters, live), Ptrie<V> {get, contains_key, insert, remove, fork,
  release, iter, prefix, subtree -> Shared, graft, drop_subtree, copy_into, len, is_empty}, Holders<V> {copied, freed},
  Plain. Told g292 (they build Copy on subtree+graft). Temp branch agents/g261-a534-tmp-1 (wip commits).
  NEXT: Green's 8 maps -> a `Dim<V>` wrapper (trie + its own `Nodes`, swappable), block hold counts, lazy history
  seeding + "no history => current" reads, Lineage (store swap) + branch with reverts + identity at V, tests.

## 2026-11-19 (g261 restarted after a usage limit): A-534 continues
- Predecessor's state: ~/slates-work/g261-a534 (branch agents/g261-a534-green-at-version) held 4 wip commits (ptrie.rs +
  lib.rs, nothing landed on main from A-534). Merged origin/main a613562eb in cleanly (5bac3336c).
- origin/main meanwhile removed replicas and store.rs (A-528), so the plan's "replicas refused while branched" and
  "store swap of store.rs" are moot. g292's A-542 (node-exclusive verdict) landed without touching the state maps.
- Plan (one landing): (1) `crates/merge/src/dims.rs`: Dim<V> (trie root + its arena), Files (content: blocks with
  holder counts, so a path copy is O(1) a node), Xattrs (escaped path + NUL NUL + name keys, BTreeMap order kept),
  Arenas; (2) Green's 8 current maps become these; readers yield owned keys (files, files_at, file_lens_at -> String;
  server work_files.rs follows); (3) ONE history rule for every green: a key with no history has held its current value
  since version 0; its first change seeds (0, old) when it had a value; with_origin writes no version-0 history and no
  zero last-changed entries (saves the origin's second copy of every file); (4) Lineage: shared arenas + greens by
  generational handle; branch(from, V) forks the 8 roots and reverts the paths changed in (V, head] from the source's
  histories, identity at V by the set hash; O(changed paths), refuses V below the fold; (5) oracle vs copying reference
  (with_origin of the source's state at V) over generated histories; non-vacuity: no node visited by a branch, reverted
  keys = changed paths. Red first: branch by copying (visits every node) fails the counter.
- 2026-11-19 (restart) built the rest of A-534 in ~/slates-work/g261-a534 (temp branch agents/g261-a534-tmp-2):
  dims.rs (Dim/Files+Blocks/Xattrs/Arenas), engine.rs (8 maps persistent; one history rule: value_at/seed/seed_content/
  union_keys/*_at readers/was_special/links_at; OriginLoad writes no version-0 history; files/files_at/file_lens_at give
  owned paths), engine/branch.rs (Lineage, LineageGreen, LineageRefused{Stale,PastHead,Folded}; branch_at reverts from
  the change index + histories, identity moved by the same keys), engine/branch/tests.rs (oracle vs copying reference,
  cost test 10k files, every-dimension test, isolation, refusals); server work_files/listing/checkout/verbs/work_charge
  follow owned paths. Docs: §4.16 new subsection (bound O(E + c(L + log n) + B)), §4.17.1 status + item 4 + piece 1,
  GAPS §4.17 entry, amendment A-534.
- Evidence (Studio, kiln r107/r113/r126): slates-merge --lib 22/22; red-checks on copies: no reverts / a walk / no
  version-0 seed each turn 3 of 5 branch tests red; cargo fmt --all clean (local pinned rustfmt matched the Studio's
  output by shasum); slates-server --lib (work_charge, checkout::, listing::) 6/6 compiled and passed.
- Seen in passing: server test build warns unused import slates_rt::TaskId (deadlines.rs:23) and unused const
  CONTROL_BROKEN (reap.rs:35) on main -- not mine.
- A-534 LANDED 2026-11-19: 53c5c77f7 + merge f8ba01f61, pushed b1549a81e..f8ba01f61 (main). One conflict at the merge,
  the amendment log's end (main's A-556..A-566 first, A-534 last). Run requests filed (merge integration suites, shuttle,
  server green/recovery suites, clippy/xtask/doc on three platforms, A/B of verdict_bench and the kernel/ rung).
  Temporary branches to delete (main's call): agents/g261-a534-tmp-1, agents/g261-a534-tmp-2.
  Studio scratch: ~/.cache/g261-a534 (src + target) -- deleted at the end of this run.
  What is left of piece 1 (main starts it): the server's repository verbs over a Lineage on the owner shard (branch
  through the register with piece 2, a branch recovered over its source at v); a retire's release sliced under the
  shard's budget; a green at a commit that is no version's head (piece 3's trees). g292's Copy builds on dims.rs:
  add `subtree`/`graft` wrappers on Dim/Files/Xattrs (xattr keys: escaped path, NUL NUL, name; a path prefix ending in
  '/' is a key prefix) and a per-prefix appearance record (a grafted key has no history and must not read as present
  before the copy; the change index and last_changed need per-prefix entries too, or the copy walks the subtree).
- Studio scratch ~/.cache/g261-a534 (src + target) NOT deleted: kiln answered "not connected to the kiln server" twice
  at the end (2026-11-19). Delete it at the next kiln contact (`rm -rf ~/.cache/g261-a534` on d1323898).
- Reported to main (SendMessage) with the g292 hand-off. A-534 done; piece 1's remaining parts are main's to start.

## A-575 (started 2026-11-19): piece 1's rest -- the server's repository verbs over one Lineage on the owner shard
Worktree ~/slates-work/g261-a575, branch agents/g261-a575-repository-verbs (from main f8ba01f61). Brief (main): create a
repository, bind a branch, branch at a version through Lineage::branch, retire with its release sliced under the shard's
per-iteration budget; every verb a typed refusal at its bounds; recovery rebuilds the Lineage from log and snapshot,
checked by a model test as A-515's was; failing --lib tests first; only --lib + fmt on the Studio (new target dir
~/.cache/g261-a575 there; main deletes ~/.cache/g261-a534).
- A-575 design (2026-11-19): kiln down at the start, so code first.
  1. merge: ptrie `Releasing` + `Nodes::release_step` (one node a step; `release` loops it); Lineage `take`/`put_back`
     (an engine taken out holds the arenas; any other lend refused `Busy`), `Lent<L: DerefMut<Target=Lineage>>` guard
     (Deref/DerefMut to Green, Drop puts back), `create` (empty member), `retire` -> `Release` job + `release_step`
     (a node a step, `Busy` while a member is out).
  2. db: `MemberOrigin {Root, Branch{from, version}}`, `MemberRecord {repository, green, origin}`, `Op::RepositoryGreen
     Joined` (appended; wire_order hand-published); Repositories keeps members in join order per repository (snapshot
     field appended); rules: repository exists, green held+Green+repository's partition, not a member yet, one Root per
     repository, Branch.from a member of the same repository; a green some live member branched from cannot be
     destroyed (new DbError); `branch_floor(green)` = min branch version of its live branches (pins its checkpoint so
     recovery can branch at v); membership outlives a destroyed repository record (A-515 leaves the greens).
  3. ipc: RequestBody appended: CreateRepository{name, tool, format, default_branch, green: Option}, CreateBranch
     {repository, name, from, version: Option}, BindBranch{repository, name, green}, RetireBranch{repository, name}
     (+ wire enums for tool/format); replies reuse GreenCreated / Acknowledged; route by owner_of_name(repository).
  4. server: `Greens` (alone map + members + RefCell<Lineage> per repository; get/get_mut guards that Deref to Green,
     busy lends refused and counted); verbs in crates/server/src/repository.rs; a member's destroy retires it with its
     release sliced (image::sliced + defer, a node a unit); recovery rebuilds each repository's members in join order
     (root replayed then Lineage::new; a branch = source lent, branch_at(v), its own checkpoint+chain replayed seeded);
     reachable_floor includes branch_floor.
- 2026-11-19 (after the usage-limit reset): local wip commit on agents/g261-a575-repository-verbs holds steps 1-3 and the
  server's Greens + crates/server/src/repository.rs + merge_service.rs guard fixes + verbs.rs dispatch/routing. Left:
  verbs.rs call sites (edit/declare base reads scoped; 8472, 9239-9707 sites; retain; fence_green and the destroy path
  for members), checkout.rs/daemon.rs/listing.rs sites, recovery (chain_phase hands the engine back on a fence;
  GreenReplay seeded; rebuild_member_sliced; members sorted by join order; GreenFence::SourceUnavailable), server unit
  tests (failing first), Studio compile (kiln was down), docs, land.
- 2026-11-19 A-575 LANDED: d0a345667 (the change, with wip 82ee2daf6..57382c0fe), merge 3497e3692 (134 main commits;
  conflicts only at the ends of the amendment log and the GAPS §4.17 entry, main's first and A-575 last; no wire tag
  collided: Op 89, RequestBody 82-85 stay A-575's), 7e13fee30 (main's new A-542 copy tests, crates/merge/src/dims/
  copy/tests.rs `release_all`, called the whole-map `release` A-575 removed: now stepped), then merges 2827482cd, 0e41d39b9, d4c3d2989 (main moved three more times; one more amendment-log conflict, A-577 first), docs 7ce9be98f + 522100c4f; pushed main 1fa500184..d4c3d2989.
  Studio runs: before the merge merge --lib 26/26, db --lib repository 4/4, server --lib 509 passed 3 ignored, four
  red-checks; at the merged tree fmt clean, db --lib 116/116, ipc --lib 74 passed 2 ignored, merge --lib 30/30, db --lib repository 4/4, server --lib 522 passed 4 ignored 1 failed (main's own status-capture deadline test: 3/3 alone at main aef6faea1 without A-575, g255's report); dialer test 1 of 3 whole runs, 3/3 alone on main and with A-575. At the landed main d4c3d2989: fmt clean; `slates-server --lib repository` 3/3; `slates-server --lib` 530 passed, 3 ignored, 3 failed -- the status capture above and, in the whole parallel suite at load ~24 on 32 cores, `deadlines::tests::a_lapsed_write_lease_is_released_at_its_term_without_a_tick` and `a_lapsed_cli_attachment_ends_at_its_term_without_a_tick` ("within half a liveness window"), which pass alone 3/3 each and in the deadlines module run (9 passed, only the status capture failed); they had passed in all four whole runs with A-575 before main's A-550 second part and A-577 came in.
  Run requests filed (wire order both crates, db suites, server/client green suites, clippy/xtask/doc on 3 platforms,
  lend A/B). Told main: g292's Copy must step its release (`releasing()` + `Nodes::release_step`).
  Owed (documented in A-575 and GAPS): a branch's base as a delta against its source's checkpoint (so a branch can be
  made below it and stops pinning its source's history); the record's replication and a branch through the register
  (piece 2); a green at a commit that is no version's head (piece 3); a client/CLI/SDK surface for the four verbs.

## A-603 (2026-11-19, relaunched for UI goal 92): §4.17.1 shared piece 2, a repository's register -- LANDED
- Readback of THE RULE sent to main first. Worktrees: ~/slates-work/g261-a603 (branch agents/g261-a603-atomic-push, five
  WIP commits a325b0d8a..02e18c751, all tested on the Studio) and ~/slates-work/g261-a603-land (branch
  agents/g261-a603-register: the same tree as one commit 0015ddd0b, made with `git diff | git apply` on the base
  e85c17f9f, then merges 519c80d84, b78205195, 48689506f). Pushed main 31548ce9d..48689506f.
- Built: `Op::RepositoryAdvanced` (tag 96, after A-596's 90-95) carrying `RepositoryEntry` {repository, position,
  checks (Head{green,version}; Names{prefix, bound}), joined members, versions (GreenVersion{green,version,entry}), names
  (Bind/Retire/Default), state (PhaseRootAdded/Removed, MarkerAdded)}; guard in crates/db/src/repository/entry.rs (an
  overlay keyed by borrows of the entry; checks read at the position before the entry's own changes; each item judged
  against what the items before it left; refusals OutOfOrder/CheckFailed/StateChanged + the lineage's and namespace's);
  apply unconditional; every repository-changing op takes one position (old single ops, a member's GreenAdvanced, a
  member's destroy); phase roots + markers (BLAKE3 ids) charged to the merge budget; snapshot `repository_registers`;
  ipc DatabaseRefusal 5-7; server: A-575's verbs record one entry each, record_accepted records a member's version as an
  entry (`repository::version_record`, also the admission's judge).
- Evidence (Mac Studio d1323898, kiln, 2026-11-19): slates-db --lib 127/127 at the change, 131/131 merged with A-596;
  model test 128 histories through a Db over a real anchor segment with crashes; census 319 multi-green accepts, 625
  stacks, 523+223 state moves, 862 checks held, 179/58/296/435 refusals, 38 member destroys, 314 snapshot + 90 log
  recoveries; red ten ways (db) and two (server); slates-ipc --lib 74 passed 2 ignored; slates-server --lib whole 535
  passed, 3 ignored, 1 failed = main's known status-capture deadline test; cargo fmt --all clean.
- First census found the generator vacuous on the core property (1 multi-green accept in 128 cases): fixed with clean
  entries (items that hold, judged on the model). The budget red-check first stayed green: fixed with two versions
  that fit alone and not together.
- Run requests filed (run-requests.md, "2026-11-19 g261 ... 48689506f"): wire_order both crates, clippy/xtask/doc/cross
  lint, db integration suites, server integration suites with repositories.
- Studio scratch ~/.cache/g261-a603 deleted (6.5 GB); no other g261 scratch left there.
- Owed (main starts the next piece): (1) the push that judges several works before any commits (engine verdict split
  into judge + commit) and makes one entry of them, with piece 4; (2) the fleet half: the register as one ledger object
  of the repository's candidates, its record the entry with each version's merge record value, committed at f+1;
  holders recompute every version of an entry before acknowledging, all or none; takeovers adopt whole entries; the
  repository record replicated with it. Until then an entry of several greens is atomic on its owner, not across a
  takeover.
- Seen in passing (not mine): verbs.rs's doc comments of `chain_admits` and `record_accepted` are interleaved (the
  former's doc sits above the latter's, ~verbs.rs:9444).
- Branches to delete (main's call): GitHub agents/g261-a603-tmp-1, agents/g261-a603-tmp-2; local
  agents/g261-a603-atomic-push, agents/g261-a603-register (worktrees g261-a603, g261-a603-land).

## A-635 (2026-11-19, UI goal 93): the push over several works, all or none, with A-603's entry rebuilt to A-642's logs -- LANDED e50a8bea1
- On main 3239c8881..e50a8bea1: the change bf9c71d14 (the tree tested at e68a6c0fe), merge 48e467ffc (main 6154528e4),
  rename ec52d3eff (`Appended`, `BranchAppended`, `AppendRefused`, so receive-pack's wiring takes them beside
  slates_git's `Pushed` and slates_merge's `PushRefused`), merge e50a8bea1 (main 3239c8881). Each merge had one
  conflict, the amendment log's end, kept whole with main's entries first and A-635 last.
- Built: merge `Green::judge` / `commit_judged` (`Judged`, `Judgment`, `HeadMoved`; `submit_resolved` is judge then
  commit, one code path). db, A-642's logs: no repository position; each green its own log (a version appended only at
  the position the push read, else `Moved { green, read, at }`); state logs Names, Phases, Markers, Locks, each at its
  own position (`RepositoryCheck::Log`, `LockTaken`/`LockReleased`); an entry over two logs or more is one
  `AtomicRecord` each green's log points to (`Join`; `split_by` refuses a split cut; records folded at checkpoints and
  destroys; the snapshot carries records with their pointers, `AtomicHeld`); `Partition::check_creating`. ipc
  `DatabaseRefusal::Moved` (tag 10; `OutOfOrder` kept, superseded). server `repository::push`: the `RepositoryLogs` trait.
- Interface (crates/server/src/repository/push.rs; `#[cfg_attr(not(test), allow(dead_code))]` until callers land):
  `pub(crate) trait RepositoryLogs { fn append(&mut self, principal: &Principal, repository: RepositoryId, changes:
  Vec<BranchChange>, parts: EntryParts) -> Result<Appended, AppendRefused>; }`, implemented for `ShardState`.
  `BranchChange { Update { green, work: Increment }, Create { name, from, version, work: Option<Increment> }, Delete
  { name, version } }`; `EntryParts { checks, names, state }`; `Appended { changes: Vec<BranchAppended>, recorded }`,
  `BranchAppended { Moved { green, version, before }, Created { green, version }, Deleted { green } }`;
  `AppendRefused { Repository(Refusal), Changes(Vec<ChangeRefused { index, why }>), Entry(Refusal) }`, `ChangeRefusal
  { Conflict { windows }, Moved { head }, Twice, ApprovalRequired { identity }, Refused(Refusal) }`. It fits g258's
  `slates_git::receive::Judge`: `append` is one call with one change, `append_atomic` one call with all of them;
  `Moved`/`Database { Moved }` answer git's FAILED_TO_UPDATE / ATOMIC_TRANSACTION_FAILED.
- Tests (Mac Studio d1323898, debug). At e68a6c0fe: merge --lib 94/94, db 138/138, ipc 74 + 2 ignored, server 577 + 3
  ignored + main's known failure; the model test's census over 128 histories with crashes: several_greens 181,
  interleaved_singles 344, state_with_versions 408, state_alone 153, stale_refused 119, check_failed 134,
  state_changed 50, refused_by_rule 398, records_folded 76, member_destroyed 18, recovered_from_snapshot 307,
  recovered_from_log 32; the engine oracle: accepted 1977, accepted_before 251, conflicts 609; red sixteen ways (the
  amendment lists them). At ec52d3eff (kiln r996): fmt clean, merge 113/113, db 138/138, ipc 74 + 2 ignored, server 614
  + 3 ignored + main's known `deadlines::tests::a_status_capture_is_held_the_failover_delay_from_its_capture_not_its_request`.
  At e50a8bea1 (kiln r1028): fmt clean, merge 118/118, db 138/138, server 614 + 3 ignored + the same known failure.
- Run requests filed for main (run-requests.md): wire_order (db, ipc); clippy, xtask, doc, cross-target; the db and
  server integration suites.
- For main to delete: GitHub agents/g261-a635-tmp-1, agents/g261-a635-tmp-2 (and older g261 temp branches still there:
  a369-tmp-1/2, a497-tmp-1, a515-tmp-1, a534-tmp-1/2, matrix-1); local agents/g261-a635-push-together (worktree
  ~/slates-work/g261-a635) and agents/g261-a635-land (~/slates-work/g261-a635-land); A-603's agents/g261-a603-atomic-push
  (g261-a603) and agents/g261-a603-register (g261-a603-land).
- Next: A-636, the fleet half (row restated to A-642's logs in amendments.tsv), when main starts it.

## A-636 part 1 (2026-11-19, UI goal 93): the fleet half's protocol, the fenced ledger over a repository's logs -- LANDED 40bff1d55
- On main cde634359..40bff1d55 (branch agents/g261-a636-fleet-logs): tests first 3ee6be4c6 (+ the borrow fix 03a009510),
  the protocol 9d4dc9226, docs 1085769be, the holder's apply simplified 165c3a4b9, rustfmt's layout b59f5e010, the
  evidence 65ef6b447, merges 3c43dc406 (018d32ba2), 39dcef8e5 (b79ece5d5), 40bff1d55 (cde634359). Diff stat at landing:
  catalog.rs, ledger.rs, ledger/logs.rs, ledger/logs/tests.rs, GAPS.md, SLATES_DESIGN.md (1,959 insertions, 53 deletions).
- What it is: `crates/db/src/ledger/logs.rs` -- each log a ledger `Cohort` placed by its own object id; a record
  committed whole (`whole_cut`); holders apply the owner's whole cut in one step (their join); replicas read by
  position vector (NotHeld, NotApplied, Split via `catalog::spans_split_by`); a takeover asks every log's round first
  (`Cohort::can_take_over`) and keeps a record only where every log's read holds it whole (`keep_whole`). Ledger hooks:
  `Cohort::can_take_over`, `Cohort::held`, `Owner::log`, `Owner::truncate`, `Owner::offer_again`; `phase_one`.
- Evidence (Windows x86_64, jupiter, debug; the Studio offline, the macOS run owed): RED at 76af85355, 9 new tests
  failed against stubs, 8 others passed; GREEN at 39dcef8e5: ledger 17/17, db lib 153/153; census 465 multi-log
  committed whole, 429 single, 310 waited, 5 interleaved, 149 Moved, 96 stale, 194 takeovers (141 refused), 12
  dropped whole, 25 kept unfinished, 1,583 applied at join, reads 260 served / 72 Split / 25 NotApplied / 530 NotHeld;
  8 mutations all RED (each also caught by the model test) at 03a009510.
- Lessons: jupiter clones need `-c http.version=HTTP/1.1 -c http.sslBackend=openssl -c core.autocrlf=false` (main's
  tip); `kiln output` with wait_s 1800 hit the MCP idle timeout and the output it had read was lost -- poll at 600 s;
  my mutation driver's NO_BUILD read cargo's `error: test failed` line as a build error (the failed list decides).
- For main to delete: GitHub agents/g261-a636-tmp-1, agents/g261-a636-tmp-2; local agents/g261-a636-fleet-logs
  (worktree ~/slates-work/g261-a636) and agents/g261-a636-red (worktree ~/slates-work/g261-a636-red).
- Next: A-636 part 2, the live plane (each log's register on the wire, the state logs' register class, the server's
  takeover by this rule), when main starts it.

## A-636 part 2a (2026-11-19 ~20:40Z, relaunched; UI goal 93): the state logs' register class and each log's promise on the wire, a log at a time
- g306's split of part 2: (2a) this; (2b) the partition's records shipped a log at a time; (2c) the server's takeover by
  the rule. Main starts 2b and 2c.
- Worktree ~/slates-work/g261-a636-p2a, branch agents/g261-a636-p2a-log-registers (from origin/main b13a6e05f).
- Design decided: register.rs `RegisterClass {Own, Catalog, State(StateLog)}`; class bits in byte 10: top bit = the
  catalog (AUD-29-17, kept), second bit = a state log, which the top and third bits then name (Names 0x40, Phases 0x60,
  Markers 0xC0, Locks 0xE0); `placement_key` clears the top bit alone when the second is clear (every id with bit 6
  clear keeps today's placement and catalog round trip) and all three when set; `in_class`, `class`; MAX_OBJECT_COUNTER
  2^47-1 -> 2^45-1. A state log's register = its repository's id in that class: shares the repository's holders AND
  successor (one writer after a takeover). catalog.rs `LogName::register()`. ledger.rs: `Cohort::promise` (a holder's
  answer as LedgerPromise bytes; empty = refusal), `PhaseOne` + `Heard` (counts one register's promises: Malformed,
  Forged, Unbound, NotCandidate, Twice), `Owner::take_over` through them; `Cohort::can_take_over` removed. logs.rs:
  `LogsCohort::new(&[LogName])`; `LogsOwner::take_over` a log at a time (no ask-first: a live round cannot unfence),
  refused at the first log without f+1, `LogsTakeoverRefused.fenced` = logs fenced before it.
- FINDING for main (2c): a repository's GREENS still each follow their own id's successor (`successor(departed,
  object)` ranks by the object's own placement key), so after a takeover a repository's greens can land on different
  hosts; one writer needs every log's successor to be the repository's (state logs now are; greens are not).
- Server test fixtures with byte 10 bit 6 set (verbs.rs 7 tests, moves.rs 1, readmission.rs 1, fuse.rs 1) read as
  state logs now: run them by name in the green run; fix fixtures if any fails.

## A-636 part 2a, relaunched (2026-11-19 ~21:00Z, fresh g261; UI goal 93)
- Found the predecessor's worktree ~/slates-work/g261-a636-p2a with 160 uncommitted lines (LogsCohort::apply's fix: a
  holder fenced on one log of a record applies none of it until it holds the rest; its new test; the design entry;
  rustfmt's layout of class_tests.rs). Saved as 652e1ce0f, pushed to agents/g261-a636p2a-tmp-1. The red copy
  (~/slates-work/g261-a636p2a-red2: the new test, the fix out) committed 3dd9f9b3d, pushed agents/g261-a636p2a-red2.
- No wire variant in this piece: the promise reuses the existing LedgerPromise codec; PhaseOne, Heard, RegisterClass
  are in-memory types. Nothing outside slates-db uses the changed API.
- Landing worktree ~/slates-work/g261-a636p2a-land, branch agents/g261-a636p2a-land from origin/main b5231f0ff: the WIP
  squashed (one conflict, the amendment log's end: main's first, A-636 part 2a last); class_tests doc corrected (the
  server's fixtures with byte 10 bit 6 set now read as state logs); design entry gains the holder-join bullet and the
  owed is_catalog-only dispatch sites (takeover.rs, fleet.rs, mirror.rs) for 2b/2c. Run commit 5244e6762 pushed as
  agents/g261-a636p2a-run1.
- Studio run r98 (d1323898, CARGO_BUILD_JOBS=2, target ~/.cache/g261-target, logs ~/.cache/g261-logs): red (logs.rs
  from red2) then green: db lib `register ledger catalog mirror --show-output`, --test ledger, --test mirror,
  --test wire_order, fmt, 11 server lib tests with bit-6 fixtures (--test-threads=4). RESULT: pending.
- Then: evidence into the design entry + GAPS; one final commit on a fresh branch from origin/main (merge --squash of
  the land branch); merge origin/main if it moved; diff --stat; markers; push HEAD:main; run requests (fuse.rs's
  a_kept_mount_of_a_volume_recovery_could_not_rebuild_answers_each_request_eio on Linux; other platforms; gates);
  delete ~/.cache/g261-target and logs on the Studio; report to main; END.
- r98 (at 5244e6762 over main b5231f0ff) and r113 (at c8f10b413, + the stronger holder checks): red 2 of 2 both times
  with LogsCohort::apply's fix out (unit test: HostId(1) applied the names position and none of A's -- r98 caught it
  at the second check only, since a read of the applied cut skips a log never applied; held_cut now judges every log a
  holder holds, r113 failed at the first; model test "HostId(5)/HostId(4) applied Split"); green db lib `register
  ledger catalog mirror` 116/116 both, --test ledger 16/16, --test mirror 6/6, --test wire_order 2/2 + 1 ignored, 11
  server lib fixture tests 11/11, fmt clean. The predecessor's own first green (Studio log, 13:02 PST, at 965371e19)
  had failed the model test "HostId(1) applied Split" of record 2: the bug the apply fix closes.
- LANDED 2026-11-19 ~21:58Z: 34d10acb5, one commit on main 2410011da (main 2410011da..34d10acb5); main's three commits
  after the run's base touch no slates-db file and use nothing it changed; final slates-db identical to c8f10b413.
- Done at END: amendments.tsv A-636 row (backup amendments.tsv.bak-g261-a636p2a); run-requests.md entry (other
  platforms, fuse.rs's fixture test on Linux, gates); Studio: every ~/.cache/g261* path deleted (0 left, the
  predecessor's included); local worktrees g261-a636-p2a, g261-a636p2a-red, -red2, -land, -final removed (branches kept).
- For main to delete: GitHub agents/g261-a636p2a-tmp-1, agents/g261-a636p2a-red, agents/g261-a636p2a-red2,
  agents/g261-a636p2a-run1; local branches agents/g261-a636-p2a-log-registers, agents/g261-a636p2a-red,
  agents/g261-a636p2a-red2, agents/g261-a636p2a-land, agents/g261-a636p2a-final (and part 1's, listed above).
- Left (main starts): 2b (the partition's records shipped to each log's holders a log at a time); 2c (the server's
  takeover by the rule; a repository's greens following one successor; the is_catalog-only sites in takeover.rs,
  fleet.rs, mirror.rs reading ObjectId::class before 2b ships a state log's record).
- ENDED 2026-11-19 ~22:00Z. Nothing open for g261's A-636 part 2a.

## A-636 part 2b (main's assignment 2026-11-19 ~22:05Z, UI goal 93): the partition's records shipped a log at a time,
## with the class dispatch (takeover.rs, fleet.rs, mirror.rs read ObjectId::class) riding in the same piece
- Worktree ~/slates-work/g261-a636p2b, branch agents/g261-a636p2b (from origin/main 092386776). Nothing built yet.
- Asked main (SendMessage ~22:15Z): "greens follow their log's register, not their own successors" can't come from
  shipping (a green's log register IS its own id; green_adopt adopts instantly only because the successor holds the
  green's ledger + replica), so one successor = placing joined greens on the repository's holders (D-14 home change
  per join; greens stop spreading over copysets). Proposed it as its own piece 2b-2 before 2c. Building (i) meanwhile.
- MAP (origin/main 092386776):
  - Nothing ships repository entries, atomic records or state-log records today (only crates/server/src/repository*
    and vcs_store.rs name them). A green's versions ship as merge records (merge_service.rs PendingMergeRecord,
    ship_record ~2877), held in the register plane's Acceptor (db register.rs: accepted (ObjectId, u64) -> (epoch,
    value), durable, per position). green_adopt.rs: the green's successor is one of its holders.
  - The register plane: fleet.rs unplaced_heads (~2420: per volume, tombstone / catalog first / head) gathers Head
    {shard, object, record, shape, acked, quorum, live} and ship_head (~11142) commits ONE record per register per
    period through the owner's acceptor at f+1, acks recorded whatever the outcome (record_acks_in); placed_heads
    per register (one sequence). A log needs every position in order: a new per-position pending list or a batch.
  - Class dispatch sites: takeover.rs record_given_up ~1833 (catalog -> settled unrecorded: same for a state log);
    takeover.rs record_adoption ~2432 (catalog by class, else decodes tombstone / HeadValue / MergeRecordValue in
    turn: a state log's record must be dispatched by class BEFORE any decode); fleet.rs ~11152 ship_head counters
    (catalog flag); fleet.rs ~2713 placement_key as a volume id; mirror.rs ~579 page of heads skips catalogs (must
    skip state logs too); live_ship.rs 788/804/811/3316 map a register to its volume by placement_key (a state log's
    "volume" is its repository, never live-shipped: harmless, check). fleet.rs ~11100 retire path forgets [object,
    object.catalog()] only (a repository's destroy must forget its four state-log registers: later).
- MAIN'S RULING (~22:25Z): (i) is 2b as scoped -- build and land. (ii) is its own piece 2b-2 (after 2b, before 2c):
  a repository's greens placed on the repository's holders when they join (bind or branch), a home change at the
  join (D-14), no write path touching the configuration group; then an atomic record spanning a green's log and a
  state log has one holder set and one successor (2c's whole-record takeover needs it); the alternative (a
  cross-holder commit per record) is coordination on a write path (banned); A-642 part 1's copyset spreading gives
  way for greens bound to a repository -- RECORD THIS TRADE AND ITS REASON IN A-636's ENTRY (2b's docs).
- 2b DESIGN (decided):
  1. Value: slates-db catalog.rs `StateLogRecord { repository, log: StateLog, from, to, spans: Vec<LogSpan> (the whole
     record's, so 2c keeps it whole), changes: Vec<LoggedChange> }`, `LoggedChange { Name(NameChange),
     Joined(MemberRecord), Left { green }, State(StateChange) }` (Wire, appended; wire_order published). Server value
     class byte STATE_LOG_CLASS = 5 (server catalog.rs; is_single_value false for it: a ledger, never compacted).
     One register record per log per op, at sequence `to` (the position the op left the log at); its changes are the
     op's changes for that log in order, replayed by the db's rule (a change that changes nothing takes no position).
  2. Owner: crates/server/src/state_log.rs keeps per state-log register a bounded queue of parts (value, sequence)
     and each candidate's highest acknowledged position; enqueue after each committed op that moved a state log
     (positions read before/after via Repositories::log_position; sites: push.rs append ~656, repository.rs record
     ~302/333/497/520/543, the member-destroy path); fleet.rs ship_shard_heads also ships unplaced parts oldest first
     per register via ship_head (Head built like owed_record, acked = hosts whose highest >= sequence);
     record_acks_in routes State-class objects to state_log (placed_heads keeps one sequence, unusable for logs).
  3. Holder: check_held_record dispatches by class: a State record must decode as a StateLogRecord naming this
     register (sequence == to, from < to) and continue the holder's copy (from == 0 or (register, from) held), else a
     typed refusal (empty reply, counted). after_held_record: no compaction, no tombstone check for State.
  4. Takeover: record_given_up settles every non-Own register unrecorded; record_adoption dispatches by class BEFORE
     any decode: State -> counted (takeover.state_log_adopted), owned and placed, materialization owed to 2c.
     mirror.rs page skips every non-Own register. ship_head/ship_outcome counters by class.
  5. Bound: per register, parts kept until every candidate holds them; derived bound with typed refusal at admission
     (Overloaded) when a log's unshipped parts reach it -- decide the formula from the merge budget.
  Owed (record): a push's answer waiting for f+1 (the barrier), an install for a holder past a dropped part, parts
  rebuilt after an owner restart, batching, a repository destroy forgetting its four state-log registers.

## 2026-11-19 ~22:15Z: g261 relaunched (fresh agent) for A-636 2b, then 2b-2, then 2c (main's queue)
- Found ~/slates-work/g261-a636p2b clean at origin/main 092386776 (nothing built by the predecessor); no g261 run or
  scratch on the Studio. Fast-forwarded the branch to origin/main b94ae6e90. Building 2b there.
- Queue (main): 2b (class dispatch first, then state-log records shipped a log at a time, each carrying its record's
  identity and spans); 2b-2 (greens placed on the repository's holders at the join, record the copyset trade in
  A-636's entry); 2c (the server's takeover adopting a repository's logs and records whole). Each its own commit +
  merge; 3-line report to main after each; END when done or context short.
- Ada 22:45Z (relaunch brief): completeness first; a finding made by reading goes to main in one line, never chased.
- 2026-11-19 ~23:40Z: 2b BUILT (WIP ac87e1064 on agents/g261-a636p2b, pushed agents/g261-a636p2b-tmp-1):
  db: catalog `LoggedChange` (wire enum, hand-published in tests/wire_order.txt) + `StateLogRecord` {repository, log,
  from, to, identity (BLAKE3 spans ++ op bytes), spans (every span the op moved, greens' too), changes};
  Repositories captures `made` per op (log_names replaces names_moved; apply_advanced per-log changes that took
  positions; green_removed -> Left), Partition::apply = clear_made + apply_op + seal_made; Db outbox `logged` +
  `logged_in_transaction` (delivered at commit, dropped at rollback), `Db::take_state_log_records`.
  server: crates/server/src/state_log.rs (StateLogs parts per register + per-candidate highest ack; take_made, admit
  (Overloaded at bound = partition merge budget), acknowledged, check_held/judge (Malformed/Misnamed/Misplaced/
  Discontinuous), moves_state_logs); catalog STATE_LOG_CLASS=5, is_single_value excludes it; fleet: ship_shard_heads
  takes made, unplaced_heads + owed_state_log_parts, ShipCounters by class, record_acks_in routes State, check_held_record
  + after_held_record dispatch; takeover: given_up_shard, record_adoption match by class (adopt_catalog/adopt_own),
  TAKEOVER_STATE_LOG_ADOPTED; mirror: page_entries skips non-Own; admission in repository.rs record() and push.rs.
  Tests: db replica oracle in the model test (entry/tests/replica.rs), scripted records test, transaction test; server
  state_log/tests.rs (judge, StateLogs), fleet::state_log_tests (owner flow at f=1, holder prefix, counters),
  takeover tests (adoption, given-up), mirror::tests (page).
- Studio run r307 (red db, red server via perl stubs, then green; script ~/.local/state/g261-scratch/run1.sh; logs
  ~/.cache/g261-logs on the Studio; target ~/.cache/g261-target; clone ~/.cache/g261-src). RESULT: pending.
- A-636 part 2b LANDED 2026-11-19 ~23:45Z: main 5206419be..2c3fa2bcc. Change 140d2fc19 (squashed on base b94ae6e90 in
  worktree ~/slates-work/g261-a636p2b-land, branch agents/g261-a636p2b-land), docs d8be8e2c9 (merged-run evidence),
  merges 96a5aa13b (main 2f73eb075), 51005a79f (f1fbd2c44), a032de332 (f7684fc20), 2c3fa2bcc (5206419be); each merge's
  only conflict the amendment log's end (main first, A-636 part 2b last).
  Evidence (macOS arm64, Mac Studio): red r307 (server 6 of 8 new tests failing, stubs), r343 (db 3 of 3); green r343,
  r367 (db lib repository:: register ledger catalog 138/138, entry census 1,083 state-log records / 603 several logs /
  17 left; --test wire_order 2/2; --test model every_recovery 2/2; server lib state_log takeover:: mirror:: fleet::
  repository:: 85/85; fmt clean), r399 (second merge: server 85/85, db repository::entry 11/11, fmt).
  amendments.tsv A-636 row updated (backup amendments.tsv.bak-g261-a636p2b); run-requests.md entry appended (backup
  run-requests.md.bak-g261-a636p2b).
  For main to delete: GitHub agents/g261-a636p2b-tmp-1..4; local agents/g261-a636p2b (worktree g261-a636p2b) and
  agents/g261-a636p2b-land (worktree g261-a636p2b-land).
  Studio scratch: ~/.cache/g261-src, ~/.cache/g261-target, ~/.cache/g261-logs, ~/.cache/g261-run{1..4}.sh.

## A-636 part 2b-2 (next in main's queue; NOT STARTED by this agent, which ENDED after 2b on context): hand-off notes
Main's ruling: a repository's greens are placed on the repository's holders when they join it (bind or branch), a home
change at the join (D-14: the configuration group holds "membership, neighbourhoods, host epochs, takeovers, and moved
homes"), never per write; copyset spreading gives way for bound greens (the trade is already recorded in A-636 part
2b's amendment entry, "Next" bullet). What this agent learned, for whoever builds it:
- Placement today (crates/db/src/register.rs): `Configuration::place(object)` -> `place_content` -> `Placement::local(
  owner, quorum, neighbourhood, domains, object)` -> `candidates_for` -> `owner_copysets` + `copyset_for(object, ..)`
  -> `rendezvous_weight(host, object.placement_key())` (~lines 349-600). The successor: `RegionalConfiguration::
  successor(departed, object)` (~3822) ranks the survivors of `recovery_cohorts(departed, object)` by
  `rendezvous_first(.., object)`. Other per-object entry points that key on the object: `lease_cohorts`,
  `current_cohorts`, `readiness_cohorts`, `readiness_current_cohorts`, `within_tolerance`, `confirmed_for`,
  `copy_stale`, `owner_of`, `Settled::cohort`. A green's own register and its catalog both key by its placement key.
- The existing per-object configuration exception is A-291's move: `RegionalConfiguration::moved` (`Moved { owner,
  version, from, cohort }`), proposed as `Reconfiguration::Move { object, from, to, cohort }` (crates/cluster/src/
  config_group.rs ~560) from the owner shard (crates/server/src/moves.rs). It changes the OWNER, not the placement key;
  a member green has the same owner as its repository (same partition), so 2b-2 needs a new configuration entry: the
  green's placement key homed at its repository's (e.g. `homed: BTreeMap<ObjectId, ObjectId>` with a
  `placed_as(object)` applied at every entry point above, so every register class of the green follows), a new
  `Reconfiguration`/`ConfigCommand` variant, and its encoding in the configuration group's log and snapshots (wire!).
- Where the join happens on the owner: crates/server/src/repository.rs (`record_repository`'s root join, the branch and
  bind verbs, A-575's `Greens`), crates/server/src/repository/push.rs (`Step::Create`), A-685's CLI create. Propose the
  home change after the db records the join, from the control shard (as moves.rs proposes a move); until it commits
  the green is placed by its own key.
- Re-placement when it commits: the green's catalog re-ships by itself (`fleet::owed_record` ships a single value to
  any candidate lacking it); its merge-record ledger needs the new candidates to catch up as a neighbourhood change does
  (`merge_service::PendingGreen::retarget`, `greens_placed_on_current_cohort`, holder checkpoints A-404, ledger input
  fetch A-412): read how a new cohort member gets a green's chain before reusing it. The old holders let their copies
  go once the new placement holds the newest record (as a move's `adopt_move` does).
- Tests to write first: placement of a homed green equals its repository's (every class), the successor of both is one
  host after a departure (register.rs model), and the server's join proposing exactly one home change per join.
- ENDED 2026-11-19 ~23:55Z after 2b (context): Studio scratch deleted (no ~/.cache/g261* left); local worktrees
  g261-a636p2b and g261-a636p2b-land removed (branches agents/g261-a636p2b, agents/g261-a636p2b-land kept for main to
  delete, with GitHub agents/g261-a636p2b-tmp-1..4). Left: 2b-2 (notes above), then 2c.

## 2026-11-20 (fresh g261, relaunched by main): A-636 part 2b-2 then 2c
- Read relaunch-2026-11-19.md (completeness first), the preamble, CLAUDE.md §8 (origin/main), lint habits.
- main has parts 1, 2a, 2b; no 2b-2 or 2c (git log origin/main | grep A-636, at 9ca6e3883).
- Worktree ~/slates-work/g261-a636p2b2, branch agents/g261-a636p2b2 from origin/main 9ca6e3883. Design doc 5,334,674 bytes.
- DESIGN (main's ruling 2026-11-20, after my config-entry proposal was redirected): NO configuration entry (a per-object
  lookup table, §2 item 12; grows per branch). Route by id. Facts that shaped it: part 1's holder rule needs ONE
  successor (LogsCohort::apply lowers only logs a host is a candidate of; spans_split_by skips unnamed logs); but a
  green's successor must HOLD its ledger (green_adopt.rs, A-333), so the family key must govern placement too (safe:
  a member is placed by it from birth, nothing moves). So the copyset trade of 2b's "Next" bullet stands.
  1. register.rs: family bit 0x10 of byte 10 (below the 3 class bits); ordinary counter <= 2^44-1 (MAX_OBJECT_COUNTER).
     Family id = creator | partition | byte10 (class|0x10|tag_hi4) | byte11 tag_lo8 | bytes12-15 number (u32).
     `line_key()` = placement_key with, for a family id, creator and number zeroed (partition, family bit, tag kept).
     `rendezvous_weight` hashes line_key (was placement_key): placement AND successor follow the family. Identity
     (placement_key, in_class, catalog(), moved keys) unchanged. Ordinary ids: line_key == placement_key, no change.
  2. db: Partition `family_minted: u64` = max family number applied (VolumeCreated, RepositoryCreated), appended
     PartitionSnapshot field. Join rule (repository.rs check_joined + entry.rs join): member line_key == repository
     line_key, else NotFound. Test helpers mint every test id in one family (tag 0) so old tests keep meaning.
  3. server: verbs::fresh_family_id(state, tag) (number = max(db.family_minted, shard reservation)+1, tag of R =
     number mod 4096); repository ids, new roots, record_branch, push plan_create mint family ids (two creates in one
     push get distinct ids). CreateRepository with an existing green: refused Unsupported (re-key needs AUD-14).
  4. Findings by reading (to main, one line each): a push creating two branches mints one id twice (next_seq doesn't
     move between mints; one transaction takes one seq); an older-build snapshot fails from_bytes and recovery skips it
     silently (every appended PartitionSnapshot field).
