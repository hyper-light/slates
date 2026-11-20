# g258-winfsp-hardlinks -- progress (rewritten 2026-11-18 ~23:30 UTC from the agent's own context after the ~/.cache wipe)

Role: Claude subagent (implementer) of the slates supervisor. Piece: A-498 (amendments.tsv: "Mercurial's hardlinked
clones and stores byte-identical on WinFsp"), UI goal 85 (17ad60b6), part 01a1f12cc15570268dd4429b477cb5d1 ("A-498:
Mercurial's hardlinked clones and stores byte-identical on WinFsp, and the WinFsp delete/purge hang").
THE RULE read back (restart preamble ~22:10 UTC + coordinator 23:2xZ): one piece, failing test first; compile only
`cargo test -p <crate> --lib <filter>` + `cargo fmt` (Studio or jupiter); no clippy/xtask/xwin/--target/doc builds/live
WinFsp/CI/diag (filed instead); merge at once; END with progress, run requests, 3-line report.

## What landed before (from context)
- A-348 (REQUEST_BOUND 2): main 093fef964. Step 7 goal record: main eb15b2ec0. bench op: 58c8b4f69.
- A-360 WinFsp hard links, slates' half + driver patch files (docs/wip/winfsp-patches): main bd062e9e0. The drive
  declares HardLinks, serves a forwarded link (host.rs op_link, inflight Link), reports NumberOfLinks = nlink (files),
  IndexNumber = ino, tells a file's other names of a change (siblings). Proven on the test-signing VM (adacachy
  `g258-win`, patched driver 2.1.26322.8570a40): CreateHardLinkW ok, `git clone --local` links 7/7 objects.
