# g085: ITEMS (updated 2026-11-18 23:20 UTC; ENDED after A-472 by the coordinator's order: main starts the next piece)
Rule: Ada's final rule: ONE piece a message -> implement -> focused --lib unit tests on the Mac Studio d1323898
(sync:false, a GitHub clone of a pushed work branch, own target under ~/.cache) -> cargo xwin check -> fmt -> merge
origin/main -> diff --stat -> git push origin HEAD:main as its own command -> file heavier runs -> few-line report -> END.
Never checkout/stash/reset/force/commit-tree. Builds never on this MacBook, never in ~/Projects/slates.

## Goal 86 (kiln 96b28e3c): DONE by my predecessor (slates main 503662a8f..a1daa3681); main marks it met when its
Windows lanes run green.

## Windows leg of goal 88's bare-metal part (kiln 01a1ec34599974448aed2672bafaf9a2; g264 keeps macOS and Linux)
- [x] W0 seam agreed with g264 (mine: every cfg(windows) arm of kiln's workspace/{place,slates}.rs, handoff.rs's
      project-dir rule for K:\, the Windows runs); notes sent to g264 and g088v.
- [x] W1 LANDED slates b2dc8363c (A-417, CONTRACT 37): mount::available() on Windows answers MountMissing::MountManager
      where WinFsp's MountUseMountmgrFromFSD is not 1.
- [-] W2 DROPPED: the kiln re-vendor is g088v's.
- [x] W3 LANDED kiln eba1841e0: a drive letter refused DriveLetterTaken moves make() to the next free letter.
- [x] W4 LANDED kiln fdd747d37: git's own repo prefix (repo_place); a drive is the workspace's only when its volume names
      it; unmount detaches its own attachment.
- [x] W5 FILED (bare_metal_tests with KILN_TEST_KILN=kiln.exe, then g266's slates_e2e bare_metal::; WinFsp +
      MountUseMountmgrFromFSD=1). Not yet run; re-filed at END.

## Owed Windows landing items (GAPS A-185 "Owed")
- [x] O1 LANDED slates 986b2b8b7 (A-438): a symbolic link in a Windows landing, in A-275's form, staged and moved.
      Its Windows test a_symlink_in_a_landing_is_a_windows_symbolic_link: not yet run; re-filed at END.
- [x] O2 LANDED slates 0986f5900 (A-445): step 6's reflink for a batch's own duplicates; ReFS / Dev Drive block cloning.
      Main ran the whole land suite at A-445 (r288, all green, crash oracles included). Its ReFS test
      a_duplicate_file_is_cloned_on_a_volume_that_shares_clusters (SLATES_TEST_REFS_DIR): not yet run; re-filed at END.
- [x] O3 LANDED slates b97d33e90 (A-451): each manifest entry carries the volume's Windows bits (layouts 3/4, bound by
      the grant); the Windows writer sets them on each file, directory and link it makes, reads them back and refuses
      EOPNOTSUPP where one did not take (FAT); a directory link lands as one. Studio: land --lib 20/20, vfs 3/3, red
      twice, macOS + xwin checks clean. Not yet run: the oracle's two bits tests, the crash oracle, the Windows
      --test os suite; re-filed at END.
