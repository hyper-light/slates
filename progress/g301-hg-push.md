# g301 — Mercurial pushes (A-641; kiln UI goal 93, 51cef9f7; §4.17.3 piece 5)

Supervisor: main. Role: implementer (Claude). THE RULE read back to main 2026-11-19: one piece in parts, failing unit
tests first, `cargo test -p <crate> --lib` + `cargo fmt` only, on the Mac Studio (d1323898, sync false, GitHub clone of
the pushed branch, own CARGO_TARGET_DIR under ~/.cache, deleted after); merge = fetch, merge origin/main, read
`git diff --stat origin/main HEAD`, `git push origin HEAD:main` as its own command; END with this file, run-requests.md
and a 3-line report. Worktree ~/slates-work/g301-hg-push (branch agents/g301-hg-push, made with `git worktree add -b`
from g066-fs-server's .git). Scratch ~/.cache/g301-scratch (commit messages, the fixture tarball).

Course correction from main (2026-11-19): A-642 (Ada) replaces A-603's repository-wide register position. Each green
keeps its own log; an atomic push is one entry on each green it moves, appended only at the positions it read; phases
and markers in logs of their own; g261 rebuilds the entry. Code unbundle against a trait of its own ("append these
greens' changesets at these expected positions, and these state changes"), never A-603's RepositoryEntry/Check;
check:heads compares the greens' positions.

## Landed
### A-641 part 1 (2026-11-19 ~06:20Z): main 6154528e4..a5dffc913
Commits: 627df5258 + be5c61030 (fixture script; its first run's fault fixed), 9fc926ee8 (code, tests, fixtures),
709928d1e (rustfmt from the Studio's patch; prop_assert fix), 355d1c987 (docs), merges a51b0831f and a5dffc913.
- `crates/hg/src/push/{mod.rs,check.rs,reply.rs,tests.rs}` (slates-hg; lib.rs one `pub mod push;` line):
  - `push::read(&Bundle2, &mut Admission) -> Result<Unbundle, Refusal>`: parts routed by lower-case type to a push's
    handlers (replycaps, check:heads/updated-heads/phases/bookmarks, changegroup, phase-heads, bookmarks, pushkey,
    obsmarkers, pushvars, output); unknown mandatory part/param -> `Refusal::Unsupported{parttype lower, params sorted}`
    (7.2's `_gethandler`); advisory ones skipped; payloads decoded by A-514/A-545 codecs, kept in stream order
    (`Unbundle.parts: Vec<Taken{id, mandatory, asks}>`); one changegroup, versions 01-03, targetphase public/draft only.
  - `push::check::judge(&Unbundle, &impl serve::Repository, &mut Admission) -> Result<(), CheckRefusal>`: the four
    checks in stream order, 7.2's exact words (`RACED` + bookmark/phase suffixes, `short` = 12 hex digits),
    check:phases in allphases order, [nullid] rules; `heads_argument` + `judge_heads_argument` (force / list in heads'
    order / hashed SHA-1; "repository changed while preparing|uploading changes - please try again").
  - `push::reply::Reply::{accepted(&[Replied], wanted), refused(&[Replied], &Refused)}` + `encoded_len`/`encode`;
    `heads_changed(removed, added, added_closing)` (never 0); `multiple_heads(branch)` (reject_new_heads' words, the
    branch as CPython's bytes repr).
- Fixtures: `crates/hg/fixtures/push/` (66 files + SHA256SUMS, made by make-push-fixtures.sh on the Studio
  2026-11-19 05:5xZ, stock hg 7.2 via uvx, SSH v1 recording stand-in): push-bookmark, push-phase, push-plain,
  push-race (replayed after another client moved the head), push-bookmark-plain/push-race-bookmark,
  push-phase-plain/push-race-phase, push-newhead (hooklib.reject_new_heads), push-unsupported-part/-param.
  Brought back via `kiln run -q --no-sync --on adas-mac-studio.local-2 -- '... tar czf - . | base64' > file`,
  `shasum -a 256 -c SHA256SUMS` 66/66 OK locally.
- Evidence (Studio, debug): slates-hg --lib 217/217 at 709928d1e (load 5.3), fmt clean; merged a51b0831f 228/228
  (load 8.7), fmt clean; a5dffc913 = a51b0831f + main commits touching nothing in crates/hg. Red six ways in
  disposable copies (each only its guarding tests red): unknown mandatory param ignored (3), updated-heads always
  hold (3), short of 10 digits (3), reply:changegroup mandatory (1), repr always single-quoted (1), check:phases in
  payload order (1, oracle only).
- Docs: §4.17.3 status (A-641 part 1) + piece 5 line; amendment "A-641 part 1" at the log's end; GAPS Mercurial
  paragraph (built + owed item 5); research/mercurial-server.md §20 (A-619 part 2 took §19 meanwhile).
- amendments.tsv row A-641 updated (owner g301, part 1 landed).

### A-641 part 2 (2026-11-19 ~07:00Z): main e2e7e7051..9c3235451
Commits: 493c41ae2 (verify.rs, tree.rs, work_tests.rs, Cargo.toml deps), 1eadecd69 (empty file logs no longer
reported new: the repeated push's test caught it; fmt; Cargo.lock's two edges), 95208b60b + a47a715a8 (path audit
transcribed from 7.2's pathutil.py 87-118 and encoding.hfsignoreclean; fmt), 2395662a9 (docs), merges c79bdecf3,
49bcf1947 and the landing merge (main moved under the shared .git twice; each time only docs conflicted).
- `push::verify::verify(&ChangegroupPart, &impl Held, &mut Admission) -> Result<Verified, Unverified>`; `Held` trait
  (changeset_text / manifest_text / file_text by node; tests use a map, `Kept`; the repository's index must answer it
  in part 3); `Verified` (new changesets/manifests/files + passed_over, indexed by node); `audit(path)` with
  `PathFault::{EndsInSeparator, IllegalComponent, NestedRepository{base}, NotHeld}`.
- `push::tree::{Trees, Move, work, LayoutRefusal, WorkRefused}`: flat manifests laid out as directories named by
  BLAKE3 (derive_key "slates 2026-11-19 a Mercurial manifest's directory") = A-618 `PushObjects`;
  `work(&Move{name, base_version, base, head}, &Green, (&Verified, &impl Held), room, &mut Admission) -> Built`.
- slates-hg now depends on slates-merge and blake3 (crates/hg/Cargo.toml; Cargo.lock +2 lines).
- Evidence (Studio, debug): slates-hg --lib 237/237 at a47a715a8 (load 10.6), fmt clean; merged 265/265 at
  49bcf1947 (load 16.4), fmt clean; the landed merge adds only main commits touching nothing in hg/merge/vcs/lock.
  Red nine ways: node check skipped, links always known, missing files tolerated (1 test each); directory names blind
  to entry ids (2); held revisions taken again (2); executable laid out as plain (1); nested-repo check, HFS+
  cleaning, short names each removed (1 each). (A first A1 mutation did not compile and proved nothing; redone.)
- Docs: §4.17.3 status "A-641 part 2" + piece 5 line; amendment "A-641 part 2"; GAPS (built + owed item 5 narrowed);
  research §20 bullets (addgroup/validate words, pathauditor + hfsignoreclean with SHA-256 and lines).
- Seen on main, not mine: slates-merge warns `source_of` never used (crates/merge/src/engine/copy.rs:535, A-626 part 2).

## Left (A-641 parts 3+), in order -- A-635 HAS LANDED (read its amendment first)
A-635's landing API, in slates-server: `crates/server/src/repository/push.rs` `RepositoryLogs::append(principal,
repository, changes, parts) -> Result<Appended, AppendRefused>`; `BranchChange::{Update{green, work}, Create{name,
from, version, work}, Delete{name, version}}`; `EntryParts` (checks, names, state changes); `RepositoryCheck::Log`
(a state log's position: phases, markers); refusals `Moved{green, read, at}`, `ChangeRefused{index, why}`; a work
sent again answered its version. `Green::judge`/`commit_judged` underneath. Fakeable by a tool's tests.

## Left, older plan (parts 2+ as first written)
1. Part 2 (slates-hg): verify the changegroup (decode `ChangegroupPart.payload` with its version; every delta's base
   held or earlier; nodes via node::check; links name changesets of the bundle; file nodes a new manifest names
   present) and classify: each new changeset's line (named branch + first parent = a line's head; new head = new
   line where policy allows; merge of two lines moves the first, retires the second), bookmarks, phases (publishing
   -> public); one work per moved line/bookmark via `slates_merge::push::build` (needs slates-hg -> slates-merge dep)
   with a `PushObjects` over flat manifests (directories synthesized with content-derived ids so unchanged subtrees
   skip; blob = filerev::split bytes; x -> Executable, l -> Symlink).
2. Part 3: the landing trait (A-642): `append(moves: [(green, read_position, work)], holds: [(green|log,
   position)], state: phase roots/markers changes)` -> refused naming the green that moved -> `error:pushraced` with
   RACED; check:heads = every line green's position + the line set; check:updated-heads = each named head's line
   green; check:bookmarks = the bookmark green or its absence; check:phases = the phase log's position.
3. Part 4 (slates-server): `vcs_policy::admit` (Name::Hg(Branch|Bookmark, name), Submit/Rewrite/Delete) and
   `hook_driver::run` with `vcs_hooks::hg_pretxnchangegroup`/`hg_changegroup` at their points (a view seam until
   piece 9's revlog view), hook words "<hook> hook exited with status <n>"; pushkey namespaces' CAS
   (bookmarks.pushbookmark, phases.pushphase rules).
4. The bundle read in slices within the shard's step budget.

## Next parts (main starts them)
3. slates-server (+ a pure classifier in slates-hg): the namespace names of Mercurial lines and bookmarks (decide a
   prefix-free scheme: one green a branch head, one a bookmark); classify a verified push's new changesets: each new
   branch head moves the line whose head it descends from on its branch (BranchChange::Update with tree::work), a new
   head on a branch is a new line (Create, policy-gated: refused in reject_new_heads' words, reply::multiple_heads), a
   new branch a line made at its first parent's version (Create), a merge of two lines moves the first and retires the
   second (Delete); bookmarks part / bookmarks pushkey -> the bookmark's green (Update/Create/Delete, CAS on the old
   value); the race checks as A-635 checks: check:heads = every line's green at its read position + the names log;
   check:updated-heads = each named head's line green; check:bookmarks = the bookmark's green or the name's absence;
   check:phases = RepositoryCheck::Log on the phases log; phases: publishing -> public roots moved (StateChange) in the
   same entry; `Moved` -> error:pushraced with check::RACED; reply via push::reply; a `Held` over HgIndex + ContentStore.
4. slates-server: vcs_policy::admit (Name::Hg(Branch|Bookmark), Submit/Rewrite/Delete) before append; hook_driver::run
   with vcs_hooks::hg_pretxnchangegroup (view seam until piece 9's revlog view) and hg_changegroup after; hook words
   "<hook> hook exited with status <n>"; pushkey namespaces' CAS (pushbookmark/pushphase rules); the bundle read in
   slices.

## Run requests filed (run-requests.md)
- "2026-11-19 g301 main a5dffc913": clippy -p slates-hg --all-targets, cargo xtask check, cargo doc -p slates-hg.
- "2026-11-19 g301 main 9c3235451": the same for part 2's files (verify.rs, tree.rs, work_tests.rs).