- Dropped delete fix (a purge's last-handle cleanup carried a delete that was answered Ok without it): main b83a5b0db.
- The delete/purge hang analysis (GAPS entry rewritten: the single receive worker was sending an answer and waited
  inside FltMgr; WinFsp completes answered IRPs inline on the sender, volume.c:1089-1102 -> iop.c:321; hypothesis: a
  minifilter post-op's synchronous I/O on the drive deadlocks the single receiver, A-198; WinFsp's DLL keeps >= 2
  dispatcher threads): main 24f5406fe (deba9cef2). Its three VM runs were filed 19:57Z (lost in the wipe; re-filed now).
- WinFsp fork: ~/slates-work/g258-step-continued-ada-winfsp, branch g258-hard-links-2 (16b57a48, 2708c772) on v2.1.

## Clones
- ~/slates-work/g258-step-continued-ada-hang: branch agents/g258-step-continued-ada-purge-hang (landed, clean).
- ~/slates-work/g258-step-continued-ada-hl, -ada (older, landed, clean).
- VM scripts: adacachy ~/.cache/g258-winvm/xfer (vm-delete-race.ps1, vm-delete-race-outside.ps1, vm-hang-probe2.ps1
  with FltMgr symbols + fltmc; local copies were lost in the wipe). kd dumps hang-kd-1..4 there too.

## A-498 now
- Fact: WinFsp's released driver refuses FileLinkInformation itself (v2.1/v2.2B5/master fileinfo.c), so on CI's stock
  WinFsp no hard link can be made; NumberOfLinks is 1 there. Only the patched driver links (test signing / a signed fork:
  Ada's outward decision, recorded in GAPS A-300 entry).
- The workload harness (crates/conformance/src/workload.rs) states WinFsp's three link-count lines as a difference
  unconditionally (WINFSP_STATED). On the patched driver they stop differing, and the harness then fails ("a stated
  difference no longer differs"). So the Mercurial workload cannot be judged byte-identical on a drive that links.
- Piece: the stated hard-link lines apply only when the drive makes no hard link, learned from the drive itself.

## Next / log
(see below)
- 2026-11-18 ~23:35Z: worktree ~/slates-work/g258-winfsp-hardlinks, branch agents/g258-winfsp-hardlinks (from origin/main
  e3fbf2ee0). Changed: crates/conformance/src/workload.rs (`HardLinks`, `stated_differences(transport, links)`; new test
  `a_drive_that_makes_links_is_held_to_its_hosts_link_counts`), xtask/src/conformance/workloads.rs (`mount_links`
  probe, `LINK_PROBE`, the record's note, `run_one`'s `links`), design (A-300's A-498 note, the A-498 entry), GAPS (the
  WinFsp version-control entry).
- Unit tests on the Mac Studio (kiln, load 9.2-13.1, debug): slates-conformance --lib workload 18/18; red once with the
  rule reversed: the new test `Differs { "a stated difference no longer differs ..." }` where Identical was expected.
  cargo fmt applied (only my xtask file; the Studio's fmt also reformatted main's crates/server/src/shards_route.rs:
  not mine, left alone). xtask NOT compiled by me (rule): filed.
- LANDED main 1d2723e51 (95f1cfac2, 2d1a215f8; merge resolved the design log's end, main's A-512.. first, A-498 last).
  amendments.tsv: A-498 landed. Studio clone and target deleted.
- Filed (run-requests 23:50Z): xtask unit tests + clippy + xtask check; the Windows lanes' workloads (stock: unchanged,
  note "refused"); the workloads on the VM's patched driver (green = mercurial/git-history Identical, note "made");
  re-filed the hang's three VM runs.
- Left: the hang's fix (needs those runs: if confirmed, a receive must not wait for a send, an A-198 amendment); what
  the VM workload run finds; WinFsp contributor agreement / signed driver stays Ada's (A-300/A-360 GAPS).

## A-551 (2026-11-19 ~00:30Z, UI goal 85): B2's Windows half
- Coordinator: CaseSensitiveSearch only for exact volumes; the stored path (A-543's `stored_path`, g079 3cc9ce3f0) as
  WinFsp's normalized name on open; the drive's sharing-violation test (g079's run request 04:10Z, run-requests.md
  ~line 1475). Pure parts with --lib tests (every host); Windows code written by me, compiled by main.
- Worktree ~/slates-work/g258-winfsp-fold, branch agents/g258-winfsp-fold (from origin/main 3c9a37dd2).
- WinFsp's rule (read 2026-11-19): inc/winfsp/winfsp.h 1495-1530 (FspFileSystemGetOpenFileInfo: NormalizedName may
  only differ in case, no terminator, size set to the bytes copied); dll/fsop.c 444-470/504-519 (the DLL points
  NormalizedName into the response, size RSP_BUFFER_SIZEMAX; taken only if <= TRANSACT_PATH_SIZEMAX 2048 bytes);
  sys/create.c 1035-1065 (only without CaseSensitiveSearch; none = upper-cased; a name not equal ignoring case refuses
  the open STATUS_OBJECT_NAME_INVALID); sys/util.c 1530-1567 (equal = same length, each unit upcased alike,
  RtlCompareUnicodeString(TRUE) past ASCII). slates' Fold = NFC + full lowercase, so a stored name can differ in length
  from the asked one (NFD vs NFC): then no normalized name (the driver's upper-case default), never a refused open.

## A-551 restart (g258 restarted 2026-11-19 ~06:xxZ; predecessor wrote no code, worktree clean)
- Readback of THE RULE sent to main. Worktree ~/slates-work/g258-winfsp-fold fast-forwarded to origin/main a613562eb.
- WinFsp facts read (v2.1 clone ~/slates-work/g258-step-continued-ada-winfsp):
  - winfsp.h 1287-1299: FspFileSystemNotify needs NORMALIZED names = the ones answered on Create/Open (or upper case
    when none are answered). So OpenNames/ledger must keep exactly the answered name.
  - device.c 750-758: the open-node table compares case-insensitively without CaseSensitiveSearch; its comment:
    node names "are now always normalized". file.c 1573-1640 FspFileNodeRename: descendants by case-insensitive prefix,
    renamed NewFileName + own suffix. fileinfo.c (SetRenameInformation, ~1813-1862): NewFileName = target dir NODE
    name (or source's parent part) + last component as typed. volume.rs 2180: a folding volume respells on case-only rename.
  - fsop.c 413 after 468: an answered name AND an open descriptor -> Response->Size counts the name twice (N stale
    packet bytes sent) -> clear them in inflight::finish_response.
- DESIGN: pure module crate::normalized (NameSearch {CaseSensitive, CaseInsensitive{upcase: fn(u16)->u16}}, NodeName,
  node_name(names, asked, stored)): each prefix spelled as Windows already holds it (OpenNames index by upcased key,
  at or beneath), else stored component when the driver takes it (same units, each upcased alike), else asked;
  > 1024 units: not answered, record upcase(asked) (driver's default). OpenNames::searching(search) + by_key index +
  spelled_at(key). Windows: system_upcase = ntdll RtlUpcaseUnicodeChar; Mounted::new flags by policy; Reply::Opened
  carries the normalized units; finish_open writes FSP_FSCTL_OPEN_FILE_INFO.NormalizedName; server passes policy.
- Residuals to state in GAPS: names the volume folds but Windows does not (NFD/NFC, Kelvin): answered as asked;
  paths > 1024 units upper-cased by the driver; Windows-equal but volume-distinct names (final sigma) share a node (F3).
- 2026-11-19 ~02:00-02:40Z: CODE DONE on branch agents/g258-winfsp-fold: 53cbb2d32 (A-551 code + docs), 238c2123d
  (evidence + rustfmt layout), cac7fff03 (merge origin/main: host.rs conflict with A-560's spin_ceiling, both kept;
  design log end, A-551 last), 8a63cb478 (unsafe-budget restored to main's + A-551's entry: my comment-reflow script had
  rewrapped other entries; fixed). Studio unit run at 53cbb2d (load 20.6-25.7): red 9/43 with today's rules restored in a
  copy, green 43/43, whole bridge-winfsp lib 96/96; fmt output applied byte-identically (blob hashes matched).
  Post-merge Studio re-run attempted 02:4xZ: kiln "not connected to the kiln server" (twice); retrying.
- Windows-only, NOT compiled by me (filed for main): host.rs (system_upcase via ntdll RtlUpcaseUnicodeChar, search_of,
  node_name_of, finish_open/put_normalized_name, Mounted::new flags, Ready::search), dispatcher.rs doc, ffi.rs
  OpenFileInfo (+const asserts, compiled only on Windows since ffi is cfg(windows)), tests/support/host.rs calls,
  server/src/winfsp.rs (name_search, defer_attach, adopt_mount, keep), cli/tests/winfsp_semantics.rs (2 drive tests).
- 2026-11-19 02:15Z: A-551 LANDED main 8cb6bef1a (f8ba01f61..8cb6bef1a; commits 53cbb2d32, 238c2123d, 8a63cb478, merges
  cac7fff03 + 8cb6bef1a; `git diff --stat origin/main HEAD` = my 15 files alone). amendments.tsv: A-551 landed.
  Run request filed (run-requests.md 02:15Z): Command 0 merged-tree `cargo test -p slates-bridge-winfsp --lib` (kiln was
  "not connected" 02:05-02:15Z, so not rerun after the merges; main's merged modules touch none of A-551's items);
  Command 1 Windows build/clippy/xtask unsafe (159); Command 2 the two drive tests + WinFsp suites.
- Left (not mine to do unasked): F3 (ς/σ: Windows-equal, volume-distinct names share one WinFsp node) needs a decision:
  refuse such a twin on a volume served by a folding drive, or refuse the second open; GAPS records it. Lesson: my
  comment-reflow helper treated unsafe-budget.toml's whole `#` run as one paragraph and rewrapped others' entries
  (caught in the diff stat, fixed in 8a63cb478 before landing): never reflow a TOML comment block by script.
- Studio scratch clones and targets deleted after each run (~/.cache/g258-a551).

## A-576 (2026-11-19 ~02:20-02:55Z, UI goal 85): a folding WinFsp drive's volume folds by Windows' upcase table
- Coordinator's decision (from principle): refuse the Windows-alike twin at create (ς beside σ), never at the second open.
- LANDED part 1 main 520a86184 (f17565e05..520a86184): bc87adb03 code (vfs NameEquivalence::Upcase + Folded::Upcase;
  crates/vfs/src/upcase.rs = Microsoft's recommended exFAT up-case table, spec §7.2.5.1 Table 25, 2,918 compressed
  entries, checksum E619D30Dh verified, 874 mappings, only Σ<-ς,σ is many-to-one, nothing maps into ASCII; export id 2;
  PolicyImage::Upcase appended; image folded index; server/image.rs refuses an Upcase archive Unsupported until part 2;
  bridge-winfsp host.rs Windows-only test vs RtlUpcaseUnicodeChar; normalized.rs tests on the real table), 1774da0a2
  test fix, 1d71032f2 evidence/docs/rustfmt (design §4.5, §4.6, A-576 entry; GAPS third case decided + part 2 owed).
- Studio: red 5/10 with the table removed in a copy, green 10/10; merged tree vfs 98/98 (1 ign), bridge-winfsp 100/100,
  server image:: 2/2, fmt --check clean. rustfmt'd files byte-identical to the Studio's (blob hashes).
- Run request filed 02:55Z: the Windows table check (`-- upcase_table`) + xwin/clippy/xtask.
- Owed (part 2, not started): NamePolicy/DbNamePolicy appended + wire_order publish, CLI/SDK/MCP selection, archive
  import, and what a WinFsp mount of a Fold (APFS) volume does; if the Windows check finds differences, take the
  system's table at mount (learned per deployment) instead of the published one.
- Lessons: zsh doesn't word-split unquoted vars; a TOML `#` run is one paragraph to a reflow script (never reflow
  others' comment blocks by script); bring Studio rustfmt output back by blob hash + small diffs, not by long base64.

## A-582 part 3 (UI goal 92, 51cef9f7; relaunched 2026-11-19 ~03:30Z after the usage-limit stop): pack entry headers + git deltas
- THE RULE read back to main (SendMessage) first. Predecessor left untracked crates/git/src/{entry,delta}.rs (no tests,
  no lib.rs mod lines, its own ObjectFormat); backed up to ~/.local/state/slates-fanout/scratch-g258-a582p3/.
- Worktree ~/slates-work/g258-git-delta, branch agents/g258-git-delta, fast-forwarded to origin/main e85c17f9f (A-600).
- Plan: entry.rs on A-600's object_name (Kind/ObjectFormat/ObjectId; RefDelta base = ObjectId; size charged to the
  admission when whole); delta.rs (try_reserve_exact, typed refusal); tests/entry.rs + tests/delta.rs; golden vectors =
  ofs.pack/ref.pack read at verify-pack's offsets, headers written back byte-identical; the two inflated deltas captured
  by fixtures/make-delta-fixtures.sh (python3 zlib over the committed packs, run on the Studio) and tied to each pack
  entry by its zlib Adler-32 trailer; blobs rebuilt from make-fixtures.sh's recipe, named via object_name::name_of.
  No inflater added (the design leaves that to piece 3, chosen by measurement).
- Coordinator note (g293, A-591 idx v2 + pack header): g293's `pack::HEADER_LEN` = my `entry::PACK_HEADER_LEN` (12);
  whoever lands second points at the other's. At my merge: if origin/main has crate::pack::HEADER_LEN, entry.rs uses
  it and drops its own. No CRC needed here (crc32fast is g293's; my test's Adler-32 is test code, no crate).
- LANDED main 3a8164117 (e85c17f9f..3a8164117, fast-forward; a43190224 code+tests+script WIP, 2cadfe303 captured
  deltas + test fix + rustfmt, 3a8164117 records). `git diff --stat origin/main HEAD` = my 13 files, insertions only.
  entry.rs (Representation::{Whole(Kind), OfsDelta{base}, RefDelta{ObjectId}}, read/write/written_len, size charged once
  whole), delta.rs (sizes, Instructions, apply with try_reserve_exact + AllocationRefused, apply_to, write_*),
  tests/{entry,delta,pack_fixtures}.rs, fixtures/make-delta-fixtures.sh + deltas/ (Python 3.9.6 zlib 1.2.12 on the
  Studio; SHA-256 e18d3bcd.. 41debd46, bb09d70c.. 671f3c00; tied to both packs by zlib Adler-32), .gitattributes line,
  design §4.17.2 status + A-582 entry "Part 3", GAPS paragraph.
- Studio runs (kiln, d1323898, debug): run 1 (load 2.0) 42/43 -- my own test's reserved-byte delta was 3 bytes (TooShort
  first), fixed; run 2 at 2cadfe303 (load 2.9-7.5) 44/44, fmt --check clean. Red five ways, all real: size bound, base
  bound, reserved byte, target charge, a captured byte (Adler 896992619 vs 897058156). Studio scratch deleted.
- Git's deltas decoded by hand: 671f3c00 = copy(0,323) copy(865,53) copy(380,10524); 41debd46 = copy(0,5342)
  insert "100" copy(5793,54) copy(5407,5493).
- Filed run request (run-requests.md, 2026-11-19 g258 main 3a8164117): clippy x3, xtask check, cargo doc + xwin doc.
- Left / for main: A-591 (g293) points pack::HEADER_LEN at entry::PACK_HEADER_LEN (one constant); lib.rs's crate doc
  still calls deltas "next parts" (shared line); amendments.tsv A-582 row left to main (multi-part).
- Lessons: a hand-built test delta must itself be >= git's 4 bytes or it tests TooShort; a red check that fails for
  the same reason green failed is not a red check (redo it after the fix).

## A-608 (UI goal 93, registered to g258 2026-11-19): git piece 3, the pack reader, in slates-git (`unpack` module)
- Coordinator's brief: streaming + bounded (admission first), inflate via flate2 (workspace zlib-rs, A-607 9bbedc2d8;
  never pick a backend), exact size (longer/shorter/unending refused), OFS by offset + REF via an "id to offset" trait
  (g293's idx v2, A-591, implements it), chain depth by admission, cycle refused, trailer checked, golden = captured
  packs whole vs verify-pack (type, size, id via object_name), tests hold inflated content + ids only, hostile: cuts,
  flips, missing base, looping chain. Failing tests first.
- Worktree ~/slates-work/g258-pack-reader, branch agents/g258-pack-reader from origin/main 9bbedc2d8. flate2 has no
  user yet: my piece adds its lock entries (resolve on the Studio, bring back only the new packages).
- Design: phase 1 streams (pack header -> entries -> trailer), OFS/indexed REF bases applied at once, others wait;
  phase 2 at finish resolves waiting deltas by sorted keys (Entry index | Id) with an explicit stack; leftovers
  classified (DeltaBaseMissing / Cycle). Exhaustive cuts/flips on a small built pack; edge cuts on the captured ones.
- Owed elsewhere: duplicate objects (git --strict "appears twice") where the index is built (piece 5); thin-pack bases
  from the repository (piece 5/6).
- LANDED main d9ef6bf9f (dbd6d0d92..d9ef6bf9f): 095f8cfe9 tests + stub (red r562: 20 of 20 new failing, 76 others ok;
  the lock resolved there), f6d1b979b reader + lock (flate2 1.1.11, zlib-rs 0.6.8; windows-sys re-pointings left out,
  --locked accepted), 4febf4ac5 records, ed9b240f4 merge (A-582 part 2/v2, A-590/objects, A-581, A-594... first),
  the merged-tree count recorded + GAPS rewrap (two docs commits), d9ef6bf9f merge (A-598, A-603, A-609 first). `git diff --stat origin/main HEAD` = 9 files, mine.
- Studio: r581 96/96 --locked + fmt clean + red 4 ways (past-size guard: a long stream silently cut to its size;
  trailer comparison; misplaced-base check; header's record charge); merged tree 162/162 --locked, fmt clean, scratch
  deleted. amendments.tsv A-608 -> landed. Run request filed (clippy x3, xtask check, cargo doc + xwin doc).
- Found and fixed before landing: read() stopped when a stream ended on a call that took no bytes (would have
  misreported the trailer as trailing bytes); output past an entry's size is judged before the stream's own error.
- Owed (recorded in A-608 and GAPS): thin-pack bases from the repository and objects too large to hold (pieces 5/6);
  duplicates refused where the repository's index is built (piece 5).

## A-620 (UI goal 93; registered by g258, next free now A-621): git piece 6, receive-pack, part 1
- Worktree ~/slates-work/g258-receive-pack, branch agents/g258-receive-pack. LANDED part 1 on main a20c44ff4
  (0b09e3ce9..a20c44ff4): 0386e84b1 (code, tests, fixtures), 844d11bd2 (connectivity reads' scratch admission),
  856559140 + merge (A-604 v0, A-615 refname, A-614 store), 3fa3fc5f5 (advertisement via v0::Advertisement,
  refname_ok via refname::is_valid), records, landing merge. `git diff --stat origin/main HEAD` = my 51 files.
- Fixtures: fixtures/push/make-push-fixtures.sh run on the MacBook (git 2.50.1 Apple Git-155): 9 pushes (create,
  update thin, tag, copy empty pack, side, delete, options, non-ff refused, atomic), second run byte-identical,
  SHA256SUMS. Observed: send-pack writes commands with no LF and caps after NUL with a leading space; rp_error lines go
  on band 2 before the band-1 report; outer flush after.
- Studio: r712 red (16 new failing on stubs, 178 others ok); first green caught the scratch-admission flaw (fixed);
  194/194 + fmt + red 5 ways; merged tree 287/287. Lesson: restoring a mutated file with mv keeps its old mtime and
  cargo reuses the stale (stubbed) build -- touch restored files (the first "green" was the stub's).
- amendments.tsv A-620 updated; run request filed (clippy x3, xtask check, cargo doc + xwin doc).
- Left (part 2 and later): each ref a work judged by the engine (A-618) as one A-603 entry, A-614's store adapted to
  receive::Repository (a base's bytes from the content plane); fsck of new objects (A-590); the server loop over one
  connection; refs/for/ review requests (v2 option lines), hidden refs, push-cert.
- Studio leftover not mine to judge: ~/.cache/g258-step-continued-ada-src (an older g258 WinFsp clone), left in place.

## A-620 continuation (UI goal 93; main's ask after a20c44ff4): fsck, hidden refs, session, verdict/append seam
- LANDED main 7896dbda9 (ad75339d3..7896dbda9): 5adf7d458 code+tests+fixtures, fce0702e5 (A-638's views test to the new
  seam, prop_oneof! out of tests/entry.rs and tests/delta.rs, fed_in_chunks refuses, GAPS markers), 6914b09be rustfmt
  (r877's nine files brought back by blob hash: the Studio has no GitHub push credentials), 8223955a6 records, merges.
- What: receive::fsck (A-590 Strict, pack order, before refs move; band 2 `error: object <id>: <finding>` + `fatal:`);
  unpack words by git's unpack_limit 100 (part 1 had index-pack for every short pack -- wrong under 100 objects);
  HiddenRefs predicate (advertisement filtered by `visible`, refusals after connectivity, git's words); Session (sans-io
  input/end/advertise; refusal ends it); Judge split per A-642: judge (pure, type Accepted) -> append (one ref, own log)
  / append_atomic (all or none); FAILED_TO_UPDATE, ATOMIC_TRANSACTION_FAILED. Fixtures: hidden + fsck pushes (11 total).
- Corrected on the way: Repository::holds doc -- git's --exclude-hidden=receive is a walk-cost change, objects only
  hidden refs reach still count as held.
- Studio: r866 compile stop (A-638's test on the old seam), r877 349/349 + red 17/31 on stubs + 10 mutations caught,
  fmt; merged tree 354/354 (load 31.2). Main moved 6 then 16 commits during the landing (A-643 merge engine, lfs):
  merged twice, no crates/git interaction; a push issued beside its check was refused non-ff (harmless).
- Registry A-620 status updated (not the next-free line). Run request filed.
- Studio scratch kept for part 2: ~/.cache/g258-a620b-{src,target,logs,orig} (delete at END). Local scratch:
  ~/.local/state/slates-fanout/scratch-g258-a620/ (reflow.py, fmt_r877.py), ~/.cache/g258-gitsrc.
- Next: part 2 (main's 2nd note): Judge over the engine -- ref -> green, A-618 build over A-638's PushedObjects, judged
  pure (A-645), several under atomic (A-635 when it lands), append per A-642 through a trait (g261's append later).
- Lessons: BSD grep's `\|` alternation silently drops a branch -- use grep -E for marker checks; never issue a push in
  the same batch as the check that gates it; a Studio clone cannot push (no credentials): fmt comes back by blob hash.
- PART 2 PLAN (2026-11-19, after reading §4.17.1 "A push is a work" items 1-5, §4.17.2 "A push" items 5-7, A-642,
  A-645, A-618 push.rs, A-638 views.rs and tests, A-614 store Repository::{admit, commit, head, tree, versions}):
  - new `receive/works.rs`: `Works<G: Greens, A: Appends, O: PushObjects>` implements `Judge` (no new crate dep:
    slates-git already depends on slates-merge). Fields: greens, repository (receive::Repository for commit bytes),
    objects (A-638 PushedObjects over the sealed quarantine), appends, push name [u8;32], room.
  - `Greens` (read side, immutable at the push's read): read(name) -> Option<{id, position, &Green}>;
    version_of(green, commit) -> Option<(version, tree)> (A-614: Repository::head/tree per version).
  - judge: update = base from version_of(old) (none -> FAILED_TO_UPDATE, "a ref that moved", before any work), tree of
    `new` (commit from Pushed then Repository), slates_merge::push::build(Push{name, base_version, base_tree, tree,
    head: Some(new)}, green, objects, room) -> Accepted::Work{green, position, increment}; create = build against
    Green::new() -> Accepted::Create{name, increment}; delete = Accepted::Retire{green, position} (old must be head).
    PushRefused -> Declined in slates' own words (typed, one per variant).
  - `Appends` (A-642 stand-in until g261): append(entry) -> Result<version, Unappended{Moved|Conflict}> ;
    append_atomic(&[entry]) all or none. Head conflict (MetaMeta) and moved -> FAILED_TO_UPDATE /
    ATOMIC_TRANSACTION_FAILED (§4.17.1 item 5: git's words for a ref that moved).
  - Store side (Repository::admit before verdict, ::commit after) belongs in the append adapter with the engine's.
  - Owed after 2a: policy checks (A-622's check point: ff, deletes, linear, signatures, evidence), hooks, A-635's
    several-works verdict, refs/for/ review requests, `.have`.
  - Tests: a test double over real engine Greens + version map; reuse A-638 test helpers' shape (store, Push quarantine,
    tree_bytes, commit_bytes); red first against a stub Works.
- PART 2 PIECE 1 LANDED main 7d0550ae6 (b3e1c0ba4..7d0550ae6): outcomes() -> check() + decide() with `Checked`
  (request, unpacked, early refusals, private by_id table charged once); Session::new(advertised, hidden);
  input(bytes, repository, admission) -> Ok(bool checked); end(repository, admission); checked(); answer(judge, out,
  admission) -> Option<Received>. 377/377 on the Studio, red 4 ways (end discarding a checked push survived first: a
  guard added). Two pushes to main were refused non-ff by main's pace; merge-then-push immediately works.
- NEXT: part 2 piece 2 = receive/works.rs per the PART 2 PLAN above; the caller (server) admits Checked::objects() to a
  store::Push quarantine and seals it between Session::input -> true and Session::answer(&mut works, ...).
- PART 2 PIECE 2 LANDED main 80def213e (1af538442..80def213e): receive::works::{Works (Judge), Greens, GreenRead,
  Version, Entry {Work, Create, Retire}, Appends, Unappended {Moved{at}, Conflict{at, windows}}} + slates' words consts
  (NAME_NOT_HELD, DUPLICATE_NAME, TARGET_NOT_HELD, KIND_CHANGED, PAST_ROOM, UNPLACED, NO_TREE). 386/386, red 6/7 on a
  stubbed judge + 5 mutations. A-637 removed PushRefused::Submodule mid-piece (submodule links are served nodes now).
- Main now exports g261's pure verdict (slates_merge::{Judged, Judgment, HeadMoved}, engine judge_tests.rs): the next
  piece can compute the engine's verdict in Works::judge and append by position through it (A-642/A-645).
- OWED (recorded in A-620's entry and GAPS): the server's push path (caller admits Checked::objects to a store::Push
  quarantine, seals, answers through Works with g261's append + A-614 Repository::admit/commit); A-635 several works;
  A-622 policy checks + hooks; refs/for/, push-cert, .have; band-2 warnings; A-590 blob checks; tag of a tag.

## A-654 (UI goal 93; registered "assigned" to g258): git's push end to end in the server -- STARTED, nothing landed
- Worktree ~/slates-work/g258-git-push, branch agents/g258-git-push from origin/main ae8a9e38a (created with
  `git worktree add -b`; nothing committed yet).
- Main's brief: Checked::objects admitted to A-614's quarantine between check and answer; Works answered through
  A-635's `crate::repository::push::RepositoryLogs::append(&mut self, principal, repository: RepositoryId,
  changes: Vec<BranchChange>, parts: EntryParts) -> Result<Appended, AppendRefused>` (one change = plain push, all =
  atomic); A-614's Repository::commit beside it; vcs_policy::admit (A-622) before the verdict; hook driver at
  pre-receive, update, post-receive; Moved and Conflict in git's words; ONE entry point for g299 (HTTPS) and g294 (SSH);
  tell main its signature.
- API facts read (2026-11-19):
  - crates/server/src/repository/push.rs: BranchChange{Update{green: DbVolumeId, work: Increment}, Create{name: String,
    from: DbVolumeId, version: u64, work: Option<Increment>}, Delete{name: String, version: u64}}; EntryParts{checks,
    names, state} (Default); Appended{changes: Vec<BranchAppended{Moved{green, version, before}, Created{green, version},
    Deleted{green}}>, recorded}; AppendRefused{Repository(Refusal), Changes(Vec<ChangeRefused{index, why:
    ChangeRefusal{Conflict{windows}, Moved{head}, Twice, ApprovalRequired{identity}, Refused(Refusal)}}>),
    Entry(Refusal: Database{Moved} names the green, CheckFailed, StateChanged, TableAtCapacity)}. `impl RepositoryLogs
    for ShardState`. Uses Green::judge / commit_judged (g261's Judged/Judgment exported by slates_merge).
  - NOTE Create is "made at version of member `from`" (a branch from another branch without a walk) + optional work
    based at that version: receive::works::Entry::Create (built over an EMPTY green) must map to it: pick `from` = the
    green whose head is the new commit's parent/any member? -> design question for the piece: a git create of a NEW
    branch at a commit = Create{from: the green holding that commit's version (e.g. the default branch), version, work:
    the delta}; with no such member, an empty repository's first branch (no `from`) is not expressible -> check
    repository.rs for a first-branch verb.
  - crates/server/src/vcs_policy.rs:838 `admit(record: &RepositoryRecord, principal: &Principal, asked: &[Asked{name:
    Name::Ref(&[u8]), right: Right}]) -> Result<(), PushRefusal{Denied{index, rule}, NotListed{index},
    NotPermitted{index}, OtherTool{index}, Unreadable{rule}}>`; Right::{Read, Submit, Rewrite, Delete}.
  - crates/server/src/vcs_hooks.rs: git_pre_receive(view: PendingView, updates...), git_update(view: PendingView,
    update: &RefUpdate), git_post_receive(...) build HookInput; crates/server/src/hook_driver.rs runs one configured hook
    as a process over a read-only view ("git's bare repository with the quarantine" -- the view is the git server's to
    make; check how PendingView(u64)/CommittedView(u64) ids are resolved before promising hooks).
  - crates/server/src/vcs_http/git.rs: GitServices{type Exchange; fn start(&mut self, &Start{repository: &RepositoryRecord,
    principal, service, version, advertise}) -> Result<Exchange, ServiceRefused>}; Exchange{async body(run), async
    body_end(), async reply(out) -> Result<bool, Failed>} -- g299 plugs receive-pack in here.
  - The server holds NO git object store yet (no slates_vcs::ContentStore, no slates_git::store::Repository in
    crates/server; slates-vcs not a server dependency): A-654 must bring A-614's per-repository store onto the owner
    shard (budget/window derived, R3; recovery: decide where the store's durability comes from -- read §4.17.2 piece 5
    and A-581/A-614 status before coding).
- PROPOSED ENTRY POINT (sent to main): a sans-io driver both edges adapt, in a new crates/server/src/git_push.rs:
    pub(crate) struct ReceivePack { session: receive::Session, quarantine: store::Push, repository: RepositoryId, ... }
    ReceivePack::start(repository: &RepositoryRecord, principal: &Principal, advertise: bool, out: &mut Vec<u8>)
      -> Result<ReceivePack, PushFailed>      // writes the advertisement (refs filtered by the policy's hidden refs)
    ReceivePack::input(&mut self, bytes: &[u8]) -> Result<(), PushFailed>   // runs session.input on the owner shard
    ReceivePack::end(&mut self) -> Result<(), PushFailed>
    async ReceivePack::answer(&mut self, out: &mut Vec<u8>) -> Result<(), PushFailed>
      // quarantine admit+seal, policy admit, pre-receive + update hooks (awaits), Works verdict, A-635 append +
      // A-614 commit in ONE with_state step, post-receive hook, report-status written to `out`
  HTTP: Exchange::body -> input, body_end -> end + answer, reply drains `out`. SSH: data -> input, EOF -> end, answer.
- Next steps when relaunched: read §4.17.2 "Pieces (git)" 5-6 + A-614/A-581 status for the store's home and durability;
  repository.rs for a first-branch create; hook views; then failing tests first (an in-process push end to end against
  ShardState with a test hook configured), then the driver.

## g258 relaunch 2026-11-19 (UI goal 93): A-662 first, then A-654
- Worktree ~/slates-work/g258-git-push (branch agents/g258-git-push), merged origin/main at 018d32ba2 (has A-647 parts 1-2).
- Main's decisions: (1) first branch = Update{green: root} (unborn ref = root green with no git head), new branch =
  Create{from: parent's (green, version) if branchable else default head}; (2) A-662 store home + durability;
  (3) hooks through hook_driver's views, refuse typed when no view; piece 10's bare view owed in GAPS.
- Main's addition: each repository's entry in my map holds its tool store AND its `slates_vcs::derived::Derivations`
  (A-647 part 2's `store::ReachSets`, kept with `reach_value` + `Derivations::keep` after each commit), made, charged,
  freed and recovered together. No separate home for derivations.
- A-662 PLAN (facts read 2026-11-19):
  - db: `RepositoryEntry.objects: Vec<u8>` (opaque record, the tool's); `Register.objects: Vec<Vec<u8>>` in apply
    order, NOT a positioned log (no new atomic join, A-642 holds); `bytes()`/`record()`/`restored()`;
    `RegisterRecord.objects`; `entry::bytes` counts it (merge budget, `Capacity` past it); destroy releases it;
    `Repositories::objects(repository)`. Schema change is refused typed at log read (record.rs "schema mismatch").
  - server `crates/server/src/vcs_store.rs` (+ `vcs_store/record.rs`, `vcs_store/tests.rs`): `VcsStores { content:
    Option<ContentStore> (window = state.store.content.chunk_bytes()), stores: BTreeMap<RepositoryId, ToolStore>,
    charged }`; `ToolStore::Git(GitStore{repository, derivations, reach: ReachSets eager})`, `Mercurial(HgIndex,
    Derivations)`, Subversion none. Budget = green-retention pattern: `state.store.make_room` +
    `state.store.budget.charge_retention` before growth, true up to `content.used()` after (credit_retention).
  - green key in the git store: `u64::from_be_bytes(id.bytes[8..16])` (partition + counter, unique per partition,
    `verbs::fresh_volume_id`).
  - record (git): magic+version, deepest, moves [(green key, Option<ObjectId>)], objects NEW to the repository in
    admission order [(kind number, ContentId = content.object_id(bytes), len, bytes)]; replay checks identity
    before admitting (`Corrupt` typed). `locate` resolves the index before the push, so omitting held objects replays
    identically; positions are (type, id) ordered, independent of admission order.
  - commit step: `EntryParts.objects: Option<Sealed>` -> entry.objects = record before `check_creating`; store commit
    + reach keep between the db check and the greens' commit; record failure -> rebuild the store from the log.
  - recovery: `rebuild_recovered` after members: `vcs_store::recover(state, slices)` a record a unit.
  - fragments: derived, clock+exchange dependent; NOT in commit/replay; owed with A-654's after-step (GAPS).
  - oracle: generated pushes against the store, recover at generated points, compare positions, rows, D(k),
    reached, heads, kept reach values; plus ShardState wiring test through the append.
- 2026-11-19 A-662 WRITTEN, NOT YET COMPILED (Studio d1323898 offline from ~07:40Z): branch agents/g258-git-push at
  74c67e956 (pushed to origin), one wip commit over main 018d32ba2. Files: crates/server/src/vcs_store.rs (+record.rs,
  tests.rs), db catalog/repository/entry(+tests), push.rs hooks (EntryParts::sealed; store commit between check_creating
  and the verdicts' commit; rebuild on a later failure; abandon otherwise), install_root -> make, rebuild_recovered ->
  recover, lfs.rs `sealed: None`, Cargo (slates-hg, slates-svn workspace deps; server deps; lock list). Docs: §4.17.1
  status + piece 3 line, A-662 entry (Evidence PENDING), GAPS paragraph after A-642's.
- Run owed (Studio, CARGO_BUILD_JOBS=4, one cargo at a time, own target, delete after):
  `git clone -q --depth 1 -b agents/g258-git-push https://github.com/hyper-light/slates.git ~/.cache/g258-a662 &&
   cd ~/.cache/g258-a662 && CARGO_TARGET_DIR=~/.cache/g258-a662-target CARGO_BUILD_JOBS=4 cargo test --locked
   -p slates-db --lib repository:: && ... -p slates-server --lib vcs_store:: repository:: lfs::` then cargo fmt diff.
  Red-first: stub replay_git -> Ok(0) / record.objects not kept -> oracle + db test must fail (red-check on a copy).
- A-662 reshaped (f34d583c6, pushed): SealedPush kept by its caller; `repository::push::push_sealed(state, (principal,
  repository), changes, parts, &mut SealedPush)`; record written per attempt (`SealedPush::record`), objects part cleared
  only on commit; `reseal` for the next ref of a non-atomic push. EntryParts/lfs.rs untouched vs main.
- LANDING RULE (main): fresh `git worktree add -b` from origin/main, cherry-pick my commits, give them real area-prefixed
  messages with measured facts (no "wip" may reach main), merge, push; never force the old branch.
- A-654 DESIGN (decided 2026-11-19, worktree ~/slates-work/g258-a654 branch agents/g258-a654 from f34d583c6):
  1. receive::works (slates-git, A-620 mine): `Greens` gains the base of a create (first parent's branchable
     (green, version), else the default branch's head, else none = empty repository/unborn); `Entry::Create` gains
     `from: Option<(u64 green, u64 version)>` and its work is built against that version's tree (main's ruling 1).
  2. names: a namespace name starting `refs/` is a full ref; any other N is `refs/heads/N` (bijective). Unborn = a
     green with no git head in the store: not advertised; a create of its name maps to Update{green: root}.
  3. version_of(green, head): GitStore keeps per key the green's version at each store chain index; the record's moves
     carry that version (record format 2, A-654; no v1 record exists in production since nothing calls push_sealed
     before A-654). Delete moves carry the green's version unchanged.
  4. creates' store keys are minted inside the append (plan_create): SealedPush moves target Green(id) | Created(name);
     push.rs resolves created names to ids after `make()` and before `entry()`/`record()`, reseals, then the guard.
  5. driver crates/server/src/git_push.rs: ReceivePack::{start(advertise), input, end, answer}; Greens + Appends impls
     over ShardState and the store; vcs_policy::admit before the verdict; hooks via hook_driver (refuse typed when no
     view: piece 10's bare view owed); atomic = one push_sealed for all, else one per ref (reseal between);
     report-status v1/v2 from receive's report writer.
- 2026-11-19 later: A-662 FOLDED final API (70d7b0e02 on agents/g258-git-push, pushed; 3 wip commits over main 018d32ba2):
  record moves = (key, green version, head); GitStore.versions + version_of(head) newest first; Target::{Green,
  Created(name)} resolved inside push_sealed after make(); GitPush::seal(state) seals generations only;
  SealedPush::admit(stores, resolved moves) per append; push_sealed(state, (principal, repo), changes, parts,
  (&mut sealed, &[StoreMove])). Tests updated + create-keyed test. Still uncompiled (Studio offline all session).
- A-654 step 1 committed a057caed9 on agents/g258-a654 (worktree ~/slates-work/g258-a654, from f34d583c6; MERGE
  agents/g258-git-push into it before continuing): receive::works create base (Greens::base_of_create, Entry::Create
  { from }), tests updated + orphan create.
- A-654 FINDING: crate::greens::Greens::get lends a member as GreenRef::Member(Lent<RefMut<Lineage>>): one member of a
  lineage at a time, the lend dropped at the end of the call, so receive::works::Greens::read cannot return
  GreenRead { green: &Green } from the server. Change the trait (A-620, mine) to a closure lend:
  `fn with_read<R>(&self, name, f: impl FnOnce(GreenRead<'_>) -> R) -> Option<R>` (and the same for base_of_create),
  Works building each work inside it. Then the server's ShardGreens over &ShardState + the git store (key -> green id
  from members_of), Appends mapping Entry::{Work, Create{from}, Retire} to BranchChange::{Update, Create, Delete} and
  StoreMove targets (unborn default name -> Update{root}), atomic = one push_sealed, else one per ref.
- NEXT when the Studio answers: build A-662 (the run line above, CARGO_BUILD_JOBS=4), fix, red-check (stub the replay /
  drop the register's objects), fill the A-662 entry's Evidence, land per main's rule (fresh worktree from origin/main,
  cherry-pick -n the three commits, one real message with the counts), report the signature; then A-654.
- A-654 step 2 committed e9b7873fa on agents/g258-a654 (pushed): receive::works::Greens lends by closure
  (read(name, with), base_of_create(parent, with)); Works builds inside the lend; test fake updated. Uncompiled.
- Studio d1323898 offline at every probe (~10 probes, 2026-11-19); main told (one-line report sent). Nothing landed.

## g258 relaunch 2026-11-19 ~16:30Z (fresh agent; UI goal 93): A-662 build, red, green, land; then A-654
- Readback of THE RULE sent to main. Worktrees: ~/slates-work/g258-git-push and ~/slates-work/g258-a654 are GONE;
  ~/slates-work/g258-a662 holds branch agents/g258-git-push (was aaed65a88 = 70d7b0e02 merged with main 4644dcec3).
  A-654 lives on origin/agents/g258-a654 (e9b7873fa); re-create its worktree with `git worktree add` when starting it.
- jupiter facts: clone $HOME/.cache/g258-a662 (update by `git fetch` + `git merge --ff-only FETCH_HEAD`, never reset);
  target $HOME/.cache/g258-target-green (built from that same clone path, so reuse is safe); logs $HOME/.cache/g258-a662-logs.
  Predecessor's db run at aaed65a: slates-db lib 154 passed 0 failed (main 153 + A-662's), Windows x86_64, jupiter.
- FOUND BY READING (pre-build), A-662 as written would have shipped:
  1. compile error: lend() called store.budget.admittable(); it is Store::admittable().
  2. replay refusal (Diverged) for EVERY non-atomic multi-ref push whose pack carried deltas: the second append's record
     has no objects but carries push.deepest(); the replay's empty push has depth 0, so the remade record differs.
  3. replay refusal (Diverged) for two pushes in flight sharing objects (a hook's await lets one commit first): the
     record listed objects judged new at ADMISSION; the store's commit (fresh()) judges at COMMIT.
  4. budget holes: RecordWriter copied every object's bytes uncharged (and record() cloned them again); GitStore.versions
     uncharged.
- PLAN: commit A (red, fb1b7cfe7 pushed): fix 1, writer notes metadata and reads bytes back at the append (fix 4),
  versions charged, Allocation refusal, Push::object made pub in slates-git; behaviour of 2/3 unchanged; new tests
  (split append at depth 2, interleaved pushes, oracle with split/defer/share + census, writer read-back) must FAIL.
  Commit B (green): record_of filters by the store's own predicate at the append (Repository::object is indexed());
  admit_object notes every New object; slates-git Push::carried(depth) and the replay carries record.deepest; the
  replay compares noted metadata (kind, identity, len) instead of re-reading bytes; docs. Then red-check mutations.
- Runs: r55 stopped (it built the old tip); r60 = red build of fb1b7cfe7, `cargo test --locked -p slates-server --lib
  vcs_store:: -- --test-threads=4`.

## g258 relaunch 2026-11-19 ~17:00Z (fresh agent #2; UI goal 93): A-662 red, commit B, green, land; then A-654
- No readback to main (brief). Worktree ~/slates-work/g258-a662 (agents/g258-git-push). Committed the predecessor's
  docs as b7cf8acd2 (wip) and pushed. origin/main now e3234e618 (A-671-ish merge engine + chain-replay test fix; touches
  slates-merge + GAPS only).
- r75 (jupiter): red build of fb1b7cf = `cargo test --locked -p slates-server --lib vcs_store:: -- --test-threads=4`,
  log $HOME/.cache/g258-a662-logs/red-fb1b7cf.log, target $HOME/.cache/g258-target-green (2.8G warm).
- Commit B written (uncommitted in the worktree): slates-git Push::carried + Repository::learned_depth + test
  a_chain_carried_by_a_push_of_no_object_teaches_the_depth; server record_of filters by `lacks` (Repository::object
  is_none, the commit's fresh() predicate) at the append; admit_object notes every Admitted::New; replay_push calls
  push.carried(record.deepest) and compares (kind, identity, len) in order (recorded_alike) instead of re-encoding;
  tests: Facts.depth, in-flight test second push at DEEP (record (DEEP,0), learned depth DEEP), Order bit 2 `whole`
  (sent_again sends all the held push lacks; census shared_whole), fixed history +2 steps (9: held at depth 3; 10:
  depth 0 whole share) so carried is red-checkable there too. Docs: A-662 entry durability/tests/applied-to.
- Landing plan: merge origin/main into agents/g258-git-push (M), green on M, then fresh worktree
  `git worktree add -b agents/g258-a662-land ~/slates-work/g258-a662-land origin/main` + `git cherry-pick -n -m 2 M`
  + one commit with a real message (tree == M), merge/push.
- Baseline repository tests to judge on Windows: repository::push::tests::every_refused_change_is_named_and_nothing_is_applied,
  repository::tests::each_verb_moves_the_names_log_and_each_members_version_its_own_log (my change touches push()).

## g258 relaunch 2026-11-19 ~19:15Z (fresh agent #3; UI goal 93): A-662 build, fix, land; then END (A-654 when main starts it)
- Worktree ~/slates-work/g258-a662 (agents/g258-git-push). Found it at 66296bff3 (commit B 368642e4c + bdf2e39e5 + a merge,
  none ever compiled). Merged origin/main twice (abeb6212c over cd84e3811, 80041f5ab over b1926d201; design log end
  resolved main's first, A-662 last). Studio: clone ~/.cache/g258-a662-src, target ~/.cache/g258-target, logs
  ~/.cache/g258-a662-logs (delete all three at END). CARGO_BUILD_JOBS=2.
- r7 (abeb621): db repository:: 22/22, git store:: 73/73 (4 ign), db --test wire_order 2/2, server --test recovery
  survive_a_daemon_restart 1/1; server lib did not compile: MAIN's breakage (vcs_http/tests/git.rs PackRefused::Stalled,
  1fb50b1be vs 188314131) -> told main; main fixed it 93bd6e807/71a43cd97.
- r26 (scratch substitution): vcs_store:: 5/13 (RED): (5) GitPush::read lent the view a settled store (room 0) ->
  every work refused PastRoom{asked 56, room 0}; (6) tests' 64-byte WINDOW below the store's object rows ->
  RowPastWindow. server repository:: 10/10.
- Fixed in 9b4cc73aa (pushed): read((&mut Store, &mut VcsStores), f) lends room then sets the budget back; tests WINDOW
  256, BLOB_BYTES 3 windows, fixed history 2.5-window blobs; rustfmt 1.98.1 run locally on my 7 files (no build);
  design entry's "Found before landing" (5), (6). r50 = full focused run on the merged tree.
- Landing: fresh worktree `git worktree add -b agents/g258-a662-land ~/slates-work/g258-a662-land origin/main`,
  `git cherry-pick -n -m 2 <final merge M>` (tree == M when main unmoved), one commit with the real message, merge, push.
- r50 (9b4cc73aa): vcs_store:: 8/13 -- the five shard tests then refused Entry(ContentUnavailable); a scratch-only
  diagnostic (refusal_of printing, restored) named Store(Malformed) from SealedPush::admit: A-614's admit walks trees /
  peels tags within ContentStore::room, and a settled store has none. Fixed 5228669c6: lent_to_read((&mut Store, &mut
  VcsStores), f) lends the shard's admittable room to read-only steps (GitPush::read, SealedPush::admit) and sets the
  budget back after. Design entry's findings (5), (6) + Evidence; GAPS' A-657 owed line (Mercurial store's home built).
- Main merged three more times (A-657 part 3 manifests: slates-hg/slates-vcs kept once each + my slates-svn; A-651's
  slates-ssh + mod vcs_ssh beside vcs_store; A-671 wildmatch). r69/r91: repository::tests::each_verb_moves_the_names_log_
  and_each_members_version_its_own_log fails -- ALSO on main's own tree 71ebf2a74 (r79): main's (likely 637a65d69,
  A-626 part 3, not bisected); told main.
- r91 at 3d0a674e3 (over main 787f5948b), macOS arm64 Mac Studio: vcs_store:: 13/13, server repository:: 16/17 (main's
  one), db repository:: 22/22, db --test wire_order 2/2, git store:: 75/75 (4 ign), server --test recovery survive 9/9,
  fmt --check clean, no warnings in my files.
- LANDED main acc2ac16f (77e5ffb46..acc2ac16f): one commit 200aac515 (cherry-pick -n -m 2 of 3d0a674e3 onto its main
  parent + Evidence) + merge of 77e5ffb46 (A-671, vfs fix; nothing A-662 uses). Diff stat: my 21 files. Main told.
  amendments.tsv A-662 -> landed. Run request filed (clippy x3, xtask check, doc gates, Linux/Windows focused runs).
- Studio scratch deleted (g258-a662-src, g258-target 14G, g258-a662-logs, g258-main-src, g258-main-target 3.5G).
  Worktrees ~/slates-work/g258-a662 (agents/g258-git-push) and ~/slates-work/g258-a662-land (agents/g258-a662-land)
  are clean; keep or remove at will.
- LEFT: A-654 (git's push end to end in the server) when main starts it: re-create its worktree from
  origin/agents/g258-a654 (e9b7873fa: receive::works create base + closure lends) and MERGE origin/main (now holds
  A-662) first. GitPush::read's signature is now read((&mut state.store, &mut state.vcs_stores), |view| ..) (destructure
  ShardState to borrow greens beside it, as vcs_store/tests.rs work_of does). Mercurial's hand-off of HgStore to
  hg_push::Objects is g304's.
- Lessons: a store whose budget is settled to what it holds has zero ContentStore::room, and A-614/A-638 parse within
  that room (refusals read PastRoom/Malformed): every read-only step must be lent room. Pure tests over a fat budget
  cannot see this; only the shard-wired tests did. Run a failing test on main's own tree (own clone + own target)
  before calling it yours.

## A-654 (UI goal 93; main started it ~20:00Z after A-662): PART 1 LANDED, part 2 owed
- Worktree ~/slates-work/g258-a654 (agents/g258-a654: e9b7873fa steps merged with main; stale A-662 wip set to main's
  version whole), landing worktree ~/slates-work/g258-a654-land (agents/g258-a654-land). Both clean.
- LANDED main e50757a1a (5c0967fb2..e50757a1a): commit 157f69a6f + merge. slates-git receive only (8 files): Session owns
  a copy of Advertised + its hidden refs (HiddenRefs for &T); RequestReader keeps a copy; Verdicts (read-only judge)
  split from Works; Unappended::Refused{at, why}; pub append_declined/atomic_declined; steps 1-2 (create at first
  parent's version else default head; greens lent by closure); tag test updated to the create-at-base model.
- Studio (macOS arm64, r131 then the merged-tree run on 6f2d690eb): receive:: 42/42, protected_fixtures 5/5,
  store::views 7/7, fmt clean, no warnings. First run 41/42 (the tag test written for A-620's empty-green create).
- PART 2 (the server driver), the design to build, crates/server/src/git_push.rs:
  - Why the split: on the owner shard the greens, the appends AND the push's object view (A-638 PushedObjects over the
    store) are one ShardState, and push_sealed mutates the content store; so the server's Judge holds &mut ShardState
    and, in judge(), lends room (add vcs_store::with_room(state, |&ShardState| ..): lend(0), read, set budget back),
    builds ShardGreens(&ShardState) + IndexedObjects (receive::Repository) + the sealed push's PushedObjects (add
    SealedPush::view(stores, |objects| ..)), runs Verdicts::judge, drops all borrows; append() maps Entry ->
    BranchChange + StoreMove and calls push_sealed (&mut state), AppendRefused -> Unappended (Changes Moved/Conflict by
    index; Entry Database Moved -> Moved; everything else -> Refused{why: slates' words}) -> append_declined /
    atomic_declined. append_atomic = one push_sealed with all.
  - ShardGreens (impl works::Greens): ref name <-> namespace name (`refs/heads/N` <-> N; other `refs/...` full);
    green = Repositories::branch(repo, N); born = the git store holds a version of green_key(green); unborn reads as
    none; GreenRead{id: green_key, position: green_head, green: lent engine (state.greens.get)}; version_of answers
    ONLY the green's current head (GitStore::version_of newest == current head), so a push from an older version is
    "moved" until A-664's server-made commits; base_of_create(parent): newest branchable version holding parent
    (Repositories::branch_floor), else the default branch's head if born, else None.
  - Entry -> BranchChange: Work{green key} -> Update{green, work} (CAS: green_head == position), store move
    (Target::Green, Some(new)); Create{name, from: None} on an unborn default root -> Update{green: root},
    Target::Green(root); Create{from: Some((key, version))} -> Create{name, from: green of key, version, work: Some},
    Target::Created(name); Retire -> Delete{name, version: head}, (Target::Green, None). Keep key->VolumeId in the judge.
  - ReceivePack {session: Session<'static, PolicyHidden>, admission, push: Option<GitPush>/sealed}: start(state,
    (principal, repo), advertise, out): record (NotFound), tool Git, store held, Advertised{format, atomic: true,
    ofs_delta: true, push_options: false, session_id: None, agent: const}, refs = branches with git heads sorted by
    full name; PolicyHidden = refs the principal may not Read (vcs_policy::admit Read). input/end via lent room +
    IndexedObjects. answer: Checked::objects -> GitPush::admit_all (Incoming{kind, claimed: Some(id), content,
    depth: chain.depth}) -> seal -> vcs_policy::admit (Submit; Rewrite when A-614's Move is not a fast-forward; Delete)
    -> hooks: any GitPreReceive/GitUpdate/GitPostReceive in record.policy.hooks -> decline all (protected::HOOK_DECLINED
    + "hooks' view owed (A-665 part 2)") -> Session::answer(&mut ServerJudge, out) -> abandon the sealed push.
  - Test: the create fixture (crates/git/fixtures/push/create.request.bin) pushed into a fresh git repository on a
    ShardState (audit_on_shard): report "unpack ok" + "ok refs/heads/main", root green at version 1, the store head of
    green_key(root) = the pushed commit, one record in the register; then an update, a delete, a hidden ref, a hook
    configured (declined), a policy denial.
- Lesson: count remaining context before a big piece; land the self-contained half and hand the rest over with its
  design written down.

## A-654 part 2 (g258 relaunched fresh 2026-11-19; UI goal 93): git's push driver in the server -- IN FLIGHT
- Worktree ~/slates-work/g258-a654-p2 (branch agents/g258-a654-p2 from origin/main e50757a1a, pushed): a56e7a725 wip red
  (tests + stub), e6d2b1405 wip green (crates/server/src/git_push.rs + tests; vcs_store with_room + SealedPush::quarantine;
  vcs_policy Compiled::admits; repository::branch_version pub(crate); slates_git::upload::AGENT pub), c76663d27 wip docs
  (design §4.17.2 status, A-654 part 2 amendment entry with evidence, GAPS A-654 line).
- Studio (macOS arm64, d1323898, target ~/.cache/g258-target, logs ~/.cache/g258-a654p2-logs, clone
  ~/.cache/g258-a654p2-src): r223 red at a56e7a725: 32 passed, 3 failed (the 3 new, at start on the stub); r234 green at
  e6d2b1405: `cargo test --locked -p slates-server --lib -- git_push:: repository::push:: vcs_policy:: vcs_store::
  --test-threads=4` 35/35, fmt clean, no warning. No server integration target pushes through git (none makes a repo).
- Landing worktree ~/slates-work/g258-a654-p2-land (agents/g258-a654-p2-land, pushed): 9d60cd640 = the one real commit
  (squash of the 3 wip, on e50757a1a) + 62db35348 = TEST merge of origin/main bb535bd4d (58 commits: slates-git store
  A-647 part 3, server vcs_store/repository/hg; design-log end conflict resolved main's first, A-654 part 2 last).
  Kiln restarted twice (main's notes); r16 = merged-tree run at 62db35348 (same filters + fmt).
- LAND PLAN (A-662's way): on green, fresh worktree from origin/main: `git worktree add -b agents/g258-a654-p2-final
  ~/slates-work/g258-a654-p2-final origin/main`, `git cherry-pick -n -m 2 62db35348` (tree == 62db35348 when main is
  bb535bd4d), add the merged-tree run to the Evidence bullet, ONE commit with 9d60cd640's message, merge newer main if
  moved (check it touches nothing the driver uses), diff stat = my 9 files, push HEAD:main.
- r16 (merged over bb535bd4d, 62db35348): 40/40 + fmt clean. Then main moved 18 commits incl. A-644 part 2 (g230:
  git_protect, vcs_policy::admit with Asked.brings, Right::{MergeCommit, UnsignedCommit}, PushRefusal::{MergeCommit,
  UnsignedCommit, Unjudged}). Merged ee5cbed4c into the landing branch as 2add5cf35 (test merge): vcs_policy.rs = main's
  + `admit_right` pub(crate) (my Compiled::admits DROPPED); driver's policy step = git_protect (right_of, StoreBrings,
  declined) via `admitted()`; unheld_key = green_key of the repository id for creates; docs/GAPS updated (A-644 part 2's
  owed line too). r48 = run at 2add5cf35 with git_protect:: added.

## A-654 part 2 LANDED (g258 relaunched fresh 2026-11-19 ~21:20Z; UI goal 93)
- Found: the landing branch agents/g258-a654-p2-land pushed at 0850516b3 (signers fix after r48's compile stop), its run
  merged3.log on the Studio 46/46 + fmt clean; ~/slates-work/g258-a654-p2-final was a stale partial checkout of main
  (its metadata gone from the g253 clone's .git/worktrees, no cherry-pick ever ran): removed and re-made on its branch.
- Merged origin/main 3612d1ef9 into the landing branch as 8af0bd751 (pushed): one conflict, the design log's end, main's
  entries first and A-654 part 2 last; the piece's 76 design lines and its GAPS lines unchanged by the merge.
- Studio (macOS arm64, d1323898, debug, CARGO_BUILD_JOBS=2) at 8af0bd751: `cargo test --locked -p slates-server --lib --
  git_push:: repository::push:: vcs_policy:: vcs_store:: git_protect:: --test-threads=4` 46/46 (git_push 3, git_protect
  3, repository::push 4, vcs_policy 18, vcs_store 18; merge, git, db, server recompiled), `cargo fmt --all -- --check`
  clean, no warning. No integration target pushes through git (none on main names receive-pack; no edge calls the driver).
- LANDED main 0577ab0bd (b5231f0ff..0577ab0bd, fast-forward, ONE commit): worktree ~/slates-work/g258-a654-p2-final
  (branch agents/g258-a654-p2-final) fast-forwarded to main b5231f0ff (docs + a vcs_http.rs comment only, so the run
  stands), `git cherry-pick -n -m 2 8af0bd751`, Evidence bullet filled (was EVIDENCE_PENDING), diff stat = my 9 files.
- amendments.tsv A-654: part 2 landed. Run request filed (Windows/Linux focused runs, clippy + xtask + doc gate).
- LIKELY CLIPPY FINDING for main (read, not run): t_a654_a_push_with_atomic_..._a_ref_at_a_time's audit_on_shard
  closure has 11 assertion macros, cognitive complexity ~12 > clippy.toml's 10 (deny, tests held to it); the policy
  test's closure is at 10. Fix = two named helpers (refused half, applied half), rules unchanged. Told main.
- Studio scratch deleted (g258-a654p2-src 202M, g258-target 9.0G, g258-a654p2-logs). Left on the Studio, older and not
  this piece's: ~/.cache/g258-step-continued-ada-src (2026-11-18 WinFsp clone). Local scratch-g258-a654p2 deleted.
- Worktrees ~/slates-work/g258-a654-p2, -p2-land, -p2-final: clean, landed; keep or remove at will.
- LEFT (A-654 owed, GAPS): the edges' calls (g299 HTTP GitServices, g294 SSH), hooks over A-665 part 2's view, older
  bases (A-664), principals other than the owner at A-635's append, push options, refs/for/, fragments off the answer's
  path, a tag of a tag.

## A-654 part 3 LANDED (main's next piece, 2026-11-19 ~22:00Z; UI goal 93): git's HTTP site serves receive-pack
- Main's ask: the site calls ReceivePack (start, input, end, answer) for git-receive-pack, so a stock git push over HTTPS
  lands through the driver; red first (503 today); focused tests vcs_http + git_push + repository::push; land; END.
- Worktree ~/slates-work/g258-a620-http (agents/g258-a620-http: wip red 05ccf25c9 + d81b05529, green 1a5273730, docs,
  merge f1e4d78e6 over main 225d7eead); landing worktree ~/slates-work/g258-a620-http-land (agents/g258-a620-http-land).
- Design: crates/server/src/vcs_http/git/receive.rs (ShardLender; ThisShard over with_state; ReceiveServices; Receive
  exchange: body -> input, body_end -> end + answer, reply gives the written bytes MAX_LEN a run; PushFailed -> status
  400/413/404/500/503; at start ServiceRefused::{NoRepository, Unreadable, Unwritable}); git.rs Halves<U, R> +
  HalfExchange, ServiceRefused::kind, Refused::Unwritable (vcs_http.git.unwritable; the prelude failure uses it too);
  daemon: DaemonSite = Halves<Unserved, ReceiveServices<ThisShard>>. git's sources read (v2.50.1): receive-pack.c
  2558-2574 (v2 ignored, `version 1` first on advertise), 2590-2644 (no commands: nothing written); http-backend.c
  555-558 (prelude unless v2); remote-curl.c 500-501 (push asks v0), 868-890/939-981 (`0000` probe, then chunked).
- Test: vcs_http::daemon_edge::tests::a_stock_git_push_over_the_daemons_edge_lands_through_the_push_driver (two-shard
  daemon, loopback TLS, create fixture -> git's very report, main advertised after, delete under a deny rule ->
  `deletion prohibited`). Its HTTP helpers are g309's verbatim (agents/g309-lender) so A-682 merges to one copy.
- Studio (macOS arm64, d1323898, debug, CARGO_BUILD_JOBS=2): red r138 at d81b05529 47 passed + the new test failing
  `(503, [])` at its first assertion; green 1a5273730 48/48 + fmt clean + no warning; merged f1e4d78e6 over 225d7eead
  49/49 (vcs_http 42, git_push 3, repository::push 4) + fmt clean + no warning.
- LANDED main f05af5288 (225d7eead..f05af5288, fast-forward, ONE commit via cherry-pick -n -m 2 of f1e4d78e6 + Evidence).
  Diff = my 11 files. amendments.tsv A-654: part 3 landed. Run request filed (Windows/Linux runs, the live git push over
  HTTPS incl. a >1 MiB push, clippy/xtask, and g309's merge points).
- Studio scratch deleted (g258-a620-src 211M, g258-target 6.0G, logs); local scratch-g258-gitsrc deleted.
- LEFT: the SSH channel's receive-pack (g294 with me, main's next); the rest of A-654's owed list.

## A-686 (relaunched g258 ~22:35Z 2026-11-19; UI goal 93): Basic on the loopback cleartext edge, Basic's challenge first
- Worktree ~/slates-work/g258-a686, branch agents/g258-a686 (pushed): 7b8ac8402 wip red (tests only, against main's
  API), 40f1709c2 merge of origin/main 770ae8826 (clean; brought g083's stock-hg cli tests, A-680 part 1), 4d2493b2f
  wip green (code + tests on the new API + design/GAPS, Evidence = EVIDENCE_PENDING in design log + GAPS A-686 line).
- Design: http-edge credential(head) and challenges(realm) take no connection; BasicInCleartext, git's
  Refused::CleartextBasic and counter vcs_http.git.cleartext_basic removed; challenges = [Basic+charset, Bearer];
  git/hg sites' answer/proof/refuse/unauthorized/start drop `secure`; Site::decide KEEPS `secure` (LFS hrefs' scheme,
  A-629 2b-ii), git/hg impls ignore it (_secure). Tests: http-edge challenges_offer_basic_first_then_bearer_each_naming_
  the_realm; git site 3; hg site 2; daemon_edge 2 new (daemon_named, cleartext_client, head_and_body); serve.rs
  served_by + stock_hg_meets_basic_first_on_a_loopback_edge_and_reaches_the_hg_site (HgSite<OwnerToken, OneMercurial,
  NoStores>, hg identify http://hg:owner-token@addr/hg -> 503); cli an_anchors_vcs_http_flag_starts_its_daemons_edge
  (Basic then Bearer). g083's two HTTP stock-hg cli tests: A-686 dropped from ignore reasons (still wait on A-680/A-685).
- Studio (d1323898; clone ~/.cache/g258-a686-src ff'd per run; target ~/.cache/g258-target; logs ~/.cache/g258-a686-logs):
  r295 RED at 7b8ac8402 over main b94ae6e90: http-edge --lib credential 9 passed 2 failed (the 2 new); server --lib
  vcs_http:: 38 passed 8 failed (exactly the 8 new/changed; stock hg: "** unknown exception", exit 1, ValueError via
  request.py:1247 -> hg url.py:418 -> request.py:1129); cli --test cli vcs_http 1 passed 1 failed (Bearer alone).
  r325 GREEN at 4d2493b2f (running): same three + fmt --check.
- FINDING for main: the daemon's site is git's alone (DaemonSite), so stock hg against a running anchor meets git's
  route (404) before any 401; composing git's and hg's sites in the daemon is not visibly anyone's piece (A-680 part 2
  is the lender; g297's svn branch notes "the sites composed into one the daemon starts (git's alone today)" as owed).
- LAND: fill Evidence (design entry + GAPS line), fresh worktree from origin/main (git worktree add -b
  agents/g258-a686-land ~/slates-work/g258-a686-land origin/main), git cherry-pick -n -m 2 <final merge> or of the
  net diff, ONE commit with the real message, merge newer main, diff stat = my 10 files, push HEAD:main.
- ~23:10Z: r325 GREEN (http-edge compile miss in tests/connection.rs fixed 59229e97a; rerun 10/10), server 46/46, cli 2/2.
  Merged main 693bc8feb (e338ae715: A-681 svn site adapted: credential(head)/challenges(realm)/answer(head); svn interop
  two HTTP legs drop A-686) -> r361 10/10, 51/51, 2/2, svn_interop --list compiles, fmt clean.
- Merged main b288b412e (1435ec4bb): A-680 part 2a put the hg site on the daemon's edge (ToolSites) and REMOVED NoStores
  -> serve.rs reverted to main's; NEW daemon test stock_hg_identifies_a_repository_on_a_daemons_loopback_edge_by_basic
  (create_hg_from_states, hg identify http://hg:TOKEN@127.0.0.1:PORT/name -> exit 0, tip = heads[0][..12]); g083's
  stock_hg_without_credentials_is_refused_in_its_own_words UN-IGNORED (last wait A-686); round trip waits A-680 part 2b.
  Union-merge gotcha: both appends ended "  );\n}" so git kept it as common suffix -> main's test lost its close; fixed.
- Red (2): branch agents/g258-a686-red2 (ce35f09fb = main b288b412e + my daemon_edge.rs and cli.rs), worktree
  ~/slates-work/g258-a686-red2, Studio clone ~/.cache/g258-a686-red2-src, target ~/.cache/g258-red2-target: r392 running.
- Then: green on 1435ec4bb (same filters + `--test cli -- vcs_http stock_hg_without_credentials`), fill RUNS_PENDING
  (GAPS) and the design Evidence, land via fresh worktree at origin/main + git diff apply, one commit, merge, push.
- A-686 LANDED main e21beb606 (20ae700a7..e21beb606, 2026-11-20 ~00:20Z): content commit 7b3044c30 (on b288b412e) +
  merges of main ba35a4985 (fc5f4ea98), cbd5382bb (8c7965bf4: A-629 part 2c, git's `answer` KEEPS `secure` for LFS's
  href scheme; proof/refuse/unauthorized/start connection-free), 7a7ac23c1 (0b0c8e12e), ee880e2e4 (23b566fd6),
  e21beb606 (20ae700a7, A-647 part 4: store only, run stands). Diff vs main = 17 files.
- Also un-ignored: g083's stock_hg_without_credentials_is_refused_in_its_own_words (cli.rs, anchor) and g309's
  stock_git_clones_pushes_and_fetches_over_the_vcs_http_edge_with_basic (git_interop.rs). Daemon test renamed to
  stock_hg_reads_a_repositorys_capabilities_on_a_daemons_loopback_edge_by_basic (`hg debugcapabilities`): `hg identify`
  stops at the site's capabilities, which don't advertise `lookup` (crates/hg/src/serve/session.rs SERVED_HTTP) -> main.
- Runs (macOS arm64, Mac Studio): red r295 (main b94ae6e90), r392/r429 (main b288b412e: stock hg crash vs daemon and
  anchor); green r415/r429 (b288b412e), r461 (fc5f4ea98), r501 (0b0c8e12e), r518 (23b566fd6): http-edge 10/10, server
  vcs_http:: 61/61, cli 4/4 (+1 ignored), git_interop 2/2, svn_interop compiled, fmt clean.
- Gotchas: (1) the MacBook disk filled during `git worktree add` (No space left on device); freed ~27 GiB by removing my
  10 landed clean worktrees; check `git diff --stat` for truncation before any commit. (2) Other agents' fetches move
  the shared refs/remotes/origin/main mid-merge: compare against MERGE_HEAD, not origin/main. (3) Union merges of two
  appends that end in the same lines ("  );\n}") lose one side's closing: git keeps them as the common suffix.
- Cleanup done: Studio g258-a686-src, g258-target (12G), red2 src/target (7.7G), land src/target (12G), logs deleted;
  local worktrees g258-a686, g258-a686-land removed (branches pushed). Left: ~/slates-work/g258-step-continued-ada-winfsp
  and Studio ~/.cache/g258-step-continued-ada-src (2026-11-18 WinFsp, not this piece's).
- LEFT for A-686: Linux/Windows runs and main's gates (run-requests filed). Next pieces are main's to start.

## A-700 (g258 relaunched fresh 2026-11-20 ~02:xxZ; UI goal 93): hg's `lookup` advertised and served as 7.2 does
- Brief: stock `hg identify` against the daemon aborts "remote repository does not support the 'lookup' capability"
  (found by use, r415). SERVED_HTTP in crates/hg/src/serve/session.rs omits `lookup`. One fix for HTTP and SSH.
  Failing tests first: daemon-level stock `hg identify` against the edge; lookup's answers held to stock 7.2's for a
  node, a bookmark, a tag, an unknown name, an ambiguous prefix.
- A-700 already claimed in amendments.tsv by my predecessor (row 243). Predecessor left worktree
  ~/slates-work/g258-a700 (branch agents/g258-a700) clean at a7b5e4292 with no commits; fast-forwarded to main 3b392dcb1.