- [x] A-458 LANDED slates main ae77935b0..5e7d7250d (tests-first 72ea0a770, fix e0aa55ffb, docs+fmt f2a768e8f, merge
      5e7d7250d): a file whose mode or Windows bits alone changed lands as Action::Chmod { from, to } (tag 8, bound by
      the grant), judged field by field (meta_verdict: set / identical / ModifyModify; a field the volume kept is the
      disk's), set through LandFs::open_to_chmod's handle (set_mode, set_windows), no byte rewritten. Second fault the
      red tests found: the plan read a metadata-only copy-up's bytes from the disk, so an outsider's chmod refused the
      whole landing BaseDrift; now its identity is its witness's (Volume::holds_witnessed_bytes). Wire: PlannedAction::
      Chmod, ActionTally::chmod (appended, wire_order.txt); API contract 41. Studio: red first 5/25 (r346); green land
      --lib 26/26, vfs 3/3, api 2/2, ipc wire_order 2/2; red-check (field vs disk alone) 3/26 failed; macOS + xwin
      checks (land, vfs, ipc, api, server --all-targets) clean. Bug record docs/bugs/2026-11-18-a-files-mode-or-windows-
      bits-changed-alone-never-landed.md. Filed 22:29Z with the re-filed A-451/A-445/A-438/W5 runs. Owed (GAPS):
      metadata-only durability (file fsync off the shard), Windows x-bits no-op entry, check-then-set window; siblings:
      touch alone, base directory mode alone, captured clone Replace over outsider mode, no golden vector for the
      manifest encoding.
- [x] A-466 LANDED slates main ba179ff61..552fe1f0c (tests-first d6214ef8a, fix dbd3085ae, docs+fmt c2f8eee74, merge
      a3097558e, the log's end resolved 552fe1f0c — the merge commit had kept conflict markers in SLATES_DESIGN.md, fixed
      by the next commit before the push): a captured clone's replacement judges its metadata field by field beside its
      bytes (verdict::landed_meta): the outsider's mode kept under the agent's bytes, a mode both changed differently
      ModifyModify, the agent's mode set alone (Landing::chmod) over bytes already on the disk. The content witness carries
      the captured FileMeta, encoded (VERSION_CONTENT 5). Overlay unchanged (T-1.14 refuses), sibling test proves it.
      Studio: red first 3/30 (r375); green 30/30; red-check (Kept taking the clone's value) 1/30 failed; macOS + xwin checks
      (land, server --all-targets) clean; fmt --check clean. Filed 22:41Z.
- [x] A-469 LANDED slates main ef82f7340..21e017deb (draft a6afeb8ef, identities 9ac590c0e, docs+fmt b3e696b5a, merge
      21e017deb): golden vectors for the manifest encoding, layout 3 (overlay) and content 5 (captured clone), every action,
      bits set and unset, header/entry/identity lines in crates/land/src/manifest_vectors.txt, `--ignored
      publish_the_manifest_vectors` writer; an independent Python encoder agreed byte for byte on every header and entry
      line; determinism test; every-field-moves-the-identity test (in place of decoder hostile tests: no decoder exists).
      Studio: land --lib 33/33 (+1 ignored); red-check (no bits in the encoding) 2/33 failed. Owed (GAPS): no decoder;
      u16 length prefix lies past 65,535 bytes (no collision possible, a zero byte in every entry); an overlay witness's
      uid/gid/birth/windows judged but not bound.
- [x] A-472 LANDED slates main 5ceda4847..1f332da55 (red 86b850aa1, fix 15dc21234, docs+fmt 06ae8b150, clippy
      ad72aeeda, merge 1f332da55): a metadata entry is staged in A-130's batch, its file synced through its own handle by
      the barrier before it is reported (Linux fsync, not fdatasync; macOS fsync; Windows FlushFileBuffers via a handle with
      FILE_WRITE_DATA), sources cited in §4.15 step 8 and the amendment (fsync(2), ext4 and XFS docs, Apple fsync(2) and
      fcntl F_FULLFSYNC, FlushFileBuffers). Also main's lint findings in land's test files (test_store fixture, vectors'
      named constants, a named type, splits). Studio: red first 3/35 (r433); green 35/35, vfs 3/3; red-check (barrier
      without the metadata files) 3/35; clippy land clean; vfs clippy only export.rs (not mine); xtask none of mine;
      macOS + xwin checks clean. Crash-oracle rerun filed 23:20Z. Owed: Windows read-only file's metadata entry fails
      alone (FlushFileBuffers needs write access); a made directory's mode has the same durability question.
- [ ] Queued by main, each its own piece: a touch alone and a base directory's mode alone landing; then O4.
- [x] O4 the seam's numbering: LANDED bf21a05b4 (A-527).
- [ ] FICLONE (Linux) and clonefile (macOS) behind LandFs::clone_temp (A-445's owed half).
- Other owed, in GAPS: the Windows base reading a link back in A-275's / form (not its print name's \); the Developer
  Mode route for links, measured; a directory's bits after mkdir (crash window, as Unix's mode); PlannedEntry shows no
  bits; duplicates across batches and the reflink's measured gain.

## Clones
- slates: ~/slates-work/g085-windows-unit-tests (branch agents/g085-windows-landing, clean, = main 1f332da55 at A-472's landing)
- kiln:   ~/slates-work/g085-windows-unit-tests-kiln (branch main)

## Log (newest last)
- 22:20Z progress rewritten after the ~/.cache wipe; A-458 assigned by main; working on it.
- 22:30Z A-458 landed 5e7d7250d; runs re-filed (A-458, A-451, A-445 ReFS, A-438 link, W5); ENDED. Next by main: O4.
- 22:41Z A-466 landed 552fe1f0c; oracle reruns filed; ENDED. Next by main: the golden vector piece.
- 22:50Z A-469 landed 21e017deb (unit tests only; nothing heavier to file); ENDED. Next by main: chmod durability.
- 23:20Z A-472 landed 1f332da55; crash-oracle rerun filed; ENDED. Next by main: touch alone and a base directory's mode.
- 23:40Z A-501 in progress (resumed after the usage cut): red tests 7e52f5c55 (r478: 3 of 38 failed as expected); base plane + manifest edits uncommitted in the clone; next verdict, engine, seam open_dir_to_chmod, sim, writers.
- 00:07Z A-501 landed fec67892f (land --lib 41 passed, vfs --lib 13 of 13, fmt clean; red r478 3 of 38); vfs integration test, clippy, xtask, xwin, oracle, crash oracle and Windows `--test os` filed for main; ENDED. Next by main: the Windows read-only-file metadata entry, then O4.
- 00:20Z A-519 assigned (a Windows read-only file's metadata entry lands); sources read: Microsoft's File Caching (metadata stored only by a flush or a FILE_FLAG_WRITE_THROUGH open), FlushFileBuffers (GENERIC_WRITE), MS-FSA 2.1.5.1.2.1 (read-only refuses FILE_WRITE_DATA/FILE_APPEND_DATA only), NtCreateFile (FILE_WRITE_THROUGH); building: pure meta_open in windows_bits.rs (red first), the writer's write-through fallback, its barrier leaving it out.
- 00:24Z A-519 landed e3fbf2ee0 (red: windows_bits 1 of 6; green: land --lib 42 passed; fmt applied); Windows `--test os` test, Windows windows_bits test, xwin and clippy filed for main; ENDED. Next by main: O4.
- 00:30Z A-527 (O4) assigned: the land seam's numbering typed; design: HostError::Win32(u32) beside Unavailable(errno), the Windows writer's mapping pure (windows_codes.rs, red first), the engine reading Win32 as itself, the server and base passing it on.
- 00:37Z A-527 (O4) landed bf21a05b4 (red: windows_codes 1 of 1; green: land --lib 45 passed, vfs host:: 10, server landing:: 12; fmt applied); Windows lanes, xwin and clippy filed for main; ENDED. Queue empty.
- 00:45Z A-539 assigned: OsCode end to end (land report, VfsError::BaseUnavailable, wire: HostAnswer/LandingDegradation/TargetRefusal/Refusal Win32 variants appended per D-15), named kinds at the seam (Exists, NoSpace, Unsupported), contract bump, golden vectors; red first on the pure mappings and the vectors.
- 01:30Z RESTARTED (usage cut). A-539 had NOT landed: branch agents/g085-a539 (4c28f696c) held red tests 033ecb3d0, the
  change 04e24caab, api/mcp/sdk-node/docs f288c4b4a at contract 45. Review found: the metadata open and three rmdir
  sites read `Unavailable` alone (named kinds now ended the landing `Aborted`); `OwnMount` kept Windows' 1131 in an
  errno; the Windows over-long base sent MSVC's 38; the GAPS claim "the anchor's layout stands" was wrong (completion
  records keep reply bytes read back with no schema word). Merged main (5d6eb5899: contract 48, A-524's `Pending`).
- 01:44Z red r59 (Studio): predecessor's red 033ecb3d0 land 2/2, ipc vectors, server 1/1 all FAILED as designed; my red
  92362f024 land 2/2 (`Exists(Errno(17))` -> Aborted/Crashed) and wire (major 2 read) FAILED as designed.
- 01:47Z fix 1ffe0d9f4 pushed (named_code, OwnMount(OsCode), PATH_TOO_LONG typed, wire MAJOR 3, anchor LAYOUT 11, docs);
  green run r76 running. NEXT after landing: main's queued link piece (r143/r212 finding: symlink_dir's unprivileged
  flag vs set_reparse's ERROR_PRIVILEGE_NOT_HELD; split crates/land/tests/os.rs:716).
- 01:55Z A-539 LANDED main 1b2adf352..efe8ea79c (contract 49 since main's A-520 third piece took 48; wire MAJOR 3; anchor
  LAYOUT_VERSION 11). Green r76 at 1ffe0d9f4 and post-landing r90 on efe8ea79c all passed; run requests filed 01:55Z
  (Windows lanes + xwin, the anchor's cross-process test, land/vfs/bridge-core integration suites, sdk-node, clippy).
  Owed (GAPS): MountRefused's Win32 code on Windows; whose errno (Linux vs macOS) across a mixed fleet; HostFault::Os.
- 01:56Z starting main's link piece (r143/r212 finding).
- 02:43Z A-568 LANDED main 2514818a5..ae0878283 (red 8e122983b, fix 121cc896c, docs 4af05d1de, three merges): a link
  the account may not make fails that entry alone (`HostError::Unprivileged`, `ERROR_PRIVILEGE_NOT_HELD` named by the
  Windows rule); the link's object opened as well as set under the link token (slates' own drive takes the privilege at
  the open, design deviation (b)); the Windows link tests take the writer as their probe; A-451's test split; stale
  os_windows.rs errno docs corrected. Cited: Microsoft Learn CreateSymbolicLinkW; MS-FSA 2.1.5.1, 2.1.5.10.37;
  bugs.python.org msg302494 (Eryk Sun); the design's WinFsp record (runs 42715955734, 42720006662). No handle-relative
  Developer Mode route exists. Red r259, green r288; post-landing r314 on ae0878283 passed (land 50, vfs 17,
  server 16); Studio target removed; run requests filed 02:45Z. ENDED; next by main: MountRefused.
  LESSON: two clones in one CARGO_TARGET_DIR with one Cargo.lock: cargo judged the second (cloned before the first's
  build) fresh and ran the first's binary (r259); clone the second after the first build, or separate targets.
- Owed (GAPS): Linux `EPERM` (a link on FAT) still ends a landing. Queued by main next: MountRefused's Win32 code.
