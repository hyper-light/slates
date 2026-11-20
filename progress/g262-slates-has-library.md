# g262-slates-has-library — progress (rewritten 2026-11-18 ~15:30 PST from the agent's own context after the wipe)

Goal: UI goal 88 (kiln 28e73c34), library part (kiln 01a1ec3459997444813b8a57ecfae5ae): slates has a library API for
embedding hosts such as kiln (crate `slates-api`, crates/api): documented, versioned; instance start and attach; volume
create, clone, snapshot, destroy; serving (mount; virtio-fs/DAX descriptor to a VMM); changes and drift; landing plans
and grant-bound landing; observation; typed refusals; by-use tests.
Owner: Claude subagent g262 (of the slates supervisor "main"). Clones: ~/slates-work/g262-items-1-3 (slates, base for
worktrees), ~/slates-work/g262-slates-has-library-kiln (kiln, read-only, stale).

## Landed on slates main (2026-11-18, this owner, one piece at a time)
- a6f5f3700 (merged b7a21d2e1): items 6-8. The Shards stream test offers DAX at INIT and a 3 MiB file is marked
  `FUSE_ATTR_DAX`; `the_contract_history_names_every_version_in_order` (doc-truth, red-checked); library.md, §4.12,
  GAPS current. Studio r70.
- 069a83e23 (pushed 434f9a303): item 11. The completion bridge's per-start `Box::leak`ed flags owned: stop is an mpsc
  channel, the pass-on reads the parked flag (`region::PARKED_ASYNC` = 3, `park::parks_synchronously`). Red 128 blocks
  kept over 64 turns (r104), green (r116); `crates/ipc/tests/completion_memory.rs`; bug record
  docs/bugs/2026-11-18-the-completion-bridge-kept-two-blocks-a-start.md.
- 190e6c824 (pushed e4752891b): item 12, the clause-by-clause audit table in GAPS' consumer-contract entry.
- d9255072c: gap (a), 28 `cfg(unix)` gates lifted in crates/api/tests/api.rs (landing, plans, drift, changes, paged
  audit, async form on Windows; `as_a_host_names_it`, `readable_within` WSAPoll twin). xwin check clean (r166).
- 06299f281 (pushed c1c450ca3): gap (b), the Shards stream test on Windows (pipe handle route); seam's Windows
  `reads_its_end` reads within START_WAIT. xwin check clean (r177).
- f137984d8: gap (d), docs/library.md's examples compiled as doctests (`#[cfg(doctest)] pub struct Guide`); 3/3, red
  E0609 (r190).
- 8551a56a6: gap (c), `a_vhost_user_device_the_library_serves_drives_a_guest_whose_file_it_reads` (Linux, never
  compiled by its author: no Linux C cross-compiler on the Studio) and the off-Linux refusal test (green macOS, r199);
  Linux-only dev-dep slates-bridge-virtiofs.
