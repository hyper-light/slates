# g088v-kiln-vendors progress (kiln UI goal 88's vendoring part, kiln goal 01a1ec34599974448569750ac79093b6)
Rewritten 2026-11-18 ~23:35Z from the agent's own context after the ~/.cache wipe.

POLICY_READ (g088v, 2026-11-18, role: implementer, kiln/slates; owned paths: kiln's vendor/slates, vendor/MANIFEST.toml
[projects.slates], vendor/patches/slates, crates/kiln/src/workspace/slates.rs + review.rs where the contract reaches,
kiln CI files for the slates build; Shards paths owned: none, vendor/shards untouched): slates CLAUDE.md origin/main §8;
restart preamble (rewritten ~22:10Z, light gates added ~23:00Z) + common-brief; kiln AGENTS.md/CLAUDE.md (no rustfmt,
clippy is the check, vendor.py check for vendored code); Shards main 4ff00b72a AGENTS.md f66d304c...6db41,
docs/development-rules.md df2acf47cf92996e9435f9a4988f10e9e7aaa21ca952a00069c044c7ed4f13d3, development-evidence.md
61613a24...68fb63 (R01-R16 read in full).

## The goal's four items (from the first brief) and where they stand
1. Re-vendor slates at current main with its manifest entry: DONE at e418f5cd2 (contract 36), kiln 688cb98ce;
   now A-496 to 1f332da55 (contract 41), below.
2. No slates CLI spawned by kiln's workspace code: audited, none (kiln on slates-api since g263).
3. slates_client::Hold where kiln keeps an attachment across turns: none needed. Only the CLI makes Termed attachments
   (verbs::consumer_of); kiln's mounts are Consumer::Bridge (no term), its Shards streams Consumer::Guest (end with the
   stream), other calls' clients end with the call. Documented in workspace/slates.rs module docs + agentic.md W3.
4. kiln's CI builds vendored slates on every platform: kiln c43229e19 added windows-11-arm + ubuntu-24.04-arm to
   .github/workflows/ci.yml's build matrix (clang-cl step for arm64 Windows). Their runs are main's.

## Landed earlier (2026-11-18 ~21:10Z): kiln main a2cc1e429..c43229e19
- 688cb98ce: vendor/slates -> e418f5cd2, CONTRACT 23 -> 36, let_go -> mount::unmount_busy (LetGo logged),
  MountMissing::LazyUnmount dropped, DriftReport::elided + LandingReport::degraded_elided shown, A-384 docs.
- c43229e19: CI arm64 lanes (item 4) + agentic.md W1.
- Unit tests: Mac Studio r87, `cargo +stable test --locked -p kiln --lib workspace::slates`: 20 passed.

## A-496 (resumed 2026-11-18 ~23:20Z by main): kiln re-vendors slates at current main (contract 41)
- Worktree ~/slates-work/g088v-kiln-vendors-kiln-a496, branch agents/g088v-kiln-vendors-a496 from kiln main ae9ae2509.
- `vendor.py slates 1f332da55 --no-gate`: change 0002 (g267's role::check_fleet) conflicted in crates/api/Cargo.toml;
  resolved to upstream's content in ~/.cache/kiln/vendor/conflict/slates; `--continue` dropped it (no change on
  upstream). 0001 refreshed. `vendor.py check slates`: 1101 files, 1 recorded change, 0 differences.