- 4ae973864 (pushed e3979c2ac): the Node SDK suites end on Windows: `crates/sdk-node/tests/anchor.mjs` (group kill on
  Unix; on Windows the anchor's own process ended, its kill-on-close job ends the daemon; fail past 20 s); CI `sdk` job
  windows-latest + windows-11-arm Node legs; the SIGSTOP freeze test skips loudly on win32 (NtSuspendProcess twin
  owed); Python on Windows owed (GAPS).
- Earlier (pre-wipe, other owners of g262): items 1-4 (bridge spawn refusal typed, the bridge's pass-on, a refused
  landing's record terminal, `mount::unmount_busy` CONTRACT 36), the consumer contract A-299 (CONTRACT 1-36), the
  async form (CONTRACT 31), the audit trail (CONTRACT 30/35), etc.

## Goal state
Every clause of goal 88's library part is held by a test on main (GAPS consumer-contract audit). To mark it met, main's
filed runs must pass: Windows `cargo test -p slates-api --test api` + doctests; Linux the vhost-user test, the Shards
DAX test, doctests; the SLATES_TEST_MOUNT busy-mount test; the two `role::liveness_tests` failures (main triaging).
Owed beyond the clauses: item 5 (MCP list tools a page at a time), item 9 (async form for the remaining verbs), item
10 (a merge's versions typed on the wire).

## Run requests filed (re-filed below if main has not confirmed them)
- Windows: slates-api `--test api` and `--doc`; the Node suites on both arches (4ae973864).
- Linux: the vhost-user test (never compiled), Shards DAX test, doctests.
- Kernel mount: `SLATES_TEST_MOUNT=1 cargo test -p slates-api --test api a_busy_mount_is_let_go_of_the_oss_way`.

## Now (2026-11-18 ~15:30 PST): kiln piece
An agent started in a person's shell whose folder lies inside ANOTHER agent's slates mount shares that agent's volume
(today kiln lays a fresh volume over the mount, which slates refuses, A-227). g264's ledger row W in kiln
docs/agentic.md. Failing kiln `--lib` test first, red-checked once; docs and ledger row; merge to kiln main.
- 2026-11-18 (after the usage-limit restart): kiln worktree ~/slates-work/g262-kiln-shell (branch
  agents/g262-shell-shares-volume, from kiln origin/main ae9ae2509): red 5513a1bff (pushed as
  agents/g262-shell-shares-volume-red: `workspace::from_shell` moving the shell step only, both call sites
  (registry.rs open_at via `Registry::live_at`, agent.rs via `workspace::live(h.list())`), the test
  `w2_an_agent_started_in_a_shell_inside_another_agents_mount_works_in_its_volume`), green 41054df8c (the share in
  `from_shell`; docs/agentic.md W2 prose + ledger row W). Studio run r473 (red) was lost with the session: rerun with
  its log kept in ~/.cache/g262-kiln-logs on the Studio. Next: land on kiln main (one commit, msg in
  ~/slates-work/g262-msg/kiln-shell.txt), END.
- LANDED on kiln main (pushed 7e5ccf240): d83f799db, a shell-started agent inside another agent's mount works in that
  agent's volume (`workspace::from_shell` at both shell-reading sites; `workspace::live`; `Registry::live_at`). Red once
  (Studio r5: FAILED at "the other agent's volume..."), green on the merged tree (r71: workspace tests 11/11). The
  docs/agentic.md conflict (ledger row W, main had changed it) resolved onto main's row. Clippy left to main (filed):
  red on kiln main's own lines in workspace/slates.rs and workspace/slates_fleet.rs, none in this change's files.
- LANDED on kiln main 70e3dcaed: kiln takes Slates at ce9b6905e (contract 42, A-507): `vendor.py slates ... --no-gate`,
  `check slates` 0 differences; Cargo.lock (slates-api + slates-bridge-oci, slates-merge - slates-mem); CONTRACT 41 ->
  42 with its doc paragraph; `missing_of` exhaustive (Busy/StillMounted/MountTable -> WorkspaceMissing::Mount) + its
  unit test; tests/common/agents.rs PATH drops /sbin (umount by name, dead since A-507; /usr/sbin kept for lsof); ledger
  row W's owed sentence closed (conflict with main's row resolved onto main's). Tests: kiln `--lib workspace::` 94/94
  (Studio r213). unmount_patiently left to g264 (its change merged in from main). Kiln CI on every platform filed.

## Now (2026-11-18 ~17:40 PST, after the usage-limit restart): UI goal 88 part 01a1f1249c5e75b5bc82c9eb011550f4
kiln's Shards microVM engine proven through kiln on Linux KVM and Windows WHP, as on macOS HVF. The predecessor's clone
~/slates-work/g262-kiln-microvm (branch agents/g262-microvm-kvm-whp) had no commits of its own: nothing of this piece
had landed.
- Read: the VMM is Shards' own on all three (vmm/build.rs: hvf/kvm/whp); the device is Slates' virtio-fs + DAX window
  over the stream kiln asks (`sandbox::served`, `serve_shards`): a Unix socket on Linux/macOS, a duplicated pipe on
  Windows, whose DAX asks Slates answers only for a live known holder (A-180) -- kiln's copy goes as the boot answers,
  so piece 3 (keep it for the run) is owed; Windows' WinFsp is for host mounts only; Windows paths: git's `C:/` top vs
  `Seen::said`'s text match (piece 2); the resync end is dropped by `SlatesVolumes` on every host.
- Plan written: docs/agentic.md "W8 on Linux KVM and Windows WHP" (six pieces), ledger row W, W2 sentence.
- Piece 1 (built): `lineage::WorkspaceUse` -- an isolated start's volume needs no host mount (both doors:
  `agents.start` by its boundary, `/api/harness/sessions` by `Registry::isolation_for`, extracted from `open_as`);
  `workspace::slates::facts` says "keeps no workspaces" first. Test
  `w8_an_isolated_agents_volume_needs_no_host_mount_so_a_runner_without_winfsp_or_fuse_serves_one`.
  Commit 64220bcfc, merged with kiln main ef2812dce as f0242d797, pushed as branch agents/g262-microvm-kvm-whp.
  Studio run r64: red (served judgment taken out) then green + neighbours.
- LANDED on kiln main da7a00ee6 (2026-11-18): piece 1 (commit 64220bcfc; merges f0242d797, d05323bb7, da7a00ee6; the
  last resolved ledger row W onto main's b7720ebca text plus my sentence, and piece 5's line updated to main's
  bare-metal drive on Windows). Studio: red r64 (FAILED as expected), green r64 141/141, merged r89 140/140; Studio
  scratch ~/.cache/g262-kiln-w8 (18 GB) removed. Remote branch agents/g262-microvm-kvm-whp left (main may delete it).
- Filed (run-requests.md, "g262 kiln main da7a00ee6"): kiln clippy on 3 OSes; the KVM `microvm::` drive on adacachy
  (runnable now); the WHP drive on jupiter (after pieces 2, 3, 5).
- LEFT (each its own piece, in order, docs/agentic.md "W8 on Linux KVM and Windows WHP"): 2 Windows' paths (`Seen` by
  path parts; `served_project` by git's prefix `Workspace::sub`; --lib-testable on any host); 3 Windows' known holder
  (`served::SlatesVolumes` keeps the attached handle per box until the run ends, hands Hex a duplicate; cfg(windows),
  main compiles it); 4 `/api/harness/workspaces` reports a served volume too; 5 the `microvm::` drive's gate opened to
  Windows; 6 the runs (main's). Owed on every host: the resync end `SlatesVolumes` drops (A-316/A-427).

## Piece 2 (main's assignment 2026-11-19 ~02:00 UTC): Windows paths, judged as Windows does
- Built: `crates/kiln/src/sandbox/windows_path.rs` (new, pure, any host): prefixes as Git for Windows'
  `normalize_ntpath` (verified in git-for-windows/git compat/mingw.c main e766978a9db5, 2026-11-13: `\??\`, `\\?\`,
  `\DosDevices\`, then `UNC\`) plus `\\.\`; `/` and `\` alike, runs collapsed; `.`/`..` evaluated, never past the
  root; drive letter, share names and every name compared caselessly by BMP simple uppercase (Microsoft Learn "File
  path formats on Windows systems" 2025-10-22, CompareStringOrdinal 2018-12-05, "Naming Files" 2024-08-28).
  `isolated.rs`: `Seen::said` and `served_project` both judge through `beneath` (Windows rule for a top not starting
  `/`, Unix components otherwise). Tests in windows_path.rs: `w8_p2_a_windows_path_lies_in_a_folder_wherever_windows_
  would_open_it_there` (pure) and `w8_p2_a_windows_projects_folders_are_told_as_its_microvm_sees_them_however_windows_
  spells_them` (by use: Seen, harness_cwd, served_project). Docs: agentic.md W8 piece 2 marked built, limits stated
  (8.3 names, links, a `//server/share` top), ledger row W.
- Commit 852f228e9 on kiln main 815a790ce, pushed as branch agents/g262-microvm-kvm-whp-p2 (not main).
- 2026-11-19 02:13 UTC: Studio run NOT started: kiln MCP and CLI both "not connected to the kiln server". Next: when
  it's back, the red (isolated.rs from HEAD~1) / green (`sandbox:: w8_p2_`) run, then merge and push to main.
- 2026-11-19 02:25-02:27 UTC: the runner link flapped (refused r220, r238: "dropped before it answered", "did not
  answer in time"); r238 never started (Studio probe: nothing running). r257 ran.
- LANDED piece 2 on kiln main 852f228e9 (fast-forward from 815a790ce): red r257 FAILED as expected (harness told
  `C:\Users\ada\proj\crates\kiln`), green r257 71/71 (`sandbox::` + `w8_p2_`). Studio scratch (17 GB) removed.
  Filed: clippy on 3 OSes; Windows `--lib sandbox::` run. Branch agents/g262-microvm-kvm-whp-p2 left (main may delete).
- LEFT, in main's order: piece 3 (Windows DAX holder: `served::SlatesVolumes` keeps the attached handle per box until
  the run ends, hands Hex a duplicate; cfg(windows), main compiles), piece 4 (`/api/harness/workspaces` reports served
  volumes), piece 5 (the `microvm::` drive opened to Windows). The resync-end bug is main's own part now.

## Regression first (main 2026-11-19 ~03:00 UTC): macOS microVM drive r134 at 815a790c, Ask agent never said WROTE
- Log ~/.cache/kiln-e2e-microvm.log is on adas-macbook-pro.local, which kiln lists offline (not this MacBook,
  Adas-MacBook-Pro-2; adalundhe-macbook-pro is another Mac without it): not read.
- Read against the ask path: neither suspect reaches it.
  - Mine (64220bcfc in da7a00ee6): the drive opens through /api/harness/sessions with an explicit `isolation`, so
    `WorkspaceUse::Served`; the fixture's macOS runner can mount (nfs) and keeps workspaces, so `workspaces_on` gives
    either way; `Registry::isolation_for` is open_as's old two steps in their order (asked, else the parent's, else a
    microvm runner's new conversation); `prepare` still makes it with no mount (`make_unmounted`), served writable
    (`served_project`: `writable: true`, `serve_shards` Intent::Write); `facts()`'s reorder changes only a kiln that
    keeps no workspaces (the fixture's runner keeps them).
  - g294's (e44819198): it changes `Roots::WebPkiPlusDer` HTTPS clients; the fixture runs plain HTTP
    (`kiln agent --server http://127.0.0.1:<port>`, vm_e2e/mod.rs:1137), and its runner's QUIC link already checked
    the server's name before; the guest's MCP lane is served in the runner's process (`serve_lane`, `Link::Here`).
- The failing step: the stand-in (tests/fixtures/vm-stub/harness/stub, `Turn::run`) writes right after HEARD, before
  any MCP call, and says `WROTE` or `WRITE_FAIL <OSError>`; the drive's wait returned only on `STUB_DONE`
  (`said_marker`), so the turn ended: either the guest's create in its served project failed (WRITE_FAIL), or kiln's
  record of its words lost the line. `agents_land` twice = the stand-in's LAND and its self-approval, so its MCP
  worked. Asked main for the error's `lines` list. Since the last macOS pass (ac2557e10, HVF r1697) the served path
  changed by Slates re-vendors (contracts 19, 23, 36, 41, 42) and the per-mode write policy (b4e859b18, 56d08d3df),
  all before b7720ebca: r296 likely fails the same; ac2557e10 is the bisect's good end.
- 2026-11-19 ~03:30 UTC, main sent r134's line: `WRITE_FAIL [Errno 13]` (EACCES on the guest's create).
  ROOT CAUSE (Slates' side, by code, not kiln's and not the write policy):
  - The policy: Slates applies `WritePolicy` to landings (server/src/landing.rs:499, 506) and submits/merges
    (verbs.rs:5213-5266, 8746-8812) alone; no write path reads it; `authorize_write` (bridge-core volume_bridge.rs)
    refuses a read-only attachment EROFS, never EACCES.
  - On the Shards route the guest's kernel checks permissions (A-314: `FUSE_POSIX_ACL`, bridge-core access.rs: "The FUSE
    mount gets these checks from the kernel"), so an unprivileged run writes its share only once Shards' init names
    the run's owner on the share's root (A-281: SETXATTR `trusted.shards.owner` = `UID:GID`, which the stream takes and
    swaps with the root's owner).
  - A-378 (Slates e418f5cd2, into kiln with 688cb98ce, 2026-11-18 11:51, contract 36: after the last macOS pass
    ac2557e10/r1697) made the guest route negotiate `FUSE_SETXATTR_EXT` (bridge-fuse/src/init.rs `acl_wanted`), so the
    guest's kernel sends the 16-byte `fuse_setxattr_in`; `slates_bridge_fuse::shards_fs::owner_word` still reads an
    8-byte head (`const HEAD: usize = 2 * size_of::<u32>()`, shards_fs.rs:230), called as `owner_word(&request)` at
    crates/server/src/shards.rs:1646 (Slates main 3f02b3ea3: :1647, unfixed) without the stream's `setxattr_ext`, so
    the name it reads begins with the zero `setxattr_flags`: the word goes to the dispatch as an ordinary attribute,
    the owner view stays the identity, the guest sees the host's uid (501 on a Mac) on its project folder, and the
    run's create is refused EACCES. Every agent (not Ask's alone: Bypass runs second); a host whose person's uid equals
    the run's would hide it (hypothesis: maybe why Linux passes). Slates' own dispatch reads the negotiated head
    (bridge.rs:1905, `COMPAT_SETXATTR_IN_BYTES`/`SETXATTR_IN_BYTES`); the owner word is the one sibling.
  - Fix (Slates, needs an A-number): `owner_word(request, extended)` with the head by the stream's form, the call
    passing `stream.setxattr_ext`; failing test first: an extended-form owner word from uid 0 on the root is `Named`.
  - No kiln change for it. Reported to main.

## A-587 (main, 2026-11-19 ~04:00 UTC; UI goal 89 part "kiln's macOS microVM drive green again")
- Slates worktree ~/slates-work/g262-a587 (branch agents/g262-a587-owner-word from Slates main aef6faea1).
- Built: `owner_word(request, extended)` (head by the stream's form); call `owner_word(&request, stream.setxattr_ext)`;
  bridge-fuse unit tests (extended form taken / short form taken) and a server unit test by use (INIT keeping
  SETXATTR_EXT, then the extended word, GETATTR shows the root as 1000:1001); tests/shards_fs.rs via owner_word_short;
  §4.6, GAPS (A-281's entry), amendment A-587, docs/bugs record. Commits 9e5dbd4c6, 74e059a0c (rustfmt's layout from the
  Studio + checks); merged Slates main d4c3d2989 as b84b75948 (log-end conflict: main's A-589 first, A-587 last; main's
  A-584 refactored shards.rs's tests: no name clash).
- Studio r421 (on 9e5dbd4c6 + fmt): red A FAILED (None where Named), red B FAILED (dispatch EIO), green bridge-fuse
  shards_fs 6/6, server shards::tests 2/2, fmt --check clean. r434: the merged tree (pending).
- Next: push Slates main; kiln re-vendor to that commit (contract 48 -> 50: kiln uses neither 49 nor 50; CONTRACT bump
  + doc paragraph), vendor.py check 0 differences, kiln --lib green on the Studio, land kiln.

## A-587 standing (recorded 2026-11-18 ~19:35 PST by the relaunched g262, parked by main; nothing uncommitted anywhere)
- Slates: ON MAIN. 9e5dbd4c6 (fix), 74e059a0c (fmt + checks), merge b84b75948, 38ed3dfd8 (the merge had left
  conflict markers in crates/server/src/shards.rs's test module; both tests kept). The predecessor pushed 38ed3dfd8 to
  main at 19:20 PST (origin/main reflog "update by push"); it is an ancestor of main e85c17f9f; `git grep` finds no
  conflict marker on main. Branch origin/agents/g262-a587-owner-word = 38ed3dfd8. Unit runs: r421 (before the merge)
  red twice, green bridge-fuse shards_fs 6/6 + server shards::tests 2/2; r434 (merged tree b84b75948) refused the
  marker file; NO green run recorded at 38ed3dfd8 itself: filed for main (run-requests.md, "g262 A-587 merged tree").
- Kiln: the re-vendor to Slates 38ed3dfd8 (contract 50) is e4d7f4eae in ~/slates-work/g262-kiln-microvm (branch
  agents/g262-microvm-kvm-whp), pushed ONLY as kiln branch origin/agents/g262-revendor-a587, not kiln main.
  vendor.py check: 1355 files, 0 differences. kiln --lib never run on it.
- Left (when main restarts it): kiln --lib on the Studio at that branch (merge kiln main first), land on kiln main,
  then main's macOS microVM drive rerun (run request "g262 root cause of the macOS microVM drive's EACCES").

## A-605 (main, 2026-11-18 relaunch; UI goal 92 51cef9f7): Subversion piece 9, mergeinfo, in slates-svn
- Started 2026-11-18 ~19:35 PST. Worktree ~/slates-work/g262-a605 (branch agents/g262-a605-mergeinfo).
- LANDED on slates main 6e5fc9819 (pushed 42e2f50ef..6e5fc9819, 2026-11-19 UTC): `crates/svn/src/mergeinfo.rs` +
  `mergeinfo/tests.rs` (new), lib.rs's `mod mergeinfo;`, §4.17.4 status block + piece 9 marked built, GAPS §4.17.4
  sentence, research notes §11 (sources with sha256), amendment A-605 at the log's end. Commits 0762d211a, 4a5f7dff5,
  32aed93d7 (WIP), bacae2720, merge 65dcd0131 (conflicts with main's A-602 locks: design status + piece line + lib.rs
  mod lines, all kept both), 666bfa347, merge 6e5fc9819 (clean).
- Built: the value read as `svn_mergeinfo_parse` (C string to first NUL; last colon divides; `svn_fspath__canonicalize`;
  `svn_revnum_parse` over `svn__strtoul`: digits only, <=10, <=2^31-1; spaces only after the colon; trailing comma refused
  at value end, accepted before newline; duplicate keys merged; refusals typed by svn's message, all E200020; `verify`
  refuses NUL first) and written as `svn_mergeinfo_to_string` (svn_path_compare_paths order); range-list algebra
  TRANSCRIBED from mergeinfo.c (merge sweep, intersect/remove/diff via rangelist_intersect_or_remove +
  combine_with_lastrange, equals) because svn's consider_inheritance walk is inexact per revision (1-10 vs 3-4*,5-6:
  empty intersection, nothing removed -- named test) and the stock client computes --show-revs with it; inheritance,
  elision (should_elide_mergeinfo); queries `mergeinfo_at` / `descendants` (resumable iterator, minfo-cnt pruning) /
  `catalog` over the index with a `Contents` seam (green's listings + property lists); #3896; ClaimsMergeinfo E160004.
- Tests (Mac Studio, `cargo test -p slates-svn --lib`): 111/111 (37 mergeinfo) on the merged tree, 1.21 s; fmt --check
  clean. mergeinfo-test.c vectors (25 valid, 26 broken by kind, 68 merges, 15 removes x2, 16 diffs x2, intersect, merge,
  to_string, 4686/4840, the 62x63 loop + per-revision check); proptests (per-revision model, hostile bytes, cuts/flips,
  round trip); query unit tests; oracle = naive whole-tree FS on 256 generated histories with a census that must move.
  Red-checked 8 mutants, each failing named tests (1, 6, 4, 4, 2, 1, 2, 2). Studio scratch (644 MB) removed.
- OWED (the next piece, A-605 part 2 or a new number): `svn mergeinfo --show-revs` eligible/merged composed from this
  algebra with piece 7's location segments and log (`svn_client__mergeinfo_log`, `filter_log_entry_with_rangelist`);
  log's include-merged-revisions (`log.c` fs_mergeinfo_changed) with piece 7; the property's UTF-8/CR checks with piece
  7's validation. Branch agents/g262-a605-mergeinfo left on origin (main may delete).

## A-613 (main, 2026-11-19; UI goal 93): ra_svn's read commands (piece 7 part 2), then mergeinfo --show-revs
- Worktree ~/slates-work/g262-a613 (branch agents/g262-a613-reads). Registry row A-613 (main wrote it; next free A-615).
- LANDED part 1 on slates main bdeb141ac (pushed 00a568db8..bdeb141ac, 2026-11-19): `crates/svn/src/reads.rs` +
  `reads/tests.rs` (new; lib.rs `mod reads;`): a sans-io `answer(index, repository, session, command, out) -> Outcome`
  (Answered | Close for malformed params, as svnserve drops the connection | Elsewhere for another piece's command);
  `Session { base }` (A-599 hands it over; not imported); `Repository: Contents` seam (revision_properties,
  properties). Commands: get-latest-rev, get-dated-rev (svn_repos_dated_revision + svn_time_from_cstring/APR dates),
  rev-proplist, rev-prop, check-path, stat, get-mergeinfo (A-605 catalog), get-iprops; unknown -> E210001.
- Fixtures `crates/svn/fixtures/reads/` (make-fixtures.py: dump-built repo with fixed UUID/dates, `svnserve -i` behind a
  relay on port 36913, stock svn 1.15.0 sessions + scripted sessions; 28 .bin transcripts, repository.txt,
  sessions.txt, subversion-version.txt, SHA256SUMS). Recorded on the Studio; bytes moved here by a kiln code_run program
  (Studio run -> base64 tar -> env var -> machines.run on this MacBook runner e30189ea...), never retyped. FINDING:
  svnserve's property-list order is APR's per-table seeded hash order (3 recordings, 3 orders): tests compare proplists
  as sets; slates writes name order.
- Also in this landing: main's clippy findings in crates/svn/src/mergeinfo/tests.rs fixed (complexity 14 and 11 -> tables;
  no prop_oneof!; no string slicing; ? / .get in Option/Result bodies; OR pattern -> range; case tables' types named),
  and the lint habits applied to reads (dated search split in steps, byte strings, type alias).
- Tests (Studio, merged tree over main b24a8c826): `cargo test -p slates-svn --lib` 273/273 (reads 7, mergeinfo 37),
  fmt --check clean, fixture sums all OK. Red-checked 6 mutants (each failed named tests). Main moved to 00a568db8 after
  the run: only Cargo.lock +1 (slates-git LFS), slates-svn untouched -> pushed. Studio scratch (1.3 GB) removed.
- LEFT (part 2, in order): get-dir, get-file (with checksum/props/entry props, contents streamed), list, get-locations,
  get-location-segments, get-deleted-rev, log (with include-merged-revisions; log.c fs_mergeinfo_changed), get-file-revs
  (svndiff from extent lists), then `svn mergeinfo --show-revs` eligible/merged (svn_client__mergeinfo_log over A-605's
  algebra + segments + log); each with transcripts from the same recorder (svn-info-file and svn-proplist-inherited
  are already recorded: they ask get-locations and get-dir). Read access by A-606's rules (today every path readable),
  and the auth request a denied anonymous read makes when the session can authenticate.
- LANDED part 2's first slice on slates main 8c2ea42d3 (pushed dbdfc253f..8c2ea42d3, 2026-11-19): `get-dir` (node +
  entry props: committed-rev, stored committed-date, last-author, uuid; dirent fields as parse_dirent_fields; get_dir's
  defaults; dir size 0; "Can't get entries of non-directory" 160016) and `get-locations`
  (svn_repos_trace_node_locations over the index: youngest copy root on the path chain, child over parent on ties,
  fresh/unrelated rejected, copy source + remainder; revisions after the peg kept while the line reaches back to the
  same path; entries, `done`, then success/failure). Session value gained `uuid`; replays take the base from the
  client's URL. Recorder gained get-dir, get-locations, hostile-locations sessions (re-recorded on the Studio, moved by
  kiln code_run; only the 2 hash-order files changed among old ones). Stock svn-info-file and svn-proplist-inherited
  now replay whole.
- Tests (Studio, merged over main 4c7208ab0): `cargo test -p slates-svn --lib` 283/283, fmt --check clean, sums OK;
  3 mutants (dir entry size, no future revisions, epoch date) each red. Main moved to dbdfc253f before the push with no
  slates-svn/manifest change. Studio scratch removed.
- LEFT (A-613 part 2, next slices, in this order): get-file (checksum from the index row's MD5, props + entry props,
  contents streamed as strings then an empty string then a second response; needs a Text seam on the green), list
  (depth, fields, patterns), get-location-segments (svn_repos_node_location_segments), get-deleted-rev
  (svn_repos_deleted_rev's binary search), log with include-merged-revisions (svn_repos_get_logs5; log.c's
  fs_mergeinfo_changed), get-file-revs (svndiff from extent lists; A-500 encoder), then `svn mergeinfo --show-revs`
  (svn_client__mergeinfo_log over A-605's algebra + segments + log); read access through A-606's authz
  (crates/svn/src/authz on main) with svnserve's refusals; each slice with transcripts from the same recorder.