- Cargo.lock: slates-base + aws-lc-rs, slates-merge + slates-mem (`cargo metadata --offline`).
- kiln port: CONTRACT 41 (doc 37-41), missing_of + MountManager (as g267's K5 maps it), summary_json shows chmod,
  planned_files counts it (doc: a mode git keeps no part of holds the landing back), writable's refusal words name
  both causes; review.rs: PlannedAction::Chmod arm (git keeps the executable bit alone; bytes read at head and held to
  the folder's copy, Moved otherwise), executable_change helper, writes_untracked includes Chmod. agentic.md W6, W8.
- New unit tests: slates.rs w6_a_plan_shows_a_file_whose_mode_alone_changes_and_counts_it; review.rs
  w8_a_files_mode_alone_changed_is_committed_where_git_keeps_it, w8_an_untracked_files_mode_change_is_judged_by_gits_ignore_rules.
- Commit c064afb06 on the branch (pushed). Studio run r482: --lib workspace::slates::tests workspace::review::tests
  workspace::slates_fleet workspace::slates_move, then clippy -p kiln --all-targets -D warnings.
- Coordination: g267's K5 (origin/agents/g267-k5, not on main at 23:20Z) re-vendors to b2909fcf5 (contract 40) and
  ports 37-40 the same way (MountManager -> WorkspaceMissing::Mount); whoever lands second re-runs
  `vendor.py slates <newest pin> --no-gate` on the merged tree and hand-merges the CONTRACT doc/const.
- Sibling noticed, not changed (kiln: change only what the task asks): review.rs judges "executable" by any of 0o111,
  git by the owner's bit alone (read-cache.c ce_permissions: mode & 0100; from memory, to verify) -- differs only for a
  file with group/other execute and no owner execute.
- 2026-11-18 ~23:40Z (after the session restart): r482 had finished before the restart: focused lib 41 passed (incl.
  the 3 new tests, slates_fleet's check_fleet tests on upstream's reader, slates_move); clippy -p kiln --all-targets
  failed on 5 PRE-EXISTING findings, none in my lines: slates.rs unmount_patiently `crate::rt::sleep(next)` with no
  kind-allow (g264, 4ad3e80dc: lib), slates_fleet.rs:761-764 `manifest["..."]` indexing in a test (g267, K2: lib test).
  Both still on kiln main 1ec78034d. Not mine to change (kiln AGENTS.md); reported to main.
- kiln main moved (K5 landed: 1ec78034d, Slates b2909fcf5, contract 40, 0002 dropped). Merged into my branch:
  31 vendored conflicts + verdict.rs taken at 1f332da55 (b2909fcf5 is its ancestor); `vendor.py check slates` 0
  differences; MANIFEST keeps 1f332da55; CONTRACT keeps K5's 40 paragraph + mine for 41. Merge ac814daba (pushed).
- r20 (Studio): the same focused lib tests + clippy on the merged tree.
- r20 (merged tree ac814daba): focused lib 46 passed; clippy the same 5 pre-existing findings only.
- kiln main moved again (b043c2833); merged clean -> d69ac4975 (it touched slates.rs, so the focused tests were
  rerun): r66 focused lib 47 passed (my 3 new tests among them); clippy the same 5 pre-existing findings only.
- Main's correction (Ada): agents compile only the focused --lib tests; no clippy/cross/full builds. Nothing in flight.
- **LANDED: kiln main b043c2833..d69ac4975** (c064afb06 re-vendor + port, ac814daba merge of K5, d69ac4975 merge).
  `git diff --stat origin/main HEAD` before the push: my files alone (Cargo.lock, review.rs, slates.rs, agentic.md,
  vendor/MANIFEST.toml, vendor/slates/*). Studio clones + 25 GB target removed; logs ~/.cache/g088v-a496-*.log there.
- Filed (run-requests.md ~23:59Z): kiln's CI on every platform (GitHub linux + build legs incl. windows-11-arm and
  ubuntu-24.04-arm, kiln's own CI/release), vendored/roles/served/e2e checks, with the 5 pre-existing clippy findings
  named as not A-496's. Told g267 (inbox).
- LEFT: nothing of A-496 in code. The sibling (review.rs judges "executable" by 0o111, git by the owner's bit alone)
  is reported, not changed.

## Piece: review judges "executable" as git does (UI goal 88, kiln only; main's brief ~00:30Z 2026-11-19)
- Source read first: git master e1ec09b23 (2026-11-16), object.h line 134 `#define ce_permissions(mode) (((mode) &
  0100) ? 0755 : 0644)`, applied by create_ce_mode (object.h 135-144) and canon_mode (146-155); read-cache.h line 28
  (ce_mode_from_stat -> create_ce_mode). (cache.h no longer holds it at master.)
- Worktree ~/slates-work/g088v-kiln-exec-bit, branch agents/g088v-kiln-exec-bit.
  - 402e3e106: the failing test alone (review.rs w8_git_judges_a_file_executable_by_its_owners_execute_bit_alone).
  - 13c0cc806: git_executable (mode & 0o100) used for written files and executable_change; unseen_mode_change (a mode
    git keeps no part of on a TRACKED file) counted by unseen_writes (replaces ignored_writes) so an unmounted
    workspace's landing is held back as a mounted one's is; writable's words; planned_files/CONTRACT docs; agentic.md W8.
    New test w6_a_mode_change_git_cant_see_on_a_tracked_file_holds_its_landing_back.
  - bca212e2f: merge of kiln main (g264's e215dc83e: ignored_writes took the landing's excludes; g267's re-vendor at
    Slates e2d1bc42b): unseen_writes takes excludes too; writable/CONTRACT/W8 conflicts resolved keeping main's words.
- Runs (Mac Studio, focused --lib only): r132 RED on 402e3e106 (the new test FAILED at review.rs:261, its first
  assertion: left Some(true), right None; the other two cases fail by the same rule, derived, not observed, since a
  test stops at its first failing assertion; 13c0cc806's message says "each of its three cases") then GREEN on
  13c0cc806 (36 passed); r187 on the merged tree bca212e2f: 36 passed. Studio clone + 27 GB target removed.
- **LANDED: kiln main 3b0f9ae91..c22e5b720** (402e3e106, 13c0cc806, bca212e2f + a clean merge of main's
  slates_fleet.rs-only commit). Diff stat before the push: review.rs, slates.rs, agentic.md alone.
- Sibling, not changed: with core.fileMode=false git trusts no execute bit (read-cache.h ce_mode_from_stat keeps the
  index's mode); review does not read core.fileMode, so such a repository's review could still commit an owner-bit
  change git itself would not record.
