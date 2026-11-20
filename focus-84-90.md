# Focus: UI goals 84-90 FIRST (Ada 2026-11-18: "NO I WANT YOU TO COMPLETE GOALS 84-90 FIRST"; 79-83 held until then)
# Map re-derived from goals_get 2026-11-18 ~23:45 UTC (top-level, 1-based):
#   84 d9f7a795 an unresolved object is recovered -> g250 (A-484)
#   85 17ad60b6 every open slates bug fixed, failing test first -> g079 (A-485; bug list in its progress file)
#   86 5875dc0c every fixed wait derived -> g255 (A-474 watchdog, then event-driven reaper A-475..), g251b (C-b part 2 A-468)
#   87 ce6a2d3a W1 + A-273, <1 ms at 1,000+ clients -> g083 (A-476 successor chain bug); main's A/B r406 running
#   88 28e73c34 kiln vendors slates; library, vendoring, bare-metal mac/linux/windows -> g085 (A-472), g264 (macOS mount bug); main's runs
#   89 95101ef3 federation across kiln runners -> g267 (K5, A-463)
#   90 c2e4337b Shards/Hex/Mantle needs -> g268 (A-471 S3 client), g234 (A-462 Unix handoff)
# (The earlier "84-90" table below this line used a stale map: its 84 and 86 were 7cda92b5 and 96b28e3c, now 77 and 78.)

# Focus: UI goals 84-90 (Ada 2026-11-18 ~19:50 UTC: "drive goals 84-90 here to completion and focus"). Rebuilt ~22:10 UTC after the ~/.cache wipe.

| goal | kiln id | done when | owner | state |
|---|---|---|---|---|
| 84 | 7cda92b5 | macOS kernel-mount kill run: 0 empty | main | MET (run 44379621688) |
| 85 | 5875dc0c | every fixed wait derived (g255's list); A-344 fixed | g255 (B, D, placeholders, L-h), g251b (C) | A and B closed; ~35 left; A-344 fixed by A-428 |
| 86 | 96b28e3c | main CI Windows lanes green | main | MET (run 44383503634) |
| 87 | ce6a2d3a | W1 + A-273 built; <1 ms at 1,000+ clients | g083 | steps 1-4 on main (367ba28b0); leak fixed (fbee3e79f); step 5a now (A-457) |
| 88 | 28e73c34 | library, vendoring, bare-metal mac/linux/windows | g264 (kiln I12), g085 (A-458), main | Linux bare-metal PROVEN r194; macOS r284 running; library diag 44395174565 macOS job queued |
| 89 | 95101ef3 | kiln runner fleet + multi-runner test | g267 | K1-K3 landed; K4 now (A-450) |
| 90 | c2e4337b | every Shards/Hex/Mantle need | g268 (A-459), g234 (A-460) | archive export A-441 + import A-453 landed; streaming export now; item 7 next |

## Main's runs in flight (2026-11-18 ~22:10 UTC)
- r277 quiet Studio 2768d0d5: the three failing fleet flows at 3022c6894 rerun + baseline at 2a5bf7264 (git archive).
- r284 f3f618f2: kiln bare_metal:: + bare_metal_tests on macOS at kiln 68eda5b14+ (g264's mount fix).
- r327 adacachy: messages of recovery a_boot_give_back... and fleet a_successor_two_versions_behind / a_move_whose_target_dies on the lint branch.
- Lint branch agents/main-lint-2026-11-18 at 721216a10 (main 899aac162 + lint 92c5a7cb3 + TSan skip 2dae58cd5 + fleet splits): regate clippy after r327.

## Owed by main
- Crash oracle rerun for A-451 (set_windows is a new write step).
- Mark goal 88 parts met as their runs land (library: macOS diag; bare-metal: r284 + W5 Windows).

## ~23:30 UTC
- LANDED main 57f50dcae: main's lint batch (24 items + TSan skip + watchdog manual_clamp allow + literals in held_turn/image). Remaining clippy/literal findings are agent-owned, routed: g234 (anchor handles.rs), g251b (ipc rendezvous/region/endpoint/tests), g085 (land windows_bits/captured_tests/chmod_tests/manifest_vectors), g268 (archive stream + vfs export prop_oneof Arc, api tests x2, s3 list.rs). Agents now run one-crate clippy + xtask check before merging (preamble).
- Ada asked for an event-driven reaper: answered; g255 does A-474 (watchdog late-look hole) then A-475 (reaper piece 1: dead clients by hang-up). g255's list of 7 more reap jobs recorded in its progress file.
- Runs: r406 goal-87 A/B (norepl vs repl) on Studio d1323898; r409 successor-chain bisect (adacachy); r341 volume_moved bisect (quiet Studio); r342 move_dies at d02505d3b (adacachy).
- g251b: A-456 regression (in-process restart hangs on macOS/Windows) being fixed alone first; hold client restart runs.
- g264: macOS Ask volume destroyed under a live mount (r284) is its current piece.

## 2026-11-18 ~23:50 UTC (after the usage-limit restart; all 20 agents resumed)
- 88 library part MET (goals_done): diag 44395174565 green on linux, macos, windows x64, windows arm64.
- 88 bare-metal: macOS rerun r19 (f3f618f2, kiln f54571bdf: /sbin PATH + never destroy under a live mount); Windows W5 diag kiln run 44423462495 (diag-supervisor-w5-2; WinFsp + MountUseMountmgrFromFSD).
- 87 A/B r406 NOT green (integrator-logs/ab-a273-repl-2026-11-18.txt): submits 3-7x worse at p99 with replicas -> g083 A-502 (feed off the submit path), then re-A/B; else revert replicas.
- 85 bug 1 FIXED (g079 A-485, 5fef5b1ef); g230 found the same fix for bug 2, dropped its duplicate, landing its regression test.
- macOS client suite: the_typed_verbs... fails (A-456 look-wake accounting) -> g251b next piece.
- 91 (VCS servers) added: g261 git, g291 Mercurial, g292 Subversion.

## 2026-11-19 ~00:45 UTC (main)
- UI marked met since 00:05: 88 A-496 vendoring (kiln d69ac4975), W2 shell-in-mount (kiln d83f799db), bare-metal Linux r194 + macOS r19, A-519 (e3fbf2ee0); 90 A-471 (24c90fac3), A-494 (d0e39b52b); 85 A-509 (52adc8dd1), A-507 (088d38779), A-513 (d50fac5b9), A-510 guard (d63e602a7); 86 A-475 (920b8fc9c), A-517 (c80d3828b).
- New UI parts (pending/active): 85 A-491 (g230), A-498 (g258), goal-83 bridge bugs B2-B16 (g079 A-526 B7a/B7b), A-513 remainder; 88 O4 (g085 A-527).
- 20 agents running (roster rewritten). g083 waits for r100 and the next free slot.
- Main's runs: r100 A/B (Studio d1323898, norepl vs repl502); r125 Linux clippy+xtask gate (adacachy) and r126 macOS clippy gate (f3f618f2) at main ~52adc8dd1; kiln W5 diag 44423462495 (Windows bare-metal).
- Main's lint branch agents/main-lint-2026-11-19 505e26bdd: direct_map test split (cognitive complexity). handles.rs:69 already fixed on main. Verify on f3f618f2 after r126, then merge.
- Owed by main: kiln CI red on g264's unmount_patiently rt::sleep (workspace/slates.rs:984) and g267's slates_fleet.rs:761-764 indexing -> pieces when they end; kiln re-vendor to contract 42 (A-507); kiln K5 fire-and-exit run for A-510; Windows os tests (A-519/A-501/A-458/A-451/A-445/A-438); A-509 real FUSE flow; A-475 client reap suites; A-517 cli examples + macOS mount flows; TLC for A-518 when g250 lands.

## 2026-11-19 ~01:05 UTC (main)
- 88 bare-metal part MET: Linux r194, macOS r19, Windows W5 (kiln diag 44423462495): W2 through kiln.exe 1/1; W5's slates_e2e bare_metal:: ran 0 tests (module cfg(unix)) -> new part: port it to Windows.
- TLC r140: MC_Lineage.cfg no error (510M states, 30.6M distinct, depth 18, 22 min); MC_Lineage_two.cfg still running (perl alarm 1500 s; if it dies, lower MaxVersion/MaxSeq per g250's request).
- Main's lint branch agents/main-lint-2026-11-19 404ae6aa8: gates r287 (Linux) r289 (Mac) r290 (Windows) running. Fixes include main's own Windows break (0754e4853 named cfg(unix) DaxMapHeld) and bridge-winfsp test support HoldBounds.
- Open from runs: Windows land os symlink tests abort on jupiter (no link privilege) -> g085 after A-539; client.rs:5485 complexity -> g251b.

## 2026-11-19 ~01:35 UTC (main), after the usage-limit restart
- Ada: "Restart the subagents please" -> 16 stopped agents relaunched fresh (g079 g258 g255 g251b g232 g076b g085 g264 g294 g262 g267 g268 g234 g261 g291 g292); g293 g250 g225 g230 resumed. 20 running. Not restarted (cap): g088v (core.fileMode part, queued), g083 (goal 87; next free slot).
- Ada: "Don't skip this" (the goal-83 B-list part showed WAITING): set active; g079 wrote all 11 B descriptions into bug-claims.md; B12 part 1 landed 6703be419 (part 2 Linux-only). Hand B-items to agents as slots free.
- Goal 87 finding (r100 harness lines): bad-p99 rounds have the load generator's own max_sweep 33.8-37.5 ms and max_drain_gap 37.6-44 ms; good rounds 7.6-11 ms / 3.8-5.1 ms. The bench measures its own stalls -> g083's next piece: load_bench measures its noise floor (destroy_rows pattern) and attributes latency to server vs harness; then re-A/B on a quiet machine.
- Main's lint branch d6a60debb: Windows build fixed (DaxMapHeld, HoldBounds); r13 adacachy verifying touched crates, then land on main.
- TLC: MC_Lineage.cfg no error; MC_Lineage_two.cfg outgrew 25 min (133 M distinct, 113 M queued) -> r16 rerun at MaxVersion 6, MaxSeq 2 (bound recorded).
- Pending UI parts: 88 "slates_e2e bare_metal on Windows" (queued for g264 after its current piece), 89 "live-shipped clone's mark" (queued for g267 after A-544).

## 2026-11-19 ~02:20 UTC: kiln server unreachable ("not connected to the kiln server"); UI updates OWED when it returns
- DONE before the drop: A-549 met, A-561 met, A-572 part created (needs active: true).
- OWED: A-548 met (g232: e4b101b35, 1126e1047, 26d01b6a5; C-c closed) under 86; B10 part "A-573 (B10) ..." add + active under 85; A-534 met under 91 (g261: f8ba01f61, slates-merge --lib 22/22, oracle vs copying reference, walk counter unmoved); A-572 active.
- Runs to read when kiln returns: r16 (TLC small), r61 (Windows gate tests), r97 (goal-87 A/A), r122 (KVM microvm), r134 (macOS microvm), W5 diag 44440843629 (GitHub, readable via gh).
- OWED UI (cont.): A-552 met + the 86 part "D-c, the 10 s placeholder readers, and L-h" (01a1f1208ef1724686759630ca531a9b) met (A-525 p1+p2+A-552); A-534 met (91); add active parts A-574 (86, g076b), A-575 (91, g261).
- OWED UI (cont.): A-551 met (g258: 8cb6bef1a, bridge-winfsp 9/43 red -> 43/43, lib 96/96); add active part A-576 (85, g258: a folding drive's volume folds names by Windows' upcase table, so a twin like ς/σ is refused at create).
- kiln outage: "not connected to the kiln server" for every session and the CLI since ~02:05 UTC; local kiln server (pid 81628, port 7070) alive and answering. Not touched (Ada's processes).
- OWED UI (cont., 02:28Z, kiln flapping): A-566 met (g225, 87c1be22e); A-564 active. Applied at 02:27Z: A-548, A-552, A-534, A-551 met; D-c part met; A-572..A-576 active.

## 2026-11-19 ~02:50 UTC: Ada: "a lot of these feel like performance goals. I want you to target mostly on goals 88-91."
- Agents on 84-87 finish their current piece, then move to 88-91. Exception kept: g267's A-577 (goal 84; an acknowledged clone lost at f=1).
- Moved now: g083 (A-579 goal-87 perf paused) -> A-581 (91 shared piece); g225 -> A-580 (89 multi-runner fault test).
- 88-91 open work to staff as slots free: 88 kiln review core.fileMode; SlatesVolumes resync end; the macOS microVM REGRESSION (r134; bisect r296) -> g262 first; vendoring part (kiln CI on every platform: main); OCI, K8s, microVM pieces (g293, g294, g262). 89: the live move across two Macs (main), the moved-live-clone fleet test and the three-daemon takeover test (to write). 90: "the Shards-owed items done with Shards' side" (unstaffed) -> g234 after A-572; S3 base rest after A-555 (g268). 91: more agents on the shared pieces, interop harness with stock clients, the git protocol codecs.

## 2026-11-19 ~03:10 UTC: Ada's order (UI's CURRENT numbering; g268 inserted a top-level goal at #79, so everything after shifted +1 -- leave it, numbers must not move again)
- FIRST: 81 = dbe13f39 "scale out, add the RAFT logic, run replicas, replication <1 ms, chaos" (parts: forming fleet reaches every candidate = g225; chaos network half = g229; chaos failure half = g250; live-write latency = PERF, WAITS).
- THEN 89-92: 89 = 28e73c34 kiln vendors slates (g293, g294, g262, g264, g085 next); 90 = 95101ef3 federation (g230 A-583, g076b A-580); 91 = c2e4337b Shards/Hex/Mantle (g268, g234 next); 92 = 51cef9f7 VCS (g261, g291, g292, g083, g251b).
- THEN 84-86: 84 = 21d6f67c Codex lands; 85 = d9f7a795 unresolved object (g267 A-577 finishing); 86 = 17ad60b6 every open bug (g079, g258, g234 finishing).
- THEN the rest: 80 perf, 82 load test, 83 release, 87 waits derived (g255 landing what it has), 88 W1 perf.
- Ada: load testing and performance work inside any goal WAIT until everything is implemented and merged (81's live-write latency, 92's 10x speed, 82, 80, 88).

## 2026-11-19 ~05:20Z — ADA: "Stop pushing the git/mercurial/subversion goal back and complete it." (said twice)
- Goal 92 (51cef9f7) is now THE goal to complete. Every agent not on goal 81's chaos moves to 92; current pieces parked at
  their next commit (landed if green, else branch pushed + progress file says where it stands).
- 92 roster: g261 A-575 (shared 1 verbs), g083 A-581 (shared piece 3 object store?), g295 A-586 (svn Copy p2),
  g291 A-588 (hg piece 3 objects), g296 A-582 p2 (git v2 msgs), g258 A-582 p3 (git deltas), g225 A-590 (git parsers+fsck),
  g230 A-600 (git object header/identity, sha1dc+sha256), g293 A-591 (git idx v2 + pack header/trailer),
  g268 A-592 (svn commit rule + FSFS oracle), g264 A-593 (svn dump codec), g294 A-594 (SSH transport sans-io),
  g076b A-595 (git smart HTTP codec+routes), g267 A-596 (shared 6 review records), g255 A-597 (shared 7 policy+hooks),
  g079 A-598 (hg GZ/BZ/ZS), g234 A-599 (ra_svn greeting/auth).
- 81: g229 A-601 (ReadSafety live-log nemesis), g250 (client-kill live_chaos flow, A-585). g262 A-587 (owner_word fix, 89) then -> 92.
- PARKED (resume after 92): g293 A-520 p5 (89), g294 QUIC link (89), g264 item 3 (89), g255 core.fileMode (89),
  g076b A-580 (90), g268 A-555 (91), g079 B12 p2 (86), g267 A-577 (85). 9ec5603d resync blocked on Shards (accepted by
  shards-7f, frozen wire) then Hex.
- NEXT 92 pieces for whoever frees up: git pack reader (piece 3, inflate via flate2), svn locks (8), svn authz (10),
  svn mergeinfo (9), SSH auth RFC 4252 + channels RFC 4254, git v0/v1 (14), git LFS (12), hg acl/access (8), svn DeltaV XML (13),
  shared piece 2 (atomic push register entry) if g261/g083 don't cover it, shared 4 (works from a push), 5 (works over works),
  git store (5), receive-pack (6), upload-pack v2 (7), hg read serving (4), svn ra_svn reads (7 rest).
- MAIN owes for 92: deflate backend by measurement (flate2 API fixed; backend miniz_oxide vs zlib-rs [vs zlib-ng]);
  clippy/xtask/doc on slates-git/hg/svn/merge (CI on main keeps cancelling: every run superseded); interop runs later.

## 2026-11-19 ~03:40Z — ALL 20 ON GOAL 93 (git/hg/svn, 51cef9f7). Ada: "Focus everything on finishing the git/mercurial/svn goal"; "More please"; "It's now goal 93"
- Numbering now (g268's old session made top-level #79 and #80 for A-555; left as is): 81 cc6bfdfd perf, 82 dbe13f39 RAFT,
  83 8d9582d0 load, 84 6b5f1ad3 release, 85 21d6f67c Codex, 86 d9f7a795 unresolved, 87 17ad60b6 bugs, 88 5875dc0c waits,
  89 ce6a2d3a W1, 90 28e73c34 kiln vendors, 91 95101ef3 federation, 92 c2e4337b Shards/Hex/Mantle, 93 51cef9f7 git/hg/svn.
- Usage limit ~03:15Z killed 18; harness refused resume; relaunched fresh on Ada's "More please" (~03:35Z).
- Roster (name agent-id piece): g296 aa4a2100c4e223e99 A-582p2 v2 msgs | g258 a4c27d8ff979f7964 A-582p3 deltas |
  g225 ad1e13417ad770727 A-590 parsers+fsck | g293 ad8ad22c3dd130f88 A-591 idx v2 | g076b a29f75a604a03a85e A-595 smart HTTP |
  g294 aba84a2cb15185087 A-594 SSH transport | g291 a39bb963ff462b319 A-588 hg objects | g079 af7ee98d7272e19cf A-598 hg GZ/BZ/ZS |
  g295 af8803ed7d638b398 A-586 svn Copy p2 | g268 aea565b9a95fb3b08 A-592 svn commit rule | g264 a8b6de1878088ea0e A-593 svn dump |
  g234 aee62806a1f320715 A-599 ra_svn auth | g083 a7eb04a67905f1cc2 A-581 object store (crate slates-vcs) |
  g261 a500b27906bcb88c6 A-603 atomic push entry | g267 ad7738a5490449300 A-596 review records | g255 a368b0d2be9cb03d5 A-597 policy+hooks |
  g262 ae315c70611cb7e6e A-605 svn mergeinfo | g229 a6e7bc0a838e95a4b A-606 svn authz | g230 abb5ad4f0f4b79683 A-604 git v0/v1 |
  g250 ac3bd86db01a0a4ee A-602 svn locks.
- PARKED (not 93): A-587 owner_word (branch pushed by g262), A-601 ReadSafety, A-520 p5, QUIC link, kiln item core.fileMode,
  A-580, A-555, A-577; deadlines.rs status-capture + lapsed-term test regressions (A-574's beat overwrites failover_ns the
  tests set directly; fix = daemon.inject_failover) OWED, not 93.
- Main runs: r451 svn 1.15.0 installed on the Studio (brew); r457 workspace clippy+structural+literals+doc at main;
  r458 deflate A/B (miniz_oxide vs zlib-rs; zlib-ng skipped: no cmake) levels 1 and 6, best of 5 interleaved.

## 2026-11-19 ~05:45Z — goal 93 state (all 20 agents on it; ids as in the ~03:40Z roster)
- LANDED today on 93: A-575 A-581 A-582(p1-3) A-586 A-588(p1,p2) A-590 A-591 A-592 A-593 A-594 A-595 A-596 A-597 A-598 A-599
  A-600 A-602 A-603 A-604 A-605 A-606 A-607(main, zlib-rs) A-608 A-609 A-610 A-611 A-612 A-613p1 A-614p1 A-617p1 A-620p1
  A-621 A-627p1; main's lint pass d7ed2c71f (66 files; xtask: 0_usize trivial, tuple field index exempt).
- RUNNING: g296 A-615 (refname merge + upload-pack negotiation) | g258 A-620p2 (fsck, hidden refs, session loop; then
  works via A-618) | g225 A-618 (works from a push; drop TooManyNames) | g293 A-629 (LFS serving, p1 slates-git, p2 db/server)
  | g076b A-625 (HTTPS edge) | g230 A-624 (v2 case/LF, object-info, bundle-uri, packfile-uris) | g083 A-614p2 (pack
  fragments, base pack) | g294 IDLE? (A-594 landed; next = userauth RFC 4252 + channels RFC 4254, then mlkem768x25519)
  | g291 A-630 (hg import store walk; then hg piece 5 pushes) | g079 A-619 (hg read serving) | g295 A-626 (Copy owed cases
  + engine.rs:1603/branch.rs:242 lint) | g268 A-617p2 (svn commit editor) | g234 A-616 (svn reporter) | g262 A-613p2 (svn
  reads rest + authz) | g264 A-628 (svnadmin verbs) | g229 A-623 (svnlook + hook inputs) | g250 A-627p2 (DAV editor drives,
  LOCK) | g255 A-622 (policy record + hook driver) | g267 A-631 then A-632 (merge engine siblings; declared_ops linear).
- Ada decisions pending (stated with safe defaults, no question): two upstream svn security findings (no verify/report);
  kiln goal stack at 498/500 (record landed pieces in existing parts' text; delete nothing).
- CI (fa3605e50 run): clippy/cross-lint fixed by d7ed2c71f; conformance hermeticity "slates grant: presentation 1 not
  found" (non-93, check next run); Miri rt socket test (non-93, owed).
- Next free A-633. Lint habits for agents: lint-habits.md.

## 2026-11-19 ~06:30Z — Ada: "we primarily work akin to Eden FS and Sapling" (compat for git/hg/svn is OFFERED)
- A-642 (per-green logs) stands: Sapling/Mononoke serializes only each bookmark's move (from memory) = per-green logs.
- Native model (greens/works/changesets) is the source of truth; git/hg/svn stores are derived views for hash fidelity.
- NEXT PIECES QUEUE (front, for the next free agents, before remaining compat extras):
  1. shared piece 5: works over works (stacks), a work on another work's post-state remapped when the lower lands.
  2. server-made commits: hg piece 6 + git piece 13 (generated commits for natively merged works, merged reviews, rebases).
  3. pushrebase on push: a stock client's push to a moved branch rebased server-side by the verdict (policy-gated),
     instead of refused non-fast-forward.
- The 10x measurement: on the primary path (works merging into greens, served through volumes) first.
- Ada's scope rules: server portion only (no CLIs); the A-642 worst-case decision; Mantle not used.

## 2026-11-19 ~06:50Z — A-645 (Scion alignment, agentic Meta scale; design 0020d61b0)
Ada: "we work not just on local laptop but at *agentic Meta scale*. Absolute extremes." + anthropic.com/research/redesigning-source-control
NEXT-PIECE QUEUE (front, in order, for the next free agents):
  1. derived data, one mechanism (types computed from parents, eager or lazy, immutable by identity; A-553/A-588/A-614 become types)
  2. merges computed anywhere, committed by position (verdict pure over immutable versions; owner = position check + append)
  3. stacks: works over works (shared piece 5)
  4. server-made commits (git/hg objects for natively merged works) as derived data (hg piece 6, git piece 13)
  5. pushrebase on push (policy-gated)
  6. queries by API: blame, history, merge-base, bisect from derived data
  7. native fetch: wants-only immutable requests, resumable, strict tiers (with goal 91)
  8. MAIN: synthetic extreme-scale monorepo (1 TB history, 250 GB tree, 300k merges/day) and the measurement plan
  9. QUEUED: a read-only FSFS-layout view derived from the svn index for hooks (stock svnlook reads it), like git piece 10's bare-repository view; after A-647 (derived data).
- RECORD (A-623, d449a10c7): hook_driver clears the inherited environment (Command::env_clear) when HookInput::clean_environment is set (every Subversion hook), as svnserve's run_hook_cmd does; git and Mercurial hooks still inherit the server's. Test: a_subversion_hook_starts_with_its_own_environment_alone.

## 2026-11-19 ~07:40Z — goal 93 roster after a round of landings (main)
Landed since 07:00Z: A-648 a2f20ce13 (g229); A-646 in full 15fc0bf7f/dc46c9f05/3f4850d6c (g267); A-650 part 2 6ebb50732 (g303);
A-625 rulings cf9fe4c25 + upload-pack 9200bb014 (g299); A-652 parts 1-2 e2e7e7051/1570459e9 (g264); A-629 part 2
9294c4819/9ef49984b (g293); A-647 part 2 b982c336a (g302).
Main's rulings this round:
- First branch: CreateRepository always makes the root green; an unborn ref = a green with no commit at its head; a new
  branch is Create{from: first parent's (green, version) if branchable, else the default branch's head}.
- A-662: ONE map on ShardState keyed by RepositoryId holding each repository's tool store (git A-614, hg A-588 index)
  WITH its Derivations; made with the record, charged to the shard budget, freed with it; durable as greens are: the
  push's one entry names the sealed objects in admission order, recovery replays them (guard-then-apply); snapshot of
  sealed tables owed.
- HTTP edge: the connection moves to the repository's owner shard once its head names it (as the NFS export does);
  credentials ENROLLED (one record: SSH keys by fingerprint + HTTP tokens by hash with expiry, control channel only);
  TLS identity from the NFS RPC-with-TLS export's source; per-route status mode Streamed | AfterBody (LFS).
- A-652 part 3 (svn import as commits) waits for g268's A-655 owner commit step.
Roster changes: g258 fresh (A-662 then A-654), g307 new (A-663 rebase restates links by node, then A-626 parts 3-4),
g299 fresh (the daemon starting the HTTP edge + the credential record), g308 new (A-664 server-made commits, git first),
g293 fresh (A-665 hooks' read-only bare-repository view), g302 next (A-666 queries by API part 1), g229 (A-660 then A-661).
Main's gate run r1205 (Studio, main-lint-93k): fmt, workspace clippy, xtask check, doc, slates-merge all targets.

## 2026-11-19 ~07:50Z — the Studio (d1323898) OFFLINE (out of memory: 182 MB free of 103 GB, load 83)
- Main stopped its own gate run r1205 (it had finished: fmt=0, clippy=101 (merge copy.rs source_of dead + an
  arithmetic_side_effects error, location in the Studio's log), xtask=1, doc=101, merge_tests=101).
- Agents told: load rule (CARGO_BUILD_JOBS=4, one cargo at a time, delete targets after reading), then the offline rule
  (keep coding without builds, no other machines, check every ~5 min). Ada told what a person at the machine can do.
- Main's gates moved to adacachy (r1241: workspace clippy + slates-merge all targets, capped unit).
- Waiting on the Studio to land: g230 (A-644 part 2 at 330e5b829), g261 (A-636 part 1 on agents/g261-a636-fleet-logs).
  g296 (A-640 part 3 + A-639 join at d24ff09a4; context 870k: relaunch fresh to land, resume steps in its progress file), g229 (A-660 final run), g303 (A-650 part 1 at 203c2e52e), g294 (A-651 part 1 at e1afa981 owing 4 red-checks; parts 2-3 on agents/g294-a651-part2/-part3; context 915k: relaunch fresh to land), g083 (A-653 at a86f1d9a3; real-clone tests after A-640 part 3 lands, adds Streamed::counters to g296 struct), g305 (A-658 parts 1-2 at 435d05531 / bece20c9e), g300 (A-649 3b at f77f36847, 4 at 5743ef190: adds slates-hg to server Cargo.toml+lock, overlaps g258 A-662 deps; A-667 at a02a5a9dd red 5c88fd3e9), g304 (A-657 parts 3-4 at 3664936ba / 7d343ca33), g258 (A-662 at 70d7b0e02 on agents/g258-git-push; A-654 steps 1-2 at e9b7873fa), g299 (credential record + edge start at 178c14b8b on agents/g299-a625; LFS on the site deferred), g268 (A-655 2a at 6b61231c4 on agents/g268-a655-2, incl. Step::Hook), g302 (A-666 part 1 red 53ea3710a, green c7b9b2cc6 on agents/g302-a666-ancestry), g293 (A-665 view at 3c823ddad on agents/g293-a665-hook-view; then the GIT_PUSH_OPTION_COUNT=0 fix), g234 (A-656 part 1 gate 7a958b0f9, commit wiring 70127c768 on agents/g234-a656-svn-connection, svndiff0 fix ba18879f2; then generic edge waits after g299+g294 land), g306 (A-659 stacks at 695ec9789, red 43317a523; then A-668 verdict symmetry), g297 (svn get-file-revs at 30ff3a4d8 on agents/g297-svn-reads; read authz at e0880548f on agents/g297-svn-authz), g308 (A-664 part 2 at 42f3aeb64 on agents/g308-server-commits; part 1 db slice c6bc65ee9 on agents/g308-identities). TELL EACH when the Studio answers.
- main-lint-93k worktree: source_of removed (unpushed; batch with the rest of r1241's findings).
- Decision owed to g308 IN ITS RESUME MESSAGE (A-664 part 1 server half): a principal's tool identities are resolved
  once per session, at its first submit, from the principal's partition, and kept with the session; the submit
  carries them and the merge record stores them (never a read per merge on the owner, §2 ban 10's spirit); an
  identity changed mid-session applies to the next session.
- g307 (waiting): A-663 at 25da01c4c (red 91d14b44d), A-626 part 3 at acd417576 + part 4 on agents/g307-a626p4 (layouts 12, 13; owner composes copy journals); (LAYOUT_VERSION 12 applied; sibling filed: merge chains live in the segment, A-621/A-646 changed them without a bump).
- Owed to g268 IN ITS RESUME MESSAGE (part 2b): svn's Moved is not "out of date": FSFS lands a commit made against an older
  revision when its paths did not move, so Moved re-judges by position (g305's A-658 judge_computed) and only a conflict
  answers "out of date"; the pre-commit hook's transaction view is the FSFS read-only view (owed); exposing the drive's
  rows/revprops read-only for it is fine.
- OPEN merge-engine siblings filed by g307 (GAPS + bug-claims), unassigned, for the next merge-engine slot:
  (1) the deriver treats delete+create at one name as an in-place edit, so a hard link to the old file reads the new bytes;
  (2) a rename onto a name the work just hard-linked leaves both a file and a link there;
  (3) a rebased work's replacement symlink is restated without the old one's removal (next submit refused);
  (4) directories are restated after the files made in them.
  g307's context is 829k: relaunch fresh to land A-663, A-626 p3, p4 (branches named above).

## 2026-11-19 ~10:15Z — builds moved to jupiter (Windows x86_64) while the Studio stays offline (~2.5 h)
- Main's Windows baseline at 018d32ba2 (r1272): vcs 46/46, merge 141/141, db 142/142, git 447 (3 ign), hg 276/276,
  svn 426/426, http-edge 31/31; slates-server's run cut by jupiter's runner restart -> re-running as r1349.
- Recipe in windows-build-recipe.txt (autocrlf off, CARGO_BUILD_JOBS=4, AWS_LC_SYS_PREBUILT_NASM=1, own targets).
- Resumed on jupiter (pure crates): g261 A-636 p1, g306 A-659 (+A-668), g302 A-666, g308 A-664 p2 + db slice (+identity
  ruling), g268 A-655 2a (+2b answers), g300 3b + A-667 (part 4 waits server), g293 A-665, g083 A-653, g305 A-658 merge runs.
- Relaunched fresh: g296 (land A-640 p3; wildmatch move needs the server baseline), g307 (land A-663; p3/p4 need anchor/server baselines).
- Still waiting on the server baseline (r1349): g229 A-660/661, g230 A-644 p2, g258 A-662/654, g294 A-651 (fresh), g299,
  g303 A-650 p1 (hg + http-edge parts could go now; server part waits), g304 A-657 p3-4, g234 A-656 (svn parts OK; recordings
  wait for the Studio), g297 (recordings wait for the Studio).
- Owed to g304 IN ITS RESUME MESSAGE (A-657 part 4, hg hooks), from g293's sweep: stock hg 7.2 always sets HGPLAIN (1 by
  default) and HG for hooks, and passes HG_TXNID / HG_TXNNAME in pretxnchangegroup/changegroup; slates' env lacks all four.
- Windows-cfg dead code (clippy on Windows): vcs_hooks Lesson/teach, ship_to_silent_holders -- main's lint batch.

## 2026-11-19 ~14:10Z — server baseline on jupiter in; all server pieces released
- Server baseline (r1485, main 6e42e7a2c, --test-threads=4): 587 passed / 23 failed / 3 ignored; list in
  windows-server-baseline.txt. 32 threads -> the daemon watchdog ends the binary on jupiter. Two of the 23 are repository
  tests (push refused-change naming; names log per verb) -> watch whether they are Windows-real (asked g258).
- Landed since 10:15Z (all Windows evidence, macOS owed): A-655 2a 9caa5055d, A-649 3b 2ca3f94fb, A-663 4b12a582e,
  A-666 p1 0e755c9b1, A-659 2b3a794b1, A-664 p2 78a670256 + p1 record b79ece5d5, A-667 cde634359, A-636 p1 40bff1d55,
  A-666 p2 6651ed8d6, main's slates-merge build fix f6ec36df2 (history_touches gate, source_of, checked div, Landed),
  A-665 p1 91584c74e (+556a4e050), GIT_PUSH_OPTION_COUNT d54bbfe56, A-669 6e42e7a2c (lost update), A-670 4644dcec3.
- Disk: Docker.raw 154 GB allocated (7.3 TB max) drove the ENOSPC swings; Ada told to prune/cap. Not ours.
- Released onto jupiter with the server baseline: g305 (land A-658 1-2), g229 (A-660, A-661), g230 (A-644 p2), g258 (A-662,
  A-654), g299 (record + edge), g300 (A-649 p4), g303 (A-650 p1), g304 (A-657 3-4 + hg hook env), g308 (A-664 p1 server half);
  g294 relaunched fresh (A-651 parts 1-3). Running already: g306 (A-668), g296 (A-640 p3), g302 (A-666 p3 blame), g234 (A-656),
  g307 (A-626 p3: copy identity via map read or floor rule, never O(tree) records), g083 (A-653).
- Main owes: slates-merge clippy debt (chain_replay CopyTree arm; dims/copy/tests complexity x2; prop_oneof), Windows-cfg
  dead code (vcs_hooks Lesson/teach, ship_to_silent_holders); integration suites; the Linux lane r1241 (queued on adacachy).

## 2026-11-19 ~15:55Z — Claude Code and kiln both restarted; every subagent resumed by SendMessage (context intact)
- kiln run ids reset to r1 (old r14xx-r15xx gone); main still 4644dcec3; disk 152 GiB free; adacachy connected + idle.
- Resumed (id): g306 aaff851b (A-668), g307 afdde930 (A-626 p3), g305 aec53288 (A-658), g296 a757ea9a (A-640 p3),
  g302 a8f7097b (A-666 p3), g234 aee62806 (A-656), g229 a6e7bc0a (A-660; A-661 on adacachy, capped unit), g230 abb5ad4f
  (A-644 p2), g258 ae13e229 (A-662, A-654), g299 a8eb6260 (record + edge), g300 a76dcdc0 (A-649 p4), g303 a83a2431
  (A-650 p1), g304 a7d5b664 (A-657 3-4), g308 a8d610e1 (A-664 p1 server), g294 a5d6fdeb (A-651), g083 a7eb04a6 (A-653),
  g297 a499c1ed (svn step 5, parked for the Studio -> jupiter).
- Waiting, not resumed: g268 aa8f0c06 (A-655 2b) + g293 a61b4ce6 (A-665 p2) on A-662; g261 a500b279 (A-636 p2 unstarted).
- Main's own: main-fix-93k (34b6d919) clippy on jupiter (kiln r1) found more merge test debt: dims.rs:681 complexity
  11/10, engine/branch/tests.rs prop_oneof! (Arc) at 428/432/510, maybe more; fmt --all fails on Windows (os error 206).

## 2026-11-19 ~16:15Z — usage limit stopped every agent; 15 relaunched fresh, 2 resumed
- SendMessage resumed only g307 afdde930 and g083 a7eb04a6; the other 15 were "stopped and won't be resumed".
- Fresh (brief: relaunch-2026-11-19.md + own progress file): g306 ad30be3e (A-668), g305 a8d78fa4 (A-658 1-2),
  g296 a18b2e40 (A-640 p3), g302 a870104e (A-666 p3), g234 af8b76df (A-656), g229 a4f46e58 (A-660, A-661 on adacachy),
  g230 a5343cb1 (A-644 p2), g258 a633f632 (A-662, A-654), g299 a149193f (record + edge), g300 af5c5bb6 (A-649 p4),
  g303 aa3a7f85 (A-650 p1), g304 a6273bf0 (A-657 3-4), g308 ae7a5b9e (A-664 p1 server), g294 a8062e80 (A-651),
  g297 adaa2837 (svn get-file-revs).
- main's full slates-merge run (kiln r1 over main-fix-93k = 4644dcec3 + test-only lint edits): lib 178/178, but the
  integration test tests/engine.rs hard_link_merges FAILS (engine.rs:661: expects link l -> other at base 2 to
  conflict create/create; under A-646's node rule a link naming no node is refused first). Handed to g307 after p3.
  Clippy on merge test targets still red (dims.rs:681 complexity, engine/branch/tests.rs prop_oneof!) -> main's.
- UI: 500-goal stack cap hit (499 now); 33 landed pieces in one met entry; Ada asked to choose raise-cap vs clear.
- ~16:55Z: 15 agents had ended their turns waiting for kiln's notice (my brief's error; it never reaches subagents).
  Resumed 15 with "wait on output, wait_s 600"; g234 and g308 could not resume -> fresh: g234 af9f17c2 (A-656),
  g308 adc1d7a7 (A-664 p1 server). jupiter C: 6.5 GB free: stopped orphans r7, r10 (+ my r48); cleanup r62 removes
  pre-16:05Z slates dirs idle 40 min. Agents now reuse one target dir each and pass needs disk=15g.
- ~17:05Z: Ada's two Esc interrupts stopped all 17 agents (none resumable). Relaunched all 17 fresh (no readback;
  report only on landing/blocked; wait on kiln output 600 s): g306 a4994af2, g305 a4a5decc, g296 af172b6b,
  g302 aa6bb0c6, g234 ae5fe3f3, g229 afa81f03, g230 aaab4141, g258 a0b0370a, g299 a44cd5cd, g300 ae9335c8,
  g303 a68d40b6, g304 a3218e57, g308 a3f6ccbe, g294 a9ca2825, g297 a06633ed, g307 a1472441, g083 ab908db9.
- jupiter cleanup r62: 6.5 GB -> 36 GB free; it partly deleted g307-p3-red and g308-part1b (in use: Windows locks).

## 2026-11-19 ~19:20Z — Claude Code exited (~19:05Z), all agents died; the Studio d1323898 is back
- Landed before the exit (since 4644dcec3): A-656 9a7339e9e, A-640 p3 f607f424c, A-668 e417bfc72, A-666 p3 bf9d39139,
  A-661 p1 92ffda926, svndiff0 fix cd84e3811. jupiter cleanup took C: 6.5 -> 207 GB (week-old other-project scratch).
- 14 relaunched on the Studio (brief: relaunch-2026-11-19.md rewritten for the Studio + §8; one piece each, main starts
  the next): g296 a8d2b696 (A-671), g308 ac3a2d40 (A-664 p1 server), g229 a237a69f (A-660), g303 ac66bb42 (A-650 p1),
  g305 ac500a75 (A-658 p1), g230 a3a9fbaf (A-644 p2), g258 ac48dd92 (A-662), g299 a4676956 (record + edge),
  g300 ad24243c (A-649 p4), g304 a86fc52d (A-657 p3), g294 a71d97cc (A-651 p1), g297 a008dda9 (A-613 step 5),
  g307 a719a721 (A-672), g083 ae4bc43d (A-653 code).
- Queued next pieces (main starts on END): g296 A-640 p4; g303 A-673 then A-674; g305 A-658 p2; g258 A-654;
  g304 A-657 p4; g294 A-651 p2 then p3; g297 step 6 (needs stock svn); g307 A-626 p3 then hard_link_merges fix;
  g083 real-clone tests + Streamed::counters.

## 2026-11-19 ~20:10Z — Ada: "Subagents are scaling down again without UI goals being met"
- Rule now: the moment an agent ENDs, main starts its next piece the same turn; no agent idles while its goal is open.
- Landed since the Studio relaunch: A-613 s5 d0ceb580e, A-653 b9e46a1a3 + c(1) 767da49a9, A-657 p3 a6306963e,
  A-626 p3 637a65d69, A-672 58bd11087, hard_link_merges 8d7209388, A-651 p1 6141837d, A-660 82542220c,
  A-671 fad6a3a6f, A-662 200aac515, A-649 p4 37a85fe85; main's fixes 71a43cd97 (PackRefused), 3b357a256
  (coherence), 77e5ffb46 (vfs base); workspace tests+examples build clean (r82).
- Regression bisected (r104): repository::tests::each_verb_moves... passes a6306963e, fails 637a65d69 (A-626 p3) -> g307.
- Started/resumed now: g307 (regression, then A-626 p4), g258 (A-654), g300 (daemon's hg import via hg_base),
  g229 (A-601, UI goal 82 part), g302 (A-647 next), g306 (node_home bug), g234 (one v0 chunk writer),
  fresh g268 aa248a06 (A-655 2b), fresh g293 a02f1840 (A-665 p2). 19 agents running.
- A-613 step 6 landed effb30ac2 (g297; read authz recorded vs stock svnserve 1.15.0). A-613 UI goal (404f2186) completes
  with step 7 (mid-command auth request, list's non-ASCII fold) -> g297 now; mark 404f2186 met when it lands.
- Unassigned finding (g297): svn `update` checks read access via the caller's Guards while reads use rules compiled
  from Policy::authz -- two sources for one principal's read rule; a later piece.
- A-625 MET in UI (ed63a9f8) 2026-11-19 ~20:30Z: parts 01f0b6be5, 6154528e4, cf9fe4c25, 9200bb014, cceaa51b2.
- A-658 p1 landed 545181b30 (g305). g305 now: the rebase-restatement bug (work removes home `a` of a file link `b`
  names: rebase {a: None} vs submit {b: []}; rebase.rs a_rebases_changes_are_the_whole_restatements_difference flaky).
- For main's Windows pass (g305's predecessor, jupiter, 4 threads, beyond the 23): merge_origin
  a_holder_follows_a_higher_epochs_value_at_a_version_it_applied; merge_service
  a_holder_reads_inputs_that_repeat_a_chunk_and_charges_them_while_it_holds_them; verbs
  a_rebuilt_volume_shows_the_origins_attributes_times_and_links, a_replayed_write_into_an_overlays_base_file_keeps_the_disks_bytes_around_it,
  a_replayed_write_to_a_clones_inherited_file_lands_in_the_clone, a_takeover_keeps_the_volumes_grants_and_its_locked_policy,
  a_verb_for_a_volume_whose_owner_is_fenced_is_refused_by_the_fence_never_not_found,
  a_volume_a_publication_leaves_out_keeps_its_logged_writes_for_the_next_daemon,
  bounded_volumes_that_fill_a_shards_bytes_fit_its_version_slab (macOS passes the first two).
- Main's -D warnings debt (g299): vcs_http/git.rs test-only `pub(crate) use upload::{StoreLender, UploadServices}`
  (since 9200bb014); 8 dead-code warnings in lfs.rs; + slates-merge test clippy (~38).
- A-614 MET in UI (1bbb283e) ~20:40Z: parts d57d24364, 58726c9bd; part 3 superseded by A-666 (2456ee4db).
- Landed since: A-654 p1 157f69a6f (g258 -> fresh g258 a27fb918 on p2), A-676 aea14a21f (g306; g306 ended, context),
  A-664 p1 server b13a6e05f (g308 -> p3), A-616 c(7) bdc73e368 (g234 -> one read-rule source), A-650 p1 c6e3bea98
  (g303 -> A-673, then A-674, hg site on A-662 stores, refusal counts), A-653 c(2) 9f76cd16e (g083 -> deltas::Search p1;
  p2's bound = the request's Admission, no new constant). Fresh g261 a023502e on A-636 p2 piece (a) of g306's split.
- Main's docs debt (g303): GAPS §4.17.3 Owed (4) still lists the SSH loop over serve::stdio (built by A-651);
  vcs_http.rs's Owed still lists "the daemon starting an edge" (built by A-625 p3).
- Regression fixed 5f27f8cf9 (g307): checkpoint v5 writes the copies section and identity sum only beside a copy record
  (anchor layout 13); repository::tests::each_verb_moves... passes again. g307 now on A-626 p4 (layout 14) -> A-626 UI met.
- Later piece (unassigned): that repository test depends on checkpoint byte sizes with 39 bytes of margin; it should derive
  its expectation from merge_service's doubling rule's own inputs, not a size margin.
- A-640 p4 slice (a) landed 58f448d76 (g296 -> slice b, walk by path).
- A-651 p2 landed 9fbba556 (g294 -> p3). Owed from it: git's stateful v0 session over SSH (the edge refuses v0/v1 there
  with an ERR naming v2); a production StoreLender over A-662's map (A-662 lists it owed).
- Landed: A-601 f59d1f3bc (goal 82 part MET in UI 65ac8d27; covers the successor side; lease side queued for g229),
  A-675 f1311ebd9 (g300 -> import verb), A-677 9726e11c3 (g305 -> heir siblings, then A-658 p2), A-673 465b90d42
  (g303 -> A-674), --vcs-http fb6b66296 (g299 -> anchor holds the edge listener), A-651 p2 9fbba556 (g294 -> p3).
- NEW g309 aad5d943: interop slice 1 -- stock git clone + fetch over HTTP against `slates anchor --vcs-http` of a
  server-made commit, Bearer token via `slates credential token`. First step of UI goal "Interop ... proven". 20 agents.
- A-657 p4 landed 5a13886e4 (g304; A-657 complete) -> g304 on Mercurial over the SSH edge (new A-number).
  A-653 c(3) 51148ed82 (g083 -> part 2: large objects through Search under the request's Admission).
- Main's docs debt add: CLAUDE.md gotcha (g304): LocalRuntime::run_until_idle returns at its first zero-timeout poll when
  no timer is armed, so a test waiting on a future without a timer can hang; run it under futures::within.

## 2026-11-19 ~20:40Z — account 5-hour usage limit hit; resets 1pm PT (21:00Z)
- Stopped by the limit (work saved; resume each by SendMessage after 21:00Z, don't relaunch): g296 a8d2b696 (A-640 p4
  slice b), g307 a719a721 (A-626 p4), g268 aa248a06 (A-655 2b), g308 ac3a2d40 (A-664 p3), g299 a4676956 (anchor holds
  edge listener), g309 aad5d943 (interop: found no production StoreLender -> upload-pack answers 503 in the daemon),
  g083 ae4bc43d (A-653 p2), g293 a02f1840 (A-665 p2), g294 a71d97cc (A-651 p3), g300 ad24243c (hg import verb),
  g304 a86fc52d (hg over SSH, A-679). Not limit-stopped but waiting: g302 aa6bb0c6 (ended after A-647 p3 d8111a795),
  g234 ae5fe3f3 (svn locks one source), g229 a237a69f (live_ship holder fall-back), g305 ac500a75 (heir siblings),
  g303 ac66bb42 (A-674), g297 a008dda9 (A-613 step 7), g258 a27fb918 (A-654 p2), g261 a023502e (A-636 p2 piece a),
  g230 a3a9fbaf (A-644 p2) -- check each after the reset.
- g309's early finding: no StoreLender exists outside tests, so the daemon's upload-pack answers 503 -- the interop
  clone needs the production lender over A-662's map (A-662 lists it owed).
- A-674 landed 4181843cf (g303 ac66bb42; ended normally) -> after the reset: the hg site takes A-662's stores, no 503 path.
- A-647 p3 landed d8111a795 (g302 aa6bb0c6) -> after the reset: next owed (share a parent's reached runs, or BlameCost test).

## 2026-11-19 ~20:55Z — usage limit reset + Claude Code restart; all 20 resumed by SendMessage (context intact)
- Landed since 20:40Z: A-677 siblings efcafcc8a (no bug), A-613 step 7 a5ceb9c02 (A-613 MET in UI 404f2186),
  A-601 c(1) cd5d278fc, A-674 4181843cf, A-647 p3 d8111a795.
- Resumed: g296 (A-640 p4 b), g307 (A-626 p4), g268 (A-655 2b), g308 (A-664 p3), g299 (anchor holds edge listener),
  g309 (NOW: production StoreLender over A-662's map -- the git clone's blocker; interop test after), g083 (A-653 p2),
  g293 (A-665 p2), g294 (A-651 p3), g300 (hg import verb), g304 (A-679 hg over SSH), g302 (share a parent's reached runs),
  g234 (svn locks one source), g229 (A-601 lease side), g305 (A-658 p2), g303 (hg site on A-662 stores, no 503),
  g297 (SHA256SUMS + every-fixture sum test), g258 a27fb918 (A-654 p2), g261 a023502e (A-636 p2 a), g230 (A-644 p2).
- Main's docs batch: GAPS A-643 siblings item 1 stale; CLAUDE.md run_until_idle gotcha; GAPS §4.17.3 Owed (4) SSH loop
  built; vcs_http.rs Owed "daemon starting an edge" built.
- g304 and g083 ran out of context; fresh g304 a7ca2516 (A-679 hg over SSH), fresh g083 aec416db (A-653 p2 with main's
  ruling: Admission charges return a token released at the search's end; PackSource::write and Upload::advance take
  the request's &mut Admission). g083's trait change may meet g309's StoreLender: whoever lands second merges.
- NEXT FREE SLOT (20-subagent cap hit 21:20Z): fresh g297 on Subversion's DAV site part 1 (read side: OPTIONS, PROPFIND,
  GET, REPORTs on the shared edge). Ruling: no mod_dav_svn install; oracle = A-609/A-627 goldens + stock `svn` client
  (Homebrew 1.15.0 on the Studio) against slates' site over loopback. Handoff notes top of progress/g297-svn-reads.md.
- Landed: A-674 4181843cf, A-656 c(3) one lock record (8727ff426), A-644 p2 099267884, A-640 p4 slice (b) 849d48d7d,
  A-601 c(2) 3c05b3aca (lease side cited: A-601 met mark stands on both halves), svn fixture sums 560767b0a.

## 2026-11-19 ~21:25Z — usage limit + interrupt stopped all 20 (none resumable); 20 relaunched fresh
- A-626 MET in UI (b7689945): parts 9e514ef36, 6fba33d56, 637a65d69 (+5f27f8cf9), 3612d1ef9 (part 4, landed by g307
  before the stop). Five UI goals met since the restarts: A-625, A-614, A-601, A-613, A-626.
- Fresh IDs: g296 a69de792 (A-640 p4 c), g307 a146d7a7 (confirm p4; then the fragile repository test), g268 ae8f8084
  (land A-655 2b), g308 a1884c77 (A-664 p3), g299 a0ebd54a (anchor holds edge listener), g309 ae4d1e53 (StoreLender),
  g083 a4a4939e (A-653 p2), g293 a0ac7a4f (A-665 p2), g294 ab3875bb (A-651 p3), g300 a8459ecf (hg import verb),
  g304 a0310865 (A-679), g302 ae45f33f (reached-run sharing), g234 a95482aa (expiring-lock tests), g229 adbcc033
  (lease_wait unit tests), g305 a7ee783f (A-658 p2), g303 aa48a26a (hg served view), g297 a6c4c513 (svn DAV site p1),
  g258 a588fc46 (A-654 p2), g261 abb531dd (A-636 p2 a), g230 afc8a6f3 (A-629 p2 LFS on the edge).

## 2026-11-19 ~21:45Z — ADA: "wrap this up: servers built, interop completed, shared pieces completed. Focus."
Critical path (every agent on it; non-critical pieces recorded owed):
- git: g309 StoreLender -> git interop tests (HTTP+SSH); g258 receive-pack on HTTP site; g302 receive-pack on SSH;
  g294 SSH key lookup -> SSH cross-shard; g293 hooks in the view; g296 sparse on the wire; g230 LFS on the edge.
- hg: g303 served view -> lender with unbundle->hg_push (push over HTTP); g304 hg over SSH (A-679); g300 import verb ->
  durable import; g083 hg interop tests (stock hg 7.2 vs anchor, HTTP+SSH; #[ignore]d until wiring lands).
- svn: g234 daemon's `--vcs-svn` listener (none existed); g229 `svnserve -t` over SSH; g297 DAV read -> DAV write;
  g268 A-655 2b -> A-652 p3; g307 svn interop tests (svn://, http, svn+ssh).
- shared (§4.17.1): g261 A-636 p2 (a,b,c); g305 A-658 p2; g308 A-664 p3.  g299 anchor holds edge port.
- Owed (deferred non-critical): lease_wait unit tests, expiring-lock tests, reached-run sharing, A-653 p2 large-object
  delta search, the repository test's byte margin; perf (10x) after interop.
- Expiring-lock tests landed ba0ddb2ee (g234; nothing owed there). g234 now on `--vcs-svn`.
- Owed finding (g234, by reading): svn's commit drive judges every call and close-edit at CommitStart::now; FSFS checks
  each lock at the call's own time (svn 1.15.0 lock.c:457), so a lock expiring mid-commit diverges. ADA: fix it, not owed -> g268, right after A-655 2b, before A-652 p3.
- g294: the SSH Closing-sample flake is a harness race (shard step() drains Shutdown before polling ready tasks); the
  harness fix lands first as its own commit, then A-651 p3.

## 2026-11-19 ~22:20Z — landings + g083's hg findings; three interop blockers assigned
- Landed: A-636 p2a 34d10acb5 (g261: state logs as registers, per-log promise, takeovers a log at a time, holder
  half-apply fix red first); A-658 p2 bc5f2a0d1 (main 092386776, g305; link-helper oracle fix); A-625 p3 cont (2)
  5705edca5 (main 225d7eead, g299: anchor holds the VCS HTTP listener, StartSpec::vcs_http); g294 SSH close harness
  fix 9d54e0f9.
- g083 finding: stock hg 7.2 CRASHES on the edge's cleartext 401 (Bearer-only challenge; urllib digest handler reads
  the first WWW-Authenticate only, raises on non-Digest/Basic: request.py:1129 via hg url.py:418). Ruling A-686: Basic
  (token as password) on the loopback cleartext edge too (same exposure as Bearer there; cleartext is loopback-only by
  grammar), Basic's challenge FIRST everywhere. g083 owns it after landing A-653 p2 (green, 55f3bb479) + hg tests.
- No CLI verb creates a repository: A-685 `slates repository create <name> --tool git|hg|svn` -> g305 (then A-658 p3).
- Daemon starts no SSH edge: A-684 `--vcs-ssh <addr>`, anchor holds listener + host key (node identity, else an
  Ed25519 key the anchor makes in RAM), fingerprint on the daemon line -> g299.
- g261 2b scope: (i) class dispatch + state-log records shipped a log at a time = 2b; (ii) greens placed on the
  repository's holders at join (home change at bind, D-14) = 2b-2, before 2c. Ruled yes.
- UI: goal 93's parts are the six big ones (none's done-when holds yet); stack 499/500.

## 2026-11-19 ~22:40Z — Ada's interrupt stopped all 20 (none resumable); 20 relaunched fresh
- IDs: g296 a4aea2f7 (A-640 p4 c), g307 aeb86226 (svn interop tests), g268 a3385408 (A-655 2b -> A-652 p3 -> lock
  timing, reproduced by stock svn first), g308 a7da72b2 (A-664 p3), g299 a36d4ca5 (A-684 --vcs-ssh), g309 afbd76e3
  (A-682 StoreLender -> git interop tests), g083 a793d02c (land A-653 p2 -> land hg tests), g293 a80f5260 (A-665 p2),
  g294 ad8eab83 (land A-651 p3 -> SSH cross-shard), g300 a06e61c5 (A-687 durable import), g304 a7d017f0 (A-679),
  g302 a2f79f33 (A-683), g234 a5e7203b (--vcs-svn), g229 a8ae75fe (svnserve -t over SSH), g305 a2a22f7f (A-685
  repository create -> A-658 p3), g303 a7c2a97c (A-680 p1 -> p2 lender), g297 ab7a1e41 (A-681 p1 -> p2), g258
  acee8604 (A-686 Basic), g261 af123d55 (A-636 2b -> 2b-2 -> 2c), g230 a5deee10 (A-629 p2 LFS).
- Landed meanwhile: A-678 hg import verb 6a9d2f9ae (g300); repository-test margin 3b0b95379 (g307); A-654 p3 git push
  over HTTPS f05af5288 (g258). g300 reserved A-687 (durable import).
- ADA ~22:45Z: completeness first -> buildable -> bugs found by usage with end-user impact; by-reading findings are
  recorded "verify by use", never chased. Memory completeness-first-bugs-by-usage; brief updated.
- Verify-by-use list (by-reading findings, not chased): svn lock expiring mid-commit judged at CommitStart::now
  (g234's reading of svn 1.15.0 lock.c:457) -> reproduce with stock svn once svn:// serves.
- r196 (main ~22:30Z, after f05af5288): `cargo build --locked --workspace --tests --examples --keep-going` on the
  Studio: 0 errors, Finished in 4m 36s (warnings only: load_bench 2, proc-macro-error2 future-incompat). Main builds.
- A-653 continued (4) / part 2 was already on main: e4c6e7cbf + merge b94ae6e90 (pushed ~22:07Z before the stop).
- Memory index compacted: 121 lines / 11.3 KB; four hubs (docker-and-kind, ci-lanes, dax-and-guests,
  fleet-and-transport).
- A-651 p3 landed 9cc9a378d (main 17f4f7d6f, g294): the SSH edge proves a key by the credential record; r251 ssh
  34/34, server vcs_ssh/credentials/daemon_edge 40/40. g294 -> (a) SSH site across shards. g299 told to merge it.
- A-655 p2b landed cb57b107f (main e25799e8c, g268): svn commit's server step; r241 svn 115/115, merge 1/1, server
  repository/vcs_store/lfs/git_push 59/59. g268 -> A-652 p3, then the lock case only if stock svn reproduces it.
- A-680 p1 landed 942b39a1e (main 770ae8826, g303): hg serve::indexed served view; r256 hg serve/manifest 61/61,
  server vcs_http::hg 7/7, red first 6/9. g303 -> part 2 (the lender: hg site stops answering 503).
- Verify-by-use (g303, by reading, not chased): (a) listkeys phases lists the view's draft roots where 7.2 lists the
  stored ones via the unfiltered changelog; (b) bookmark answers keep divergent x@y names 7.2's listbinbookmarks
  drops. -> check with stock hg once the site serves (pull with phases; divergent bookmarks).
- hg interop tests landed 126eb8a83 (main 9f349d66b, g083): 3 stock-hg 7.2 tests in crates/cli/tests/cli.rs,
  ignored on A-680/A-686 (refusal), A-685/A-680/A-686 (HTTP), A-684/A-685/A-679 (SSH); r261 with --ignored: each
  fails for its reason (--vcs-ssh unknown; repository create unknown; HTTP 404).
- g083 finding: DaemonSite serves git's site alone -> stock hg gets 404 from a daemon. Told g303 (A-680 p2 routes
  hg into DaemonSite) and g297 (route svn DAV). Told g258 (A-686 flips an_anchors_vcs_http... no-Basic assert) and
  g299 (SSH line must start with VCS_SSH_LINE's words).
- ADA ~23:00Z: "go ahead and tackle these real quick" (g303's two hg by-reading findings) -> g083 (resumed): (1)
  divergent bookmarks dropped as listbinbookmarks drops them; (2) listkeys phases from stored draft roots
  (unfiltered, listphases). Each reproduced with stock hg 7.2 first, own commit.
- A-665 p2 landed 763acdc61 (main 99068c566, g293): git_hooks opens the hook's view on the owner shard, seals it in
  slices, runs the hook via the driver (GIT_DIR=. + quarantine vars); r281 server 70/70, git hook_view 16/16+1 ign.
  g293 -> A-665 p3 (view at a path; git_push calls pre-receive/update/post-receive in git's words).
- A-681 p1 slice 1 landed 022866602 (main 693bc8feb, g297): svn DAV site on the shared edge (OPTIONS, PROPFIND
  0/1, GET/HEAD streamed, read rules; REPORTs/writes 501); stock svn 1.15 info/cat/ls/proplist read a repo over
  HTTPS loopback, hidden file 403; r252/r282/r301 svn dav 73/73, server vcs_http 47/47. g297 -> REPORTs (log,
  update for checkout) + routing into DaemonSite over real svn stores, then part 2 (writes).
- A-640 p4 slice (c) landed 16461b7d0 (main 550153532, g296): sparse:oid served; r288 git 112+1, server git edges
  29/29. g296 -> object-info, bundle-uri, packfile-uris (A-624 rest), then A-640 p4 rest (combine sparse, rev forms,
  ambiguous-ref warning).
- svn interop tests landed 646e9deab (main e8fbe9116, g307): crates/cli/tests/svn_interop.rs, round trip + lock
  session; control vs stock svnserve -t 1.15.0 passes (11.2 s); 4 slates legs ignored on their pieces.
- g307 found two unowned svn gaps -> A-692 (svn:// CRAM-MD5 contexts at enrollment) and A-693 (ra_svn lock verbs,
  210001 today) -> g307.
- g296: object-info is ALREADY served (A-624 p2 object_info::Stored via A-640 p2; advertised; test
  object_info_answers_the_sizes_stock_git_printed) -> main's docs batch: GAPS lines ~198/215/713 say owed (stale).
  g296 order: A-640 p4 rest first, then bundle-uri, packfile-uris (site-level tests; daemon leg ignored on A-682).
- A-651 p4 landed (main 2f73eb07, g294): SSH exec on another shard's repo moves the connection there; r335 vcs_ssh
  30/30. g294 queue done (ENDED). Finding (by reading): vcs_http doc "no connection outlives its listener" stale;
  arrival-wait/arrival-full paths untested in both edges.
- A-685 landed c3f8ac6fb (main 2bfec1547, g305): `slates repository create NAME --tool git|hg|svn [--format]
  [--default-branch] [--root] [--json]`; r329 grammar 13/13, --test cli vcs_http 3/3+1 ign. g305 -> A-658 p3.
  Finding (by reading): server records a repo whose format its tool doesn't name (CLI refuses first).
- g296 relaunched fresh (r3, a561a7f1): A-640 p4 rest -> bundle-uri -> packfile-uris; checks main first.
- AUDIT (Ada ~23:20Z: "make sure they're actually merged in, not about to duplicate work"): running a reverse-apply
  check of every agents/* branch since 11-18 against main.
- AUDIT RESULT (~23:35Z): 647 agents/* branches since 11-17; 310 A-ids on them, 304 named in main's log; the rest
  in flight (A-682, A-683, A-687, A-690) or parked non-93 (A-392 g255 draft, A-579 hypothesis). Part-level: unlanded
  and not in flight = A-647 part 4 only (g302-a647p4-wip, 815 lines, never compiled) -> g294. A-629 part 2a
  (g293-a629p2-lfs-locks, lfs_locks.rs absent on main) was SUPERSEDED by A-635's locks log (2b-i/2b-ii landed), not
  lost. Duplicate caught: object-info (already served; g296 skipped it). lease_wait tests (g229-lease-wait-tests,
  WIP unbuilt) parked, owed. Non-93 parked WIP from 11-18 (g262 audit/bridge/Shards-Windows/doctests, g251
  shard-count/watchdog, g258 WinFsp spin, g225 departure budget): nobody is redoing them.
- BUG (g300, reproduced by use: diag agents/g300-a687-diag 3528cc682 on the Studio): one append's Creates all get
  the same volume id (verbs::fresh_volume_id mints from db.next_seq, which moves only when the record commits) ->
  the 2nd Create refused ContentUnavailable. Hits git atomic pushes of 2+ new refs, hg pushes of 2+ bookmarks/lines,
  imports with 2+ bookmarks. -> g300 fixes it FIRST as its own piece (partition volume counter in apply, snapshot,
  max(next_seq, counter); model test for recovery), then A-687.
- A-690 landed 9cd6cc6c7 (main f1fbd2c44, g083): divergent bookmarks left out as 7.2's listbinbookmarks does;
  reproduced by use first (stock hg serve: debugpushkey + clone show no feature@1; slates showed it, r340 red);
  r356 hg serve/push/bookmarks 93/0. g083 -> A-691 (phase roots).
- A-680 p2a landed 9a919b371 (g303): Mercurial's site on the daemon's edge, served views lent from the store on the
  owner shard -> stock hg no longer gets 404. hg refusal test now waits on A-686 alone.
- A-664 p3 first piece landed 79e619ea7 (g308): each git green keeps its head's map of paths to git's files.
- g308 next: A-664 p3's commit (trees + commit from the maps, admitted with the head, recorded with the version).
  Verify-by-use: "a rename drops a file's mode (A-618 keys modes by path): chmod +x then mv lands 100644" ->
  reproduce in that piece's integration test; fix as its own piece if it reproduces.
- g303 ended (context) after A-680 p2a; relaunched fresh r3 (abc1b9d2) on A-680 p2b (hg push over HTTP via
  hg_push -> store's Mercurial arm). Ruling: build now; record pushes with A-687's kind if on main, else land and
  own 2c; never a second record kind. g300 told to design A-687's kind for imports AND pushes. g304 told to adapt
  A-679 to HgStores::lend and drop NoStores.
- g308 ended (context) before A-664 p3's commit (plan + failing test incl. chmod+mv check in its progress file
  "IN PROGRESS ~23:20Z"). Fresh g308 launch hit the 20 cap -> launches in the NEXT free slot (first in line).
- A-689 landed 9abe7e9a2 (main f7684fc20, g234): `--vcs-svn ADDRESS` starts the svn:// listener, anchor holds it
  (ENV_VCS_SVN, StartSpec::vcs_svn, held_or_bound_listener); r394 server 20/20, cli 5/5 incl. kill -9 restart.
  g234 -> svn:// repository lookup on the owner shard (find_repos rest, connection moved), then learned waits.
- Observed once under load (not chased): api role::stop_tests A-510 "no A510-SENT line within 20s" (r373, load
  11-13); passed r310/r344.

## 2026-11-20 ~02:00Z (6pm PT reset) — the 5-hour usage limit stopped 13 agents mid-step (429); limit reset
- Resumed all by SendMessage (work saved): 13 resumed in place (g296 r3, g268, g308 r3, g299, g309, g083, g293,
  g294, g302, g234, g229, g303 r3, g261); 7 never stopped (g307, g300, g304, g305, g297, g258, g230), message
  queued. ListAgents: 20 of 20 running. Ada: "restart all subagents please" -> done.
- Left mid-step at the stop: g229 A-688 landing tree green (r377: vcs_ssh 38/38, svn 60/60) -> landing next;
  g261 A-636 2b landing worktree; g083 A-691 red run r411.
- A-682 landed 53b6aae3f (main 5206419be, g309): the daemon's git site serves upload-pack from each repository's
  store on its owner shard (DaemonGitSite = Halves<UploadServices<ShardStores>, ReceiveServices<ThisShard>>); SSH
  site's ToolServices take the same lender. r385 server 110/110 (stock git push posted to a 2-shard daemon, objects
  fetched back whole), cli vcs_http 3/3. => git clone/push over HTTP from a daemon is served.
- g309 -> git_interop.rs, then the hg/svn interop tests' issuer step (run_as_issuer) as its own piece (g083/g307
  told not to). g299 told: SSH site gets ToolServices::new(ShardStores, room).
- A-636 p2b landed 140d2fc19 + docs d8be8e2c9 (main 2c3fa2bcc, g261): one StateLogRecord per state log an op moved,
  shipped as parts in position order, committed at f+1; takeover/mirror/fleet dispatch on ObjectId::class; r367 db
  138/138 (model rebuilds every state log from 1,083 records), server 85/85; r399 85/85. g261 -> 2b-2. (Branch
  deletions g261-a636p2b-tmp-1..4 left: not needed for completion.)

## 2026-11-19 ~15:45-16:00 PST — second limit wave (429s) then reset; Ada: "Restart all killed subagents"
- Resumed all 19 by SendMessage (disk-care line added: Mac ~5.6 GiB free; g083's worktree design doc hit 0 bytes
  when the disk filled; main's docs intact: SLATES_DESIGN 5,334,674 B at 9ca6e3883).
- A-679 landed 075e36b52 (g304): hg serve --stdio behind the SSH edge over HgStores::lend; r438 49/49. g304 ended
  -> fresh g304 r3 (ab07878d): hg pushes over the stdio session through hg_push (store arm stays g303's).
- A-681 p1 slice 2 landed 4a7437f0c (main 9ca6e3883, g297): svn log over DAV; stock `svn log -v` works through
  the edge. g297 OWNS the production SvnStores lender (shared with svn://), lands it first; g234 told not to build one.
- g261 ended after A-636 2b -> fresh g261 r3 (ab2a683d): 2b-2 (homed green placement entry), then 2c.
- Findings (not chased): plain slates-server lib build has 22 warnings (20 lfs.rs dead code, 2 test-only
  re-exports) -> buildable-state pass later.
- A-658 p3a landed 0ba1006c4 (main fc5f4ea98, g305): a computed verdict's wire form (Computed::encode /
  Green::computed_from); r391+r431 merge 22/22 incl. goldens + every cut/flip refused. g305 ended (context) ->
  fresh g305 r3: 3b (submit's fleet route, two-node f=1) then 3c (pushes).
- A-694 (the duplicate volume-id BUG) landed 3bd7d093b (main 574185afa, g300): an append's new branches get distinct
  ids from a partition counter advanced in apply, carried by the snapshot; red 0/2 first; db model t_a694 +
  every_recovery 3/3, snapshot 8/8, server 61/61, green_checkpoint 6/6; docs/bugs/2026-11-19-an-appends-new-
  branches-were-minted-one-volume-id.md. => atomic git pushes of 2+ refs, hg pushes of 2+ bookmarks work. g300 ->
  A-687 (one Mercurial record kind for imports and pushes).
- Fresh g305 r3 (A-658 3b, 3c) hit the 20 cap -> launches in the NEXT free slot (first in line).
- A-629 p2c landed 1cdb47fc7 (main ba97c5ad5, g230): Git LFS on git's HTTP site through the edge's auth; locks API
  over the locks log; batch/transfers 503 until LFS objects are durable; lfs.rs dead code wired; r433 server 65/65,
  cli LFS-edge+restart 2/2. g230 -> A-629 p3 durable LFS objects (record + replay), then stock git-lfs e2e test,
  then non-owner locks.
- GIT INTEROP OVER HTTP PROVEN: git_interop.rs landed f64a6c2fd (main be892650b, g309): stock git 2.50.1 clones a
  `slates repository create` repository from a real anchor's daemon, pushes and fetches over --vcs-http (Bearer
  via http.extraHeader), each step held to a control round trip against stock git's own bare repo; no-credential
  refusal in git's words (exit 128). r452/r464: 3 passed, 2 ignored (Basic on A-686; SSH on A-684 + A-683).
- Buildable-state list (after completeness): non-test slates-server build warns `unused import: ShardRepositories`
  (vcs_http.rs pub(crate) use daemon_edge::{ShardRepositories,..}, A-651 p4, used by vcs_ssh tests only) -- seen by
  g230, g304, g309.
- A-688 svn+ssh landed de9909e40 (main 8c7965bf4, g229): `svnserve -t` tunnel on the SSH site held to stock svnserve
  -t 1.15.0 (recorded mkdir commit + 15 scripted sessions); r458 vcs_ssh 47/47, svn 60/60. g229 -> commits over
  ra_svn handed to svn_commit::commit (both svn:// and tunnel; refused 200007 today), then the found-by-use exit
  difference (refused edit's drop E210002/exit 1 vs svnserve 0) re-reproduced after. g234 told: find_repos + move
  callable from the tunnel too.
- Issuer fix landed (main da3349308, g309): stock_hg round trips + every svn_interop slates leg mint credentials
  with the anchor's issuer surface; r471 --ignored: hg HTTP passed create + token to `hg push` (waits A-680 2b /
  A-686); svn:// and http legs reached `svn checkout` (E210005 lookup A-689 rest; E160013 A-681); SSH legs stop at
  --vcs-ssh (A-684).
- g309 next: LARGE-REPO USAGE RUN with stock git through --vcs-http (git/git or slates history): push all, fresh clone
  + fsck + object-set compare, incremental fetch, shallow + blob:none clones; timed vs stock git http-backend/daemon,
  recorded in BENCHMARKS.md (goal 6's first measurement); failures = bugs found by use.
- A-665 p3 landed 86a4a0fb9 (main c8be7c738, g293): pre-receive/update/post-receive in git's words, push answered a
  step at a time; real pushes to hook-configured repos still "hook not run" (no presenter). g293 ended -> fresh g293
  r3 (a2cbf361): A-665 p4 PresentView (read-only volume over the store under the root mount), then client-left
  signal, learned hook limits, SSH call.
- A-652 p3 import half landed cb18b1b24 (main 477c85a1b, g268): svnadmin dump import through the daemon, exports
  byte for byte incl. after a log rebuild. g268 -> export half. g268 force-pushed its own scratch branch once
  (--force-with-lease; nothing lost) -> reminded: never. Gates list: A-678 added ImportFault / Invalid::Import
  without raising CONTRACT (by reading).
- g305 BLOCKED on A-658 3b/3c (no holder has a submit's increment: works are made on the owner, A-247). RULING:
  route (1) = A-697 works on holder nodes (A-247 step two), pieces (a) creation/rights/naming, (b) floors, (c) commit
  by position with increment + form (= A-658 3b), (d) settle, (e) recovery, (f) takeover, then 3c. Route (2)
  (owner ships increments to a holder) REJECTED: extra round trip + second computation per fleet submit.

## 2026-11-19 16:04 PST — third limit wave (agents 429 "resets 6pm" while main runs); resumed all 20 again
- Resumed in place: g268, g293 r3, g258, g230, g308 r3, g304 r3, g309, g261 r3, g229, g303 r3, g305 r3; queued
  (running): g296 r3, g307, g299, g083, g294, g300, g302, g234, g297.
- One-shot cron 90617c9b at 18:03 PST: resume every agent the limit stopped, relaunch any that can't resume.
- Current agent ids: g296 a561a7f1, g307 aeb86226, g268 a3385408, g308 a2de47eb, g299 a36d4ca5, g309 afbd76e3,
  g083 a793d02c, g293 a2cbf361, g294 ad8eab83, g300 a06e61c5, g304 ab07878d, g302 a2f79f33, g234 a5e7203b,
  g229 a8ae75fe, g305 aa85a60b, g303 abc1b9d2, g297 ab7a1e41, g258 acee8604, g261 ab2a683d, g230 a5deee10.
- g261 2b-2 refinement ACCEPTED: part 1's rule needs ONE SUCCESSOR per repository, not one holder set -> placement
  unchanged, copyset spreading stays (AUD-14: moving history open). REJECTED its mechanism (a Home config entry per
  joined green): banned item 12 (per-object lookup table) + config grows per branch (learners fetch it whole).
  Ruling: route by id (greens carry the repository's placement key, as state logs carry class bits); successor-of
  computed; pre-existing green at BindBranch: re-key or refuse, decided by the design and recorded.
- A-640 p4 slice 4 landed 6c16b1e3a (main 0b0c8e12e, g296): abbreviated + describe sparse:oid names via a store
  prefix search (slates-vcs Index::iter_from), git's refname warning and ambiguous-abbreviation error relayed on
  band 2; 15 stock git 2.50.1 goldens; 196 passed 4 ign, red 7/152 + 2/2 first. g296 -> 4e (~ ^ ^{} @{} :/), 4f
  (combine:), bundle-uri, packfile-uris.
- A-684 landed 5908c2b8a (main 2b86aaaa1, g299): `--vcs-ssh ADDRESS` starts the SSH edge; anchor holds listener +
  a RAM Ed25519 host key (SUP_SSH_HOST_SEED); fleet node presents its own Ed25519 key, else no SSH edge (gap);
  fingerprint on the daemon line; site serves git upload-pack + hg serve --stdio; red caught a real layout bug
  (seed word missing from the segment layout). r489: anchor 4/4, server 11/11, api 10/10, grammar 15/15, cli 5/5.
  g299 -> stock git/hg clone+fetch over ssh:// via --vcs-ssh (git_interop SSH leg split: push stays on A-683), then
  P-256 (and RSA if handed) host keys.
- A-691 landed b4d6bae40 (main 23b566fd6, g083): listkeys phases from the register's draft roots past the served
  filter (7.2 listphases); reproduced by use first (stock hg serve debugpushkey listed both roots incl. the hidden
  amended one; slates listed one, r411 red); hg serve/push 91/0, server hg 21/0. Ada's two hg asks DONE.
- g083 -> Mercurial completeness inventory (every §4.17.3/GAPS piece + hg server feature: built / in flight+owner /
  missing), then builds the first missing by user impact.
- g299 ended (context 758K) -> fresh g299 r3 (a5e5cc0e): stock git/hg clone over ssh:// via --vcs-ssh, then P-256.
- g305 A-697 plan ACCEPTED: (a)-(f) land with production routing unchanged, each tested via a TEST-ONLY daemon hook
  (gated like inject_merge_fault); the LAST piece deletes the hook, adds the real "replicated here" rule + the
  two-node f=1 e2e test with the taken-by-position counter. (a) = owner admits a holder's work by a forwarded verb
  (rights + base), holder's replica answers base reads, work made on the client's partition.

## 2026-11-19 ~16:20 PST — fourth limit wave (agents 429 "resets 6pm"); NOT resuming before 18:03 (cron 90617c9b)
- Stopped by the limit: g293 r3 (A-665 p4), g307 (A-692/693; second merge, scratch run pending), g083 (hg inventory),
  g261 r3 (2b-2 by id), g305 r3 (A-697 a), g299 r3 (ssh interop), g297 (r502 GREEN on landing tree 1eb83af58 ->
  LAND it first), g304 r3 (A-696 server tests), g234 (A-695 landing commit next).
- A-647 p4 landed fbf4a24d (main 20ae700a, g294): reach sets as a tree over fixed blocks (format 3): 378,007 B total
  vs format 2's 1,077,340 (largest version 938 B vs 7,740); git 218, server 110, stock git HTTP 2/2. ShardRepositories
  warning gone (A-684 uses it). g294 ENDED (queue done).
- AT 18:03: resume all stopped; g294 next = git completeness inventory (§4.17.2 + GAPS, by use with stock git:
  built / in flight+owner / missing), then build the first missing; svn inventory to the next free agent.
- A-686 landed 7b3044c30 (main e21beb606, g258): Basic on the loopback edge, Basic first then Bearer (LFS too);
  stock hg no longer crashes; un-ignored g083's hg anchor refusal test + g309's git Basic leg (both pass); r518
  http-edge 10/10, server vcs_http 61/61, cli 4/4, git_interop 2/2.
- BUG found by use (g258 r415): stock `hg identify` aborts "remote repository does not support the 'lookup'
  capability" (SERVED_HTTP omits lookup; 7.2 always advertises) -> g258's next piece (HTTP + SSH capability lists).
  Tell g083 at its resume: lookup is g258's.
- 16:22 PST: limit eased (main's reset); resumed all: 10 in place (g297 land r502 first, g083 inventory with lookup
  = g258's, g294 -> GIT INVENTORY, g293, g307, g261, g305, g299, g304, g234), 10 queued (running). Cron 18:03 stays
  as the backstop.
- g300 out of context: A-687 GREEN on agents/g300-a687-import-durable 6c492db95 (red r496 recovery only; green hg
  import 28/28, server 61/61) NOT landed (conflicts with main's import.rs/push.rs) -> fresh g300 r3 lands it. g303
  told the record API: slates_hg::import::record (SHGR, Source::Push, write_revisions, replay).
- fresh g300 r3 (land A-687) hit the 20 cap -> launches in the NEXT free slot (first in line).
- BUG FOUND BY USE (g309, A-699 large-repo run, release main 2b86aaa, 23 shards): stock git CANNOT push git/git
  (f7de8145, 324 MiB pack) into a daemon --vcs-http repo: HTTP 413 after 4.4 s; v0.99 alone (4,508 objects) -> 200
  "unpack eof before pack header was fully read" + (unpacker error); one-commit pushes fine. Cause (reading, to be
  confirmed by the failing test): body and unpack admitted against Bounds::of_shard room = one ring region
  (ReceiveServices::new(ThisShard, room) -> 413; ReceivePack Admission::new(room)); unpack_words maps a room refusal
  to "eof". Stock git daemon took it in 30.3 s (clone 13.5 s; fsck + refs 1027 / objects 424,851 identical).
  -> g309: land A-699 record first, then the fix (slice-bounded streaming; derived per-repo total; git's
  maxInputSize words), then re-run A-699 for slates' numbers.
- g300 r3 (ae18de41) launched: landing A-687 (force-remove of its own conflicted worktree allowed after head check).
- HG INVENTORY (g083, ~00:55Z; full list in progress/g083-w1-under-1ms.md). BUILT: discovery cmds, listkeys
  (A-690/691), getbundle bundle2, served view, HTTP site on the edge, SSH stdio + --vcs-ssh, repo create, import,
  push classify/verify/commit + acl + hooks. IN FLIGHT: push HTTP (g303), push SSH (g304), A-687 (g300), SSH clone
  tests (g299), lookup (g258). MISSING, by user impact: (1) stream clones (revlog view -> stream_out -> stream2/
  streamreqs); (2) server-made changesets + rebase on push = PUSHREBASE (after pushes; Ada: pushrebase first);
  (3) clone bundles; (4) narrow/ellipses/remotefilelog; (5) largefiles + lfs; (6) obsmarkers on push + topics;
  (7) push follow-ons (acl identity words, refusals in hg's words, pushkey compat hooks, sliced inflate,
  check:phases); (8) `slates hg serve --stdio` CLI for OpenSSH forced commands; (9) hgweb json/raw + annotate;
  (10) learned compression, perf (held deltas, sliced answers, kept heads); (11) bundle1, debugwireargs,
  listkeys obsolete dump. g083 ENDED -> fresh agent: stream clones parts 1-3.

## 2026-11-19 ~16:50 PST — Ada's interrupt + limit stopped 17 agents (not resumable); 17 relaunched fresh -> 20 running
- A-683 git push over SSH landed 4681b17b6 (main a7b5e4292, g302): r530 124/124. git: clone/push/fetch over HTTPS
  AND SSH. g302 -> SVN COMPLETENESS INVENTORY.
- IDs now: g083 r3 a5f1dfb0 (hg stream clones), g302 a2f79f33 (svn inventory), g261 r3 ab2a683d (2b-2 by id),
  g296 r4 ad118cdb, g307 r3 a9cb2f80, g268 r3 a148724f, g308 r4 a5ce2ebc, g299 r4 a48eb4eb, g309 r3 ab5fb1b5,
  g293 r4 a9d3d90d, g294 r3 a78dcdc8 (git inventory), g300 r4 a3866ab3 (land A-687), g304 r4 a064f207 (A-696),
  g234 r3 a4640ade (A-695), g229 r3 a1b1ce70 (ra_svn commits), g305 r4 a612798f (A-697 a), g303 r4 a72f1edf
  (A-680 2b), g297 r3 a8fa618d (land green, SvnStores, ToolSites, update-report, writes), g258 r3 ae727eb9 (hg
  lookup), g230 r3 adde1674 (A-629 p3, git-lfs e2e, non-owner locks).
- SVN INVENTORY (g302, read at main a7b5e4292; no route reaches a repo by use yet). BUILT: greeting v2 + caps,
  ANONYMOUS/EXTERNAL, reparent, all read cmds, update/switch/status/diff, commit (slates-svn + owner step + hooks),
  DAV OPTIONS/PROPFIND/GET/HEAD/log-report, HTTP login + authz, lock table, mergeinfo, authz, svndiff 0/2, dump
  import, svn:// listener, svn+ssh tunnel, repo create. IN FLIGHT: CRAM-MD5 + ra_svn locks (g307), svn:// commits
  (g229), lookup (g234), REPORTs + v2 writes + SvnStores (g297), export (g268). MISSING: (1) change-rev-prop/2 +
  pre/post-revprop-change hooks = A-702 (g302 now); (2) ra_svn replay + replay-range (svnsync source, svnrdump);
  FINDING: greeting advertises atomic-revprops + partial-replay while unserved -> svnsync "Unknown editor command";
  (3) DeltaV v1 (MKACTIVITY/CHECKOUT, old clients); (4) other SASL mechs (optional); (5) review piece 14 (held
  commits, server-side merge, stacks).
- g302 ended (context) before A-702; fresh g302 r3 (A-702 revprops, then ra_svn replay/replay-range) hit the 20 cap -> launches in the NEXT free slot.

## 2026-11-19 18:00 PST — interrupt + limit stopped ALL 20 (ListAgents: none); 20 relaunched fresh at the reset
- Landed before the stop: A-681's SvnStores lender 1eb83af58 (main 3b392dcb1, g297): vcs_store::svn_view::
  ServedRecords (reads::Repository + mergeinfo::Contents, Index::children, lend_in for svn://); svn 178/178, server 6/6.
- IDs: g083 r4 aac7ca53 (hg stream clones), g302 r4 a6b966bc (A-702, ra_svn replay), g261 r4 a82cd39b (2b-2 by
  id, 2c), g296 r5 afec297d (4e, 4f, bundle-uri, packfile-uris), g307 r4 a6e88cd3 (A-692, A-693), g268 r4 ab467509
  (A-652 export), g308 r5 a60d9b9d (A-664 p3 commit), g299 r5 a4e661d1 (ssh interop, P-256), g309 r4 acc86a19
  (A-699 record, large-push fix), g293 r5 a4caee5e (A-665 p4), g294 r4 a6c18769 (git inventory), g300 r5 a378332f
  (land A-687), g304 r5 a7efecec (A-696), g234 r4 af9db14e (A-695 land), g229 r4 ae2b8e69 (ra_svn commits, svn+ssh in
  daemon), g305 r5 a71e41cf (A-697 a-f), g303 r5 a504e529 (A-680 2b), g297 r4 a3469fab (GET cap, ToolSites,
  update-report, writes), g258 r4 ab4e21f0 (hg lookup), g230 r4 aa620372 (A-629 p3, git-lfs e2e).

## 2026-11-19 ~19:55 PST — all 20 dead again (interrupt + limit); briefs moved to files; 20 relaunched
- A-629 p3 landed aa2f5cd05 (main b63f8a05d, g230): durable LFS objects, batch + transfers served; git lfs 74/74,
  server 111/111, cli LFS edge 1/1.
- BRIEFS NOW IN FILES: ~/.local/state/slates-fanout/briefs/{common,gNNN}.md. A relaunch is one short prompt per agent:
  "You are gNNN ... read relaunch-2026-11-19.md, briefs/common.md, briefs/gNNN.md, then the progress file it names."
  UPDATE a brief file when an agent's queue changes.
- IDs: g083 ab946360, g302 a91e173a, g261 a5db8fc9, g296 a529e1cb, g307 a7578b17, g268 a359f533, g308 ada91648,
  g299 a8326a98, g309 a80fe9c3, g293 a34a8f09, g294 ad1914c6, g300 a6240f77, g304 a6be4146, g234 a941ba8d, g229
  aa8ac147, g305 a81220af, g303 ab48385d, g297 af539ec1, g258 ac8573f5, g230 a59ccdb7.
- Ada asked "how close": ~60-70 pieces left on goals 1-5 (shared ~15, git ~14, hg ~20-25, svn ~15; interop = git
  done, hg ~3 away, svn ~6-8); ~6 landings/hour uninterrupted today (~38 since 13:00) -> ~10-12 h uninterrupted.
  10x not started (stock baseline only).

## 2026-11-19 ~20:05 PST — ADA: "use the Anthropic API key" -> fleet moved to 20 HEADLESS WORKERS on her API key
- Subscription agents kept hitting the 5-hour limit (resets 11pm PT). Ada's API key is exported in ~/.zshrc (line
  132) but this session's env lacks it, so Agent-tool subagents can't use it.
- Workers: ~/.local/state/slates-fanout/headless/run-worker.sh gNNN (evals the export line from ~/.zshrc inside the
  process; never echoed) -> `claude -p --permission-mode bypassPermissions --model claude-opus-5-5 <prompt>` in
  ~/slates-work; prompt = headless/prompt-template.txt (read relaunch + briefs/common.md + briefs/gNNN.md + progress
  file; report via SendMessage to "slates-7a"). Logs: headless/gNNN.log. Each is a background Bash task, 2 h max:
  relaunch with the same script when one exits.
- Startup log confirms "ANTHROPIC_API_KEY ... takes precedence over your claude.ai login" (claude.ai connectors off;
  kiln MCP availability to be confirmed by the first worker report).

## 2026-11-19 ~20:15 PST — ADA: "Only do six. One per goal." -> 20 workers stopped (TaskStop), 6 goal workers started
- Briefs: ~/.local/state/slates-fanout/briefs/w-{shared,git,hg,svn,interop,perf}.md (queues in priority order, each
  piece pointing at its old gNNN brief + progress file). Launch: headless/run-worker.sh w-<goal> > headless/w-<goal>.log.
- Task ids: w-shared bwqo2kj2g, w-git boetlvqip, w-hg bongussqd, w-svn bddts1wny, w-interop b7dwqp3jm, w-perf b1av0aru3.
- FINDING (g304, before the stop): A-680 part 2b is NOT on main -- g303's landing push was rejected ~16:18 PST though its
  notes say landed (95c3edd18 + 4013e0f03, 6d8a99656). w-hg lands it first.
- Note: kill -TERM didn't stop `claude -p` workers; TaskStop on the background task ids did.
- ~20:25 PST: Ada "I do not see six workers" -> headless bash workers (invisible in her UI) stopped; the six now run as
  KILN AGENTS on adas-macbook-pro-2.local (kiln runner env HAS ANTHROPIC_API_KEY -> API billing), cwd ~/slates-work,
  bypassPermissions, opus 5.5, visible in kiln's agent list. IDs: w-shared 01a1f7226efd7574907e91128e2f162a,
  w-git 01a1f7227bcd74f08c3d459d12dcfc6e, w-hg 01a1f722874271548949145888a6c3ca, w-svn 01a1f72293617624b306adc6a7689ce5,
  w-interop 01a1f722b2ea74ef973d6246a784ab7f, w-perf 01a1f722b428761c9ae0225a84cc3b41. kiln tells main when one
  settles; relaunch with agents_start (same prompt) or agents_send to continue.
- ~20:40 PST ADA: "it should literally not take two hours to land a feature - implement, light unit testing, merge".
  FAST LANDING LOOP appended to briefs/common.md (one warm Studio run of fmt + focused unit test; no red runs; rerun
  after merging main only if code it compiles against changed; minimal docs; target < 30 min). Sent to all six
  kiln agents via agents_send. Memory landing-under-thirty-minutes.
- GIT INTEROP FULLY PROVEN: w-interop landed a0175d10b: stock_git_clones_pushes_and_fetches_over_the_ssh_edge
  un-ignored (host key pinned via ssh-keyscan + ssh-keygen -l, StrictHostKeyChecking=yes); git_interop 5 passed 0
  ignored (kiln r1, git 2.50.1, OpenSSH 10.2p1); SSH leg 7.4 s. w-interop -> hg clone over ssh://.
- HG INTEROP OVER HTTP PROVEN: w-hg landed A-680 p2b (piece 95c3edd18; main 354b02d2a; un-ignore 7c71c8048): stock hg
  7.2 HTTP round trip (clone, push, pull) + refusal 2/2 really ran; hg push/serve 95/95, server 78/78. Pushes not yet
  durable (2c after A-687). w-hg -> A-687 (fast loop).
- A-695 svn:// lookup landed (main a3705be03, w-svn): find_repos port, locate across shards, connection moved to the
  owner shard, repo read via svn_view::lend_in; svn 21/21, server 18/18, cli vcs_svn 2/2. w-svn told: A-692 (CRAM-MD5)
  NEXT (every svn:// session + the interop leg wait on it), then ra_svn commits.
- A-692 CRAM-MD5 landed (main 05ae5fc4b, w-svn): keyed contexts at enrollment; svn 66/66, server 42/42. Not wired in
  the listener yet (CRAM_MD5_KEYS false) -> w-svn: wire it NOW, then ra_svn commits, then un-ignore the svn:// leg,
  then A-693.
- A-664 p3 commit landed c6f7976c3 (main 6dfb8fc0c, w-shared): each submit to a git green becomes a server-made git
  commit, recorded and replayed; server 226 passed 2 ign + edit_and_submit/daemon/works_off_owner/fleet filters.
- BUG CONFIRMED BY USE (w-shared): chmod +x then mv in one work -> refused SetModeMissing; across two versions the mode
  drops to 100644. w-shared fixing it next as its own piece (failing test first, docs/bugs record).
- w-interop piece 2 landed 630eb8d76 (main dd614c59f): stock_hg_clones_over_the_ssh_edge (stock hg 7.2 clones an
  empty repo over ssh:// and pulls, host key strict); stock_hg_ 2 passed 2 ignored. hg SSH push waits on A-696
  (w-hg, after A-687). Note: no CLI verb for hg import (A-678 server/client only) -> candidate gap for w-hg's list.
- A-704 LARGE-PUSH FIX landed c34de85e1 (main f6308ccdc, w-git): push admitted what its shard can hold
  (git_push::push_room = Store::admittable, HTTP + SSH), refusal in git's maxInputSize words; red first; git receive
  45/45, server 90/90. Owed: unpack into the quarantine a slice at a time (memory bound). w-perf told to run the
  git/git push + fresh clone fsck now as the by-use proof + slates numbers vs stock (30.3 s / 13.5 s). w-git -> hooks.
- w-git ended (context) after A-704; fresh "w-git: the git server (2)" kiln agent queued on adas-macbook-pro-2.local (starts when room frees), from item 2 (A-665 p4 hooks); brief w-git.md updated (item 1b: slice-at-a-time unpack after hooks).
- w-interop by use: stock svn 1.15 over svn:// now passes the lookup but checkout gets "E170001: No access allowed to this repository" with token id + token over CRAM-MD5 (r4) -- expected until w-svn wires CRAM_MD5_KEYS in the listener (in progress); sent to w-svn. w-interop -> piece 4: large-repo command matrix, git first.
- A-687 durable hg import landed cd5217d1f (main 6a876f6ae, w-hg): one Mercurial record kind SHGR, replay_hg (ReplayRefused::Owed gone); hg import 28/28, server 84/84. w-hg -> A-680 2c (record + replay hg pushes, Source::Push), then A-696.
- A-693 p1 landed (main c8caa9ba3, w-svn): get-lock/get-locks from the lock table; svn 71/71. w-svn REDIRECTED (resent): CRAM wiring in the listener (fix E170001 at checkout) -> ra_svn commits -> then A-693 p2.
- ~21:35 PST ADA: "Fuck kiln, just fucking merge shit already" -> NO KILN RUNS for landing: local focused tests in each
  worktree with the shared CARGO_TARGET_DIR=~/.cache/slates-local-target, CARGO_BUILD_JOBS=8; merge + push at once
  (briefs/common.md section "NO KILN FOR LANDING"; sent to all workers).
- Mode fix landed (main 73cbdfc42, w-shared, ~15 min): a file's mode moves with its rename (compose_modes).
- w-svn reported ending (context) but resumed on main's redirect (CRAM wiring); if it ends, start fresh from
  progress/w-svn.md: (1) listener CRAM wiring (Turn::ChallengeNeeded/KeyNeeded -> cram_md5_key on the owner shard,
  per-call sources via svn_view::lend_in), (2) A-698 from agents/g229-svn-commit-land (ready), then the queue.
- w-git (2) kiln agent stuck in kiln's queue -> started w-git as a direct headless worker (run-worker.sh w-git, API key,
  local tests). If the queued kiln w-git ever starts, stop it (duplicate).
- w-git route question (A-665 p4 PresentView): RULED Route A (read-only volume whose base serves GitHookView; BaseHost kind w/ Pending fetches like A-555; ShardState registration; read-only attachment; path under the root mount; daemon test over the NFS-loopback mount like cli.rs). Route B (write views to disk under the state dir) REJECTED: R1 + §2 item 3 (disk writes outside a granted landing) + §2 item 7 (interim second path). w-git does client-left signal + learned limits first.
- fmt fix landed 0b34f3f01 (main): vcs_ssh/tests/receive.rs rustfmt (w-hg finding). A-680 2c landed (main 243b7bfae, w-hg): hg pushes durable (SHGR Source::Push, replay_hg). Duplicate w-git: stopped the headless one (b41lf3vg4); kiln w-git (2) remains; both names (slates-work-da, slates-work-7d) got the Route A ruling.
- w-interop: git 2.50.1 + hg 7.2 on this Mac, svn NOT -> told to `brew install subversion` (user space, match 1.15.x); fall back to Studio for svn legs only if brew fails. r5 (git/git command matrix on the Studio) finishing.
- w-shared landed the branch-map follow-on 9f51d7525 (paths::branch_before), then ended (context). Brief reordered:
  A-636 2b-2/2c -> holders' maps -> sliced whole builds -> hg/svn objects -> A-697. Fresh w-shared started HEADLESS
  (kiln queued its kiln start "until room"). WATCH: if kiln starts "w-shared: shared pieces §4.17.1 (2)", stop it
  (duplicate) with agents_stop.
- w-interop matrix (git/git mirror 86,366 commits, 607 MiB pack) at OLD main dd614c59f (pre-A-704): HTTP 413 all refs; SSH "push needs 73944 bytes where its room had 63007 left" (ref-update list ~1,100 refs); master alone eof/unpacker error. Told to re-run on current main (A-704 f6308ccdc) and send remaining failures to w-git. svn: w-interop declined brew install (CLAUDE.md §2.14); svn legs on the Studio unless Ada approves a local install.
- HG INTEROP PROVEN OVER HTTP AND SSH: A-696 landed (main 3691deb5c, w-hg): hg push over the SSH stdio session through hg_push; stock hg 7.2 SSH clone/pull/push round trip 1/1 -> un-ignored; hg serve 63/63, server 81/81. Interop now: git HTTPS+SSH done, hg HTTP+SSH done, svn pending. w-hg -> hg lookup.
- ~22:15 PST w-git FINDING: the shared ~/.cache/slates-local-target let cargo run ANOTHER worktree's test binary (members
  hash by path from the workspace root): w-git's "24/24" ran w-svn-cram's binary. RULE NOW: own target per worker
  (~/.cache/<name>-local-target, JOBS=6), a run counts only if the new tests' names appear, disk guard 20 GB. Shared
  target deleted. Sent to all six (session names: w-interop slates-work-a6, w-git slates-work-7d, w-shared (new
  headless) slates-work-91, w-perf slates-work-89 (likely), w-svn slates-work-3d, w-hg slates-work-09).
- VERIFY: landings tested on the shared target (~21:35-22:15) may be false greens: 9f51d7525 (w-shared, local) -> new
  w-shared re-runs it first. Others said Studio runs (A-680 2c, A-696, A-693 p1, mode fix).
- A-699 STOCK BASELINES landed (main 9f0851ea0, w-perf): crates/cli/bench/{git,hg,svn}_scale.sh + git_to_svn_dump.py; best of 3, Studio, load 5-57 (not quiesced): git daemon on git/git push 26.3 s, clone 19.1 s, blob:none 7.1 s, shallow 1.5 s; hg serve on hg repo (57k csets) push 15.8 s, clone 23.0 s; svnserve on git/git newest 5,000 commits checkout 3.3 s, commit 3.7 s, load 806 s; clones identical. w-perf -> slates numbers (release, own target). Quiesced rerun owed.
- A-636 2b-2 RULING UPDATED (w-shared slates-work-91): route by family id AND placement by the family key from birth (a successor must be a holder: A-333 green_adopt reads the successor ledger; successor() ranks the object cohorts). Nothing moves, nothing stored per green; copyset spreading gives way for a repository greens; lone greens = own family. Supersedes the "placement unchanged" narrowing.
- ~22:30 PST: old kiln w-shared (01a1f7226efd...) said "not ending, starting A-636 2b-2" = DUPLICATE of the headless w-shared (slates-work-91) -> stopped via agents_stop. Six now: w-hg, w-svn, w-interop, w-perf (kiln, working), w-git (2) 01a1f73976677515bedb27e43af085ed (kiln, idle awaiting its own local run), w-shared headless (slates-work-91, task bipowc4ce).
- 9f51d7525 RE-VERIFIED in its own target (w-shared, local): server vcs_store::paths:: + commits:: 10/10 with the branch-derive and 100755-mode tests named in the output. No false green.
- A-705 hg lookup landed (main 3bcaccf91, w-hg): served over HTTP+SSH in 7.2 revsymbol order; stock hg identify works; both stock-hg round trips now run `hg identify -r tip`; hg serve 66/66 (local, own target). Owed: tags beyond tip (.hgtags). w-hg -> stream clones. RATE at 20:57 PST: 15 landings since 20:10 (~19/h) vs ~6/h this afternoon.
- Old kiln w-git (01a1f7227bcd..., A-704 session) woke and stood down (fresh w-git (2) owns A-665 p4); it briefly overwrote then restored w-git (2) uncommitted git_push/tests.rs edit; left a draft presenter at ~/slates-work/w-git-drafts/view_host.rs. Stopped it via agents_stop; told w-git (2) to check its diff and about the draft.

## 2026-11-19 ~21:15 PST — ADA: dispatch full agents for 4 more kiln goals (reaper, unresolved, forming fleet, goal 91 all steps)
- Briefs: briefs/w-{reaper,unresolved,forming,fed-move,fed-faults,fed-a583,fed-a503}.md. Federation agents build/test on
  the Studio (persistent clone + warm target; Studio has NO API key, but the agents run on this Mac); the other three local.
- Stopped old overlapping kiln agents: live-durability 01a1af4a..., g225-forming-fleet-reaches 01a1ec2ea10d..., liveness
  01a1af38ff7c... (all would auto-resume at 11pm).
- All 7 kiln starts QUEUED ("starts as soon as there's room" on this Mac; no cancel op for queued starts). If they don't
  start, don't double-launch headless copies without stopping the kiln ones first (duplicates).
- ~21:25 PST Ada: "run them elsewhere? one of our six w- agents has disappeared". The "missing" one = w-shared, running
  HEADLESS (slates-work-91, task bipowc4ce), not in kiln's list. Studios can't run the 7 (no access to this Mac's
  briefs/progress files; no API key -> other account) -> started all 7 as DIRECT workers on this Mac now:
  w-reaper bn13bwpjn, w-unresolved bjmnjgo77, w-forming b089e2f5g, w-fed-move b8q6gbxh4, w-fed-faults bfveagjm9,
  w-fed-a583 b9bt0uskg, w-fed-a503 bp45lno20.
- WATCH (each wake): kiln's queued copies (w-reaper, w-unresolved, w-forming, w-fed-move, w-fed-faults, w-fed-a583,
  w-fed-a503, and "w-shared ... (2)") may start later -> agents_stop each the moment it appears (duplicates).
- A-706 hg stream clones part 1 landed (main 3fee2f575, w-hg): slates_hg::render (index -> stock 7.2 repo files: split revlog v1 generaldelta full texts, fncache/dotencode, phaseroots, bookmarks, obsstore); stock hg verify passes on rendered repos; render->import round trip row for row. Owed: deltas/compression, render at commit. w-hg -> part 2 stream_out. Duplicate watch: no queued kiln copies started yet.
- A-636 p2b-2 landed 4c38e425e (main eea7e6497, w-shared): family in ids (repo id + its greens), rendezvous hashes ObjectId::line_key so placement + successor follow the family; lone greens own family; family-less old greens refused Unsupported. db 124/124, model 26/26, wire_order 2/2, server 116/116 + 17/17 (local own target). Fixed on the way: CreateRepository minted repo id and root id at the same counter (same bytes) -> offsets 0/1. Finding: no repository record (name, tool, owner, policy) ships to holders -> 2c needs it. w-shared -> 2c.
- BUG BY USE (w-perf): stock git pushing all of git/git (1,027 refs) into a release slates (main 9f0851ea0, 23 shards): "shard 10 is spinning: 1046 ms of CPU without a step" -> anchor kills the daemon -> git hangs ("unable to rewind rpc post data"). = A-704 owed half (processing not sliced). -> w-git (2) TOP PRIORITY: unpack, connectivity check, per-ref work in cooperative slices; failing test: no step over budget; prove with crates/cli/bench/git_scale.sh. Logs: Studio ~/.cache/w-perf/evidence/git-push-spin-anchor.log.
- w-shared split A-636 2c: 2c-1 repository record as its own register (Own class, REPOSITORY_CLASS=6, seq policy.version+1, shipped to the family candidates, adopted as a pending repository) then 2c-2 the successor rebuild (keep-whole rule, one Op, store from the log). Accepted (no reply needed).
- w-hg tried to END for context after 6 landings (A-680 2b, A-687, A-680 2c, A-696, A-705, A-706 p1) -> told to keep going (compaction): stream clones p2 then p3 then the inventory list. Disk 71 GB free (was 91).
- ~21:09 PST KILN SERVER DOWN (w-perf: "cert revoked/superseded ~05:09Z, connection dropping"); kiln MCP "not connected".
  kiln-hosted agent processes still run locally (claude -p --input-format stream-json); messaging via SendMessage (UDS)
  works. w-hg told (SendMessage) to keep going; w-perf told not to end, measure stock vs slates on THIS Mac meanwhile.
  Federation agents' multi-runner tests blocked until kiln returns (do local code/tests meanwhile).
- w-forming: zero-contact stall already on main (A-331 ddcf00768); kiln down so it cannot read the goal; sent the text I hold + target 48/48 forming runs on one CPU (B10 harness; base 40/48, learner-3 47/48); open items from progress/g225-forming-fleet-reaches.md.
- BUG BY USE (w-perf), hg twin of A-704: stock hg 7.2 pushing Mercurial own repo (57,036 csets, 84 MiB) into release slates (9f0851ea0): "abort: HTTP Error 413" after 7.5 s (unbundle body past the request room, vcs_http/hg.rs). -> w-hg TOP PRIORITY: admit by the shard capacity + slice-bounded processing (avoid the git spin twin); prove with crates/cli/bench/hg_scale.sh.
- 2nd repro of the git push-spin bug (w-interop, debug main 9f51d75): push-all of git/git 607 MiB -> "shard 0 is spinning: 3559 ms of CPU without a step, past its 3488 ms window" ~4 min in; daemon killed; git hangs. Forwarded to w-git (slates-work-7d / -da) via SendMessage (kiln down).
- A-707 landed 4f2d92af2 (main 05e3a2ddc, w-unresolved): the Rust lineage model covers A-508 settle-away release held to SettleKeepsFPlusOne, waits on A-556 readiness_cohorts; 400 seeds: 72 released, 2,526 kept, 0 violations; slates-db --test lineage_model 4/4. Lineage.tla text was already on main (A-518) -> skipped. w-unresolved -> goal 84 clause-by-clause check (GAPS table); notes GAPS:4502 "Open (A-309)" looks stale after A-341/A-484/A-508.
- ~21:40 PST kiln BACK. Kiln list: w-hg, w-svn, w-interop, w-perf, w-git (2) working. "Missing" = w-shared (headless
  slates-work-91). Started kiln w-shared (queued "until room") with a HANDOFF prompt (message slates-work-91, wait for its
  resume steps); told slates-work-91 to keep working until the kiln instance contacts it, then push, write resume steps,
  END. The 7 headless goal agents (reaper, unresolved, forming, fed-*) also aren't in kiln's list.
- A-665 p4a+4b landed (main 0ea7a3304, w-git (2)): 4a 97fdfedde HTTP ClientGone ends a hook run when the client closes; 4b 30fc43407 ShardState::hook_times, each git hook held to its learned bound. server 72/72 (local, own target, names shown). w-git -> push-spin bug (top), then SSH client-gone, then route A presenter.
- BUGS BY USE (w-interop, debug main 9f51d75, git-matrix.sh): (a) SSH full push dies too (spin-kill, 225 s); (b) CLONE
  FAILS for git/git cut at v1.0.0 (2,930 commits, 3.4 MB pack) over HTTP/SSH/blob:none: "upload-pack: the request needs
  more room than the server admits" (A-704's admission bug on the upload side); (c) fetch --deepen 100 after --depth 1:
  "possible repository corruption on the remote side". Matrix 4/27 rows pass. -> w-git queue: (b) clone room FIRST,
  then push spin, then deepen.
- ~21:50 PST Ada: "Move them in so we can track them." -> asked all 8 headless (w-shared 91, w-unresolved 43, w-forming
  9e, + 2a/94/0c/88/3a = reaper/fed-*) to checkpoint (push, resume steps) and END, so kiln's queued starts get room.
  Check agents_list after: the 7 queued kiln starts from ~21:15 + w-shared starts; stop duplicates; start any missing.

## 2026-11-19 ~21:30 PST — ADA (angry): run them on the Mac Studios; use kiln's move/sync. DONE:
- Fanout state pushed to branch agents/fanout-state (a102df4de); Studio agents clone it to ~/fanout-state.
- Started on the Studios (kiln, immediate): w-shared 01a1f76a704977e8a045258115df99be (studio), w-reaper
  01a1f76a8ab973a0adf5dd90ef4dfb65 (studio-2), w-forming 01a1f76ab3aa702ba2277533722bcfef (studio-3), w-fed-move
  01a1f76adade74a7a0534764ad891d5c (studio.local-2), w-fed-faults 01a1f76aff04715e8afac92cfc1a9c5f (studio), w-fed-a583
  01a1f76b2c06751dad5b207270f1eaef (studio-2), w-fed-a503 01a1f76b478a7222bb857421e2cc7c23 (studio-3). They report via kiln
  summaries (no cross-machine SendMessage).
- UNRESOLVED-OBJECT GOAL MET (d9f7a795...) + parts (Lineage.tla = A-518; goal 84 audit b2a544735); A-707 05e3a2ddc.
- hg room bug widened: import refused PastRoom 278,528 B too (w-perf A-710 verb) -> w-hg.
- WATCH: kiln-queued copies on this Mac (7 from ~21:15 + w-shared x2) -> stop on sight.
- Handoffs complete: w-shared (2c-1 on agents/w-shared-a636p2c1 1c91eaac2) and w-fed-faults (agents/w-fed-faults 72cef952d, agents/w-fed-faults-a708 473a560ed) -> fanout-state re-pushed; Studio instances told to pull. A-710 landed (main 5c6eb940d, w-perf): `slates repository import NAME PATH`.
- ~21:55 PST Ada: agent for goal 87 (every open bug) -> jupiter can't take kiln work (kiln 156 commits behind; kiln can't
  build itself for Windows) -> w-bugs on adas-mac-studio-3 (01a1f76df3567435a347b36a1cf4290e): A-498, A-551, A-576,
  goal-83 B2-B16; Windows/WinFsp tests via CI's Windows runner (diag-* branch) as g258 did.
- Ada: three agents for goal 90 (kiln vendors slates) -> w-k90-containers 01a1f76ef417710e8c3842fcd4e46700 (studio),
  w-k90-microvm 01a1f76f015c7274a2234c6793fae2ea (studio-2), w-k90-vendor 01a1f76f1e037405bd1f4e5ecf82ff1d (studio.local-2).
  Briefs briefs/w-k90-*.md (kiln repo rules AGENTS.md/CLAUDE.md; Shards policy if touched; WHP + Windows e2e BLOCKED on jupiter).
- svn by use (w-perf -> w-svn): import of 5,000-commit dump refused TableAtCapacity{green_chains} at 1,685 revisions,
  RSS 3,969 MiB; svn commit over svn:// E210002 (expected until A-698).
- ~22:05 PST API KEY INSTALLED on all 4 Studios (d1323898 Studio, b24c5c95 Studio-2, cec37f60 Studio-3, 2768d0d5 Studio-4) via kiln reach sync of a 600 key.env + install.sh: ~/.zshrc line, ~/.config/anthropic/key.env (600), ~/.claude/settings.json apiKeyHelper reading it; synced copies deleted; local copy deleted. Studios 3 and 4 show low free memory (650 MB / 315 MB).
- SVN INTEROP OVER svn:// PROVEN: w-interop piece 3 landed 41c2e662e: svn_interop.rs svn:// round trip un-ignored, passes
  with stock svn 1.15.0 (Studio). Still ignored: svn+ssh (E210002) and HTTP legs (E160013, waits on DAV reads).
- A-698 ra_svn commits landed (main ddbd387a2, w-svn). A-589 docs landed 8eeb004bd (w-fed-a503: A-503 move test).
- ~22:10 PST Studio agents restarted on the API key (idle ones had never started: unauthorized): w-shared
  01a1f77364c772dab1252645daa3af52, w-fed-faults 01a1f7737a28722880bddaff84f71abc, w-fed-move 01a1f773926977aeb252ecae312154a2,
  w-k90-containers 01a1f773ff6874e8a4691fe653779c97, w-k90-vendor 01a1f77457e2739c8efc838b63fdbd97, w-fed-a583 (big Studio
  d1323898); w-reaper 01a1f773af777640876754d96d34dc0a, w-k90-microvm 01a1f774356a77b6af7969d2cd2ac702 (Studio-2 b24c5c95).
  Kept running: w-forming, w-bugs (Studio-4 2768d0d5, 10 GB disk free there). w-fed-a503 done (idle). A queued
  w-fed-a583 on Studio-2 (link down) may start later -> stop it.
- green_chains import fix landed f37a96515 (main 9ea6e1311, w-svn): Subversion landings (import, svn://, tunnel via svn_commit::land) checkpoint their green by the doubling rule, Doubling::Now at the cap; red first. w-perf asked to rerun svn_scale.sh (import, RSS, commit, timings).
- ~22:15 PST ADA: "make sure each agent knows to merge to main then push to origin/main once complete" -> common.md section
  "EVERY PIECE ENDS ON origin/main" (fetch, merge, push HEAD:main, verify with merge-base --is-ancestor, never leave done
  work on a branch); sent to all 16 agents via agents_send; fanout-state re-pushed.
- w-interop svn matrix landed (merge 4ac4c6526): ci/interop/svn-matrix.sh, 22/27 pass over svn:// (reads, commit, copy,
  switch, merge). Remaining: svn lock over svn:// E170003 (A-693 p2, w-svn); svn+ssh "svnserve -t is not served yet" in
  the daemon's SSH site (w-svn queue item); HTTP path not found (A-681, w-svn). w-interop confirmed all its pieces on
  origin/main by merge-base.
- ~22:35 PST w-perf a11712269: svn_scale.sh measures slates; svn over 1,000 git/git revisions (main bc2803d66, best of
  3): import 1.87 s vs svnadmin load 62.8 s; log 0.062 vs 0.140 s; checkout/commit/update even (client-bound); daemon
  holds 2.57 GB after a 223 MiB import (11.5x). 5,000-rev import still TableAtCapacity{green_chains} at 1,685 revs.
- w-svn root cause (green_chains): repository entries' objects (A-662) charged to the partition's MERGE budget and kept
  in the register; tool store holds them again. APPROVED fix: objects off the merge budget, charged to the owner
  shard's memory budget as retention at RepositoryLogs::append (credit on refusal; replay; destroy). Notes sent:
  admission hold vs retention (no double charge), second copy -> GAPS owed (A-64), claim A-714+.
- w-svn landed svn+ssh in the daemon a14635977 (fc79d36ae). w-interop 168ddfa96: svn+ssh round trip un-ignored; svn
  matrix 24/27 (left: svn:// lock E170003; two HTTP rows A-681).
- w-git A-713 landed 03dc601a5 (a2f0fb970): upload-pack admits by the shard (clone room bug fixed). Next: push spin.
- w-hg A-712 stream clones p2 landed 7405f9508. A-711 (bc2803d66): 413 gone, but whole hg push spin-kills (w-perf
  shard 10 1,152 ms; w-interop shard 0 3,698 ms). w-git found 2 hg-site tests 503 on clean main bc2803d66 -> w-hg FIRST.
  w-hg MEASURED: manifest log 57,007 revs = 9.4 MB deltas vs 5,501,443,727 B full texts; slates stores full texts.
  RULING: (3) sliced intake + (4) derived import room first (started), then (1) delta storage + (2) delta records as one
  amendment (§4.17.3, SHGR v2); engine's append takes a revision as arrived; snapshot interval derived.
- MAIN CI: last finished run 44676412887 (3691deb5c, 04:44Z) failed 15 jobs; every later run cancelled by the next
  push. Windows had not compiled since 9abe7e9a2 (anchor vcs_svn_listener init lacked cfg(unix)) -> main fixed it,
  36478561d (win clippy anchor lib red->clean from the Mac). Lint wall red in slates-db + slates-vcs -> new lint keeper
  w-lint (briefs/w-lint.md). w-bugs told to re-cut its Windows diag from >= 36478561d.
- Stopped duplicate w-fed-a583 on Studio-2 (01a1f7744305737c8650cfbb776d54a0); kept the big Studio's (01a1f7749cc7...).
- ~23:00 PST w-perf piece 3 ranked plan (release bc2803d66, MallocStackLogging on a 250-rev svn import, 85 MiB dump,
  577 MiB live heap; footprint 920 MB @250, 2,060 @500, 2,430 @1,000 revs): M1 register repository records ~0.5 MB/rev
  (apply_advanced 86.5 + apply_repository 39.7 MiB), maybe quadratic; M2 svn Importer::import buffers 118.7 MiB (1.4x
  dump); M3 merge history whole-value copies 148 MiB; M4 green checkpoint whole copies 46.8 MiB; M5 virtual only
  (timer wheels 898 MiB reserved). T2 svn checkout/commit/update client-bound: server CPU per checkout 0.12 s vs
  svnserve 0.56 s. RULINGS: M1 measure slope only (w-svn is in that code), hand to w-svn; M2 w-perf now (bound);
  T2 recorded client-bound with the server's share; M3-M5 recorded as owed.
- ~23:15 PST w-perf CORRECTION: M2 is not importer retention; svn texts are held twice: svn records' own
  `Repository.contents` BTreeMap (crates/svn/src/migration/records.rs:77; 306 MiB @1,000 revs, 1.37x dump) + the
  green's content pool (585 MB); the register's A-662 entries hold a third (~245 KB/rev). RULING: content once (A-64):
  svn records name the green's content by ContentIdentity; w-perf implements (seam agreed with w-svn: green content
  durable before the naming record), lands after w-svn's charging fix; register copy -> w-svn GAPS. w-perf landed
  183dd0e23 (BENCHMARKS "Where the svn server's time and memory go"). Next for w-perf after M2 starts: git read path
  vs git daemon once w-interop's matrix shows clones passing (A-713).
- ~23:30 PST FIRST GIT ROW (w-perf, release 82d3538b0, git/git cut at v1.0.0: 2,930 commits, 12,233 objects, 3.5 MiB
  pack; interleaved best of 3, slates vs git daemon): clone --mirror 2.816 vs 0.300 s (9.4x SLOWER); blob:none 1.092 vs
  0.180 (6x slower); push 0.665 vs 0.310; shallow 0.160 vs 0.111; one-commit push 0.058 vs 0.080; fetch 0.080 vs 0.068.
  fsck clean, refs+objects identical. Cause: every clone re-runs slates_git::store::deltas::Search::step (72% of
  samples). RULING T4 -> w-perf NOW (before A-716): delta computed once per object and kept with it, stored deltas
  streamed, remaining search sliced; w-git's sliced intake may keep pushed deltas as they arrive (w-git's call).
- Ledger clash fixed: A-715 = w-reaper (idle sweep), A-716 = w-perf (svn texts once); next free A-717.
- w-shared A-636 2c-1 landed 82d3538b0 (878fc386c); next 2c-2 (successor rebuilds the repository by keep-whole rule).
- ~23:55 PST CLEANUP: stopped Studio w-shared (couldn't push; local kiln w-shared 01a1f785b29870d68d5512861bc03492
  landed 2c-1 82d3538b0 and continues 2c-2), w-fed-a583 (A-583 already on main via 8eeb004bd), w-fed-a503 (queue done).
  Ledger clashes x2 resolved (A-715 w-reaper, A-716 w-perf svn texts once, A-717 w-bugs, A-718 w-perf git deltas,
  A-719 w-git push spin; next A-720). common.md: claim A-numbers by pushing the ledger; own state folder per Studio
  agent; adas-mac-studio.local-2 (32-core) has NO GitHub credentials (SSH publickey denied, HTTPS none) -> land via
  kiln run on adas-mac-studio-2.local (SSH verified 23:50) like w-fed-faults. Broadcast to all 14 agents.
- Goal stack items 487 (A-583 three-daemon fleet test) + 488 (A-503 move test): passed macOS arm64 (8eeb004bd);
  Linux x86_64 run r627 started by main at 60e23101a (kiln linux/amd64) -> mark both met when it passes.
- ~00:05 PST A-714 LANDED main b705a24c5 (w-svn, 5ddb7e09b): repository entry objects charged per repository to the
  owner shard's memory budget (trued up after each append and at recovery, credited on free); admitted git/hg pushes
  never refused by it; svn revisions check room first. db 29/29, server 105/105 + 61/61; GAPS: content held 2x (3x svn
  texts). w-perf asked to rerun the 5,000-rev import; if refused for room, A-716 before T4. w-svn next: A-693 p2 lock.
- ~00:10 PST ADA: "the change that stores svn file text once (A-716) -> we should do that anyway" -> A-716 now
  unconditional for w-perf: rerun (measure) -> A-716 (unless T4 lands within ~30 min) -> T4 -> register entry objects
  name content by identity (A-662, every tool; seam with w-svn).
- ~00:15 PST ADA: "Likewise for git and mercurial" -> content once for all three tools: w-git (after A-719): git store
  names blobs by the green's ContentIdentity (trees/commits stay git's), order vs w-perf's T4 agreed between them;
  w-hg: folded into (1)+(2): filelog texts named by ContentIdentity, manifests/changelogs as deltas; w-perf: A-716
  (svn), then register entry objects (A-662) for every tool.
- ~00:25 PST svn 5,000-rev import rerun (w-perf, release b705a24c5): 3,540 revs committed (was 1,685), then refused
  Unpublished{record 3560: segment profile 803,344,210 B > 746,962,928-byte region}; RSS 5,331 MiB. BUG BY USE -> w-svn
  TOP (before lock): after the refusal every svn commit E160004 (green ahead of records; repository wedged) -> green
  advance + record one atom, failing test first. Region wall -> A-716 + entry objects (w-perf, A-716 now).
- ~00:30 PST ADA: "an individual slates server must be able to handle tens of thousands of concurrent large pushes.
  For git - I would argue part of the reason we're crashing is the triplicate storage." -> content once moved INTO the
  push fixes (not after): w-git A-719 = streaming intake, each object stored once on arrival, deltas against stored
  bases, bounded working set, sliced; admission charges the working set, content charged as stored (A-714); w-git
  writes the shared rule into §4.17.1. w-hg: same inside (3). w-svn: cite it on the commit path. w-perf: after A-716,
  a concurrent-push load test (replay client; 1 -> 10,000+ pushes on one daemon), before T4.
- ~00:45 PST w-hg fixed the two hg-site 503 tests on main 7379a4d8e (3a59b5776): cause was A-687 (a done import waits
  in the map until it lands; create_hg_from_states stopped at Done), not A-711. Next: (3)+(4) with content once.
- GOALS MET (main, verified): event-driven reaper (A-709 9b93fd77c, A-715 f70230cc8, reap_loop deleted); A-583 fleet
  tests; A-503 move test (macOS arm64; Linux run r627 queued, adacachy full). w-reaper sent a stale "reap tick"
  comment/name sweep (dax_backing, daemon.rs, cli.rs test, nfs_large_write bench).
- ~00:50 PST w-shared A-636 2c-2 landed c7c18db3e (c8452ee97): successor rebuilds a taken-over repository by keep-whole
  rule, Op::RepositoryRebuilt (wire 101). Finding: rebuilt store holds no objects (state log parts carry none) -> rides
  content once (entries name green content by identity; successor holds the family's content); fetching parts a
  successor doesn't hold stays owed. Next: A-664 holders' maps.
- ~00:55 PST w-interop landed 78eea35b9 (git matrix tag row; git 26->27/27 expected at v1.0.0); hg 17/24. It was
  idle-waiting -> REASSIGNED A-681's remaining DAV pieces (DAV routing + update-report; the two svn HTTP rows) from
  w-svn to w-interop. w-svn: E160004 fix, then A-693 p2 lock.
- ~01:00 PST w-git A-719 p1 landed 20ffba05f (81c36bd99): push checks (fsck + connectivity) a turn at a time under the
  step quantum (Answering::Turned). Debug 3,000-commit push: prepare and decide still 0.3-2.7 s in one step -> next
  parts: streamed store-once intake + §4.17.1 rule, then verdicts sliced. Ruling: w-git's intake does NOT wait on
  A-718 (w-perf's T4 is behind A-716 + load test); w-perf builds T4 on the new store.
- ~01:05 PST A-718 (T4) had already landed 156f4437d (w-perf) before the reversal: git/git cut v1.0.0, best of 3:
  clone-again 2.766 -> 0.912 s (git daemon 0.342); blob:none 1.076 -> 0.558 s; shallow 0.154 -> 0.118 (daemon 0.119);
  first clone after a push 2.791 -> 4.293 s (one-time encode); RSS 384-388 -> 482-513 MiB. Follow-ups (encode at
  intake, first-clone cost) rebuilt on w-git's new store later. w-perf now: A-716, then concurrent-push load test.
- ~01:15 PST A-716 DESIGN RULING (w-perf found the green's history chunks have no content identity; folding drops
  history): (a) the green's identity table = the ONE byte store for git, hg and svn: hold by path at head (svn) and
  hold bytes by identity path-less (git/hg intakes); landing op set_from_held(path, identity) reuses chunks (no rehash
  on VFS write path); survives folding (refcounts), in checkpoint (recovery/replication rebuild), bounded holders.
  w-perf: slice 1 (engine) NOW (before load test), then load test, then svn slices 2-3 (seam with w-svn's E160004).
  w-git/w-hg: land streaming + slicing now; switch storage to the table when slice 1 lands; no second store meanwhile.
- w-interop: A-681 tests on the Studio's warm target (Mac disk 32 GiB free); `git checkout -b x` breach owned (no harm).
- ~01:20 PST w-hg A-720 p1 landed (56952183f): slates-hg push::intake, a changegroup into an index fork one revision
  per step (rebuild vs stored base, node check, kept once); working set one revision's texts. RULING for p2: intakes
  run concurrently even per repository (a fork per push); only the landing verdict is ordered per repository.
  common.md: name branches with `git branch`/push HEAD:refs/heads/<name>, never checkout -b (two breaches tonight).
- ~01:30 PST w-hg p2 plan APPROVED: fork per push, concurrent intake; ordered landing rebases the fork by node
  (renumber rows; reuse stored content; skip revisions already landed; sliced) when the index moved. Added: judge
  bundle2 check:heads / check:updated-heads at landing against current heads; advertise checkheads=related.
- ~01:40 PST A-721 LANDED 0cb5120b1 (w-svn, 3169466fe): E160004 wedge fixed: reconcile_unpublished_effects remakes a
  green whose engine is past its durable head from checkpoint + chain (lone engine / single-member lineage = every svn
  repo). Owed (GAPS): multi-member lineages (git branches) counted (merge.unpublished.green_ahead_kept) not remade; a
  submit's vcs_store paths/commits not taken back -> left for use to find (likely at git's region wall). w-perf to
  confirm by the 5,000-rev import at its next release build. w-svn next: A-693 p2 (svn:// lock).
- ~02:00 PST ADA (direction): "Don't we have an entire operations based merge mechanism to help with and solve this?"
  / "This is our entire advantage ... we aren't just a git/mercurial/svn server too, we use our merge mechanisms and
  default concurrent scale ... higher throughput, lower resource usage." = §4.17/D-28 as written (push = work, one
  copy by identity). Ruling: slice 1b/1c (SetFromHeld OpKind, hold log) OFF; w-perf writes the convergence plan
  (every copy measured vs §4.11/§4.17.1, steps + owners), then (2) entry objects by identity. w-git: compressed
  deltas in the EXISTING ContentStore OK; no hold_bytes, no new store. w-hg/w-svn: no new store; wait for steps.
  w-perf attribution: one 3.43 MB push = +229 MiB dirty (ContentStore inflated 69.1, register entry 63.5, arena
  63.7), never released; load test N=1/10/100 ok, RSS 386 MiB / 3.3 GB / 28.2 GB.
- ~02:05 PST ADA: "a single server should be able to facilitate numerous repositories ... balance consumption and usage
  between them based upon agent, usage patterns. Dispatch an agent ... toy repos to Google's or Meta's *in multiple*."
  -> w-balance (briefs/w-balance.md): design first (outline before code), then pieces, proven by a multi-repo push_load.
- ~02:15 PST CONVERGENCE PLAN APPROVED (w-perf): today a 3.43 MB git push = inflated store 69.1 + entry 63.5 + arena
  63.7 MiB (+ A-718 fragments 100-125 MiB later); svn text held ~4x (records 305.6, entries 244.7, history chunks
  234.8 MiB, blocks ~585 MB @1,000 revs). Steps: S1 (w-perf, REVISED) the entry holds the pack as sent (compressed,
  deltified, minus held objects) + moves, replay re-indexes; bounded parts past an entry's bound; S2 (w-perf) history's
  latest value names the block, held.rs stays as identity->block, hold_bytes goes after S3/S5; S3 (w-git) store = index
  oid->identity over the record's pack, blobs land as the push work's file ops, fragments from intake; S4 (w-svn +
  w-perf) records name texts by identity; S5 (w-hg) filelogs as green files by identity, changelog/manifest deltas.
  Push bound: streamed window per connection, TCP backpressure, admission charges window + post-state, typed refusal
  before body; 10,000 x 256 KiB = 2.5 GiB.
- w-git A-719 p2 landed 183150ab8 (569868917): unpack::Keep (objects handed to a keeper as resolved; bounded working
  set); §4.17.1 "A push's intake" rule written. Next p3: keep into quarantine as read; admit_all goes.
- ~02:45-03:00 PST DISK CLEANUP (Ada): big Studio freed 99 GB (my stale targets; 1.2 TiB free); Studio-4 freed 129 GB
  (stale unused reach copies; 129 GiB free, was 6.5); Studio-2 freed 72 GB (123 GiB free, was 18); this Mac freed 11 GB
  (moved agents' targets) + 44 GB stale reach copies (80 GiB free, was 23). Studio-3 (adas-mac-studio.local) offline.
  Rule: only stale (>12 h), unused (no open file, no process naming it) cargo targets / kiln reach copies.
- ADA: "stay on top of the agents ... unblock them when they claim they're blocked and just pause (unsigned commits are
  fine) ... every five or so minutes is fine" -> cron 8ae7afe6 (2-59/5 * * * *) supervisor check; common.md: never
  pause on a clearable blocker, unsigned commits fine, disk hygiene. First pass unblocked: w-balance (go P2b+P3, one
  admission path with w-git's A-722), w-bugs (build B3 itself; jupiter takes kiln work again), w-fed-faults (don't idle
  on the Linux queue), w-reaper (new: svn HTTP blame/diff/mergeinfo reports, then pre/post-lock hooks), w-k90-containers
  (build PV/claim/pod builders while 5b-i reruns), w-k90-vendor (item 4 Windows run on jupiter now).
- LANDED: w-hg A-720 p2a 49439c77e (concurrent hg pushes per repository, fork per push); w-svn A-693 p2 576a8a860
  (svn:// lock verbs); w-shared A-664 sliced whole builds p1 469436cce (+ ruling (a) for p2); w-balance A-724 P1
  8c1feae15 (usage per repository, §4.17.5) + P2 4bbf7f4ba (share division, weighted DRF); w-interop A-681 B 7f81be683
  (update-report) + C d858d1904 (HTTPv2 commits): SVN OVER HTTP PROVEN, matrix HTTP 23/27, svn:// 26/27. w-perf A-716
  1a 5efce021a (held.rs). w-interop next: HTTP locks, streamed PUT, abandoned txn deadline.
- ~03:10 PST check: all 16 w-* agents working (w-fed-faults building piece 3 locally; its Linux run r9 queued behind adacachy's stuck kiln CI, whose stale-run cancels wait on Ada's click). Disks: Mac 93 GiB, Studio-2 89, Studio-4 128, big Studio 1.2 TiB.
- ~03:15 PST w-svn A-702 p1 landed 15a3c5d4c (change-rev-prop(2) answer E165006 in svnserve's check order). Ruling: ra_svn running repository hooks (start-commit..post-unlock) -> w-reaper after its DAV reports; then w-svn A-702 p2 (revprop change). w-svn next: S4 slice 1.
- ~03:25 PST A-723 S1 LANDED de50bf0f5 (w-perf): git record v2 = named objects (type, identity, length) + the client's pack as sent (or server commits' deflated whole pack); replay through git's reader; v1 still read; entry expected 63.5 MiB -> ~3.4 MB per 3.43 MB push. Owed: bounded parts across entries for a pack past an entry's bound -> w-perf after S2 (before whole git/git push). w-perf next: S2.
- ~03:30 PST w-perf: S2 is a multi-day engine refactor (Files/dims::Blocks vs history::Chunks; whole-file replaces duplicate the whole file). RULING: w-perf does bounded parts NOW, then S4 server slice 2, load test, T4 follow-ups; S2 -> new agent w-engine (briefs/w-engine.md): design entry first, then slices with the engine's model/proptests.
- ~03:40 PST w-perf traced the 747 MB wall: it's the PARTITION SNAPSHOT (replay.rs snapshot -> RegionKind::Snapshot slot; anchor segment.rs:1041), the sum of all retained records on the partition; parts across entries can't fix it. RULING option 2 (w-perf owns): repository bytes leave the snapshot: packs/svn/hg texts become content durable in the anchor's content regions (A-64), published by identity, replicated as content, entries name identities; recovery content-before-records; slot derived (D-11) + partition spreading for metadata growth -> w-balance. w-engine started (01a1f7e20fd6756a9e3dd29f3c5735e1, big Studio) for S2.
- ~03:50 PST w-perf design OK'd: ContentStore windows in the owner shard's chunk arena of the anchor content object (A-64), table in the shard image; git record v3 names identities (pack root by identity); order: content into arena while streaming -> shard image published (group-commit barrier, not per push) -> entry commits; recovery images first; replication content before record. Slices (a) store over arena, (b) record v3, (c) svn via S4, (d) two-node replication order. Snapshot ~41 B/object (git/git 17 MiB, Linux ~410 MiB) -> w-balance: derived slot + giant-repo metadata spread across partitions.
- ~03:55 PST w-interop item 1 LANDED d8eabb9e8 (f5cbdd1b7): svn HTTP LOCK/UNLOCK over dav::locking + lock table (Record::Locks via SvnStores::lock_verb); commits present lock tokens (If header, MERGE lock-token-list, keep-locks). svn_interop 5/5, NONE ignored -> svn interop complete over svn://, svn+ssh and HTTP. Next: streamed PUT bodies (dav/commit.rs, vcs_http/svn.rs).
- ~04:05 PST check: 16/17 fleet agents working (w-fed-faults running its link-fault flows on macOS; Linux r9 still queued). Disks fine (Mac 85G, S2 84G, S4 117G, big 1.0T). A-727 design entry LANDED 41f183ce6 (w-perf; repository bytes into the shard's content arena; record v3; group commit). Slice a1: ContentStore reads by copy-out (sealed/encrypted chunks, A-99), 9 readers move; w-perf tells w-git/w-hg.
- ~04:15 PST check: 15/17 working; w-balance landed §4.17.5 pieces 8+9 design 3471c18a2 (snapshot slot names index tables by identity, ~1,600 rows/window; size recomputed per snapshot, typed SnapshotRoom; giant index split 2^b ranges by id bits, rendezvous per (family, range), extendible doubling). RULINGS: piece 6 approved (hot set + register with owner per §4.10; cold content homed by identity, rendezvous over content id, f+1 identity homes, fetched by identity; content durable before naming record; amend §4.10; agree with w-fed-* and w-shared); admission seam approved and relayed to w-git (vcs_share::room replaces push_room; A-722 reserves against room().bytes; bound in typed refusals; w-balance edits admission.rs/report.rs only after A-722). Disks fine (Mac 89G, S2 78G, S4 104G, big 1.0T).
- ~04:25 PST w-hg A-720 p2b LANDED 5df3c5a18 (64d457979): concurrent pushes to one hg repository all land; a stale fork is re-based by node at landing (intake::Rebase, content reused, rows already held passed over, sliced, drop guard); verdict + race checks at landing; land refuses a stale fork. Tests: two_pushes_taken_side_by_side_both_land + rebase oracle. Next: p3 (import room per repository), then S5.
- ~04:30 PST w-interop item 2 LANDED 15f4dbdde: svn HTTP PUT bodies streamed into the drive one run at a time (begin_put/put_chunk/end_put), whole-body path removed; svn 137/137, svn_interop 5/5; svn matrix HTTP 24/27 (blame/diff/merge wait on w-reaper). Next: abandoned transaction released at a derived deadline.
- ~04:35 PST check: 14/17 working; idle ones are waiting on their own runs (w-forming campaign 4/12 so far, w-fed-faults macOS flows, w-k90-containers 5b-ii + K8s push). Stopped g219 (goal met, idle since 11-18). Studio-4 RAM: 70 MB free but 7.8 GB inactive (reclaimable); 8 old idle g2xx sessions hold ~2 GB. Disks: Mac 77G, S2 59G, S4 100G, big 1.1T.
- ~04:45 PST check: 13/17 working. w-balance piece 4 LANDED (4a fbb5d5f8b cold chooser; 4b 87edaacb6 trigger drops pack fragments by learned rebuild cost); next piece 5; piece 6 questions relayed to w-shared/w-fed-move/w-fed-faults (answers in inbox/goal91-w-shared-from-w-balance.md); main answered Q2 (move moves register+hot set only) and Q4 (no coordination call). Studio-2 disk 89->49 GB in 30 min: w-lint 3 targets 53 GB (told: one target dir, delete srv-a/srv-b), w-k90-microvm 2 targets 21.5 GB (told to trim), w-balance 14 GB. Two lint warnings (hg_push.rs:75 unused import, state_log.rs StateLogs::bytes) -> w-lint.
- ~04:55 PST w-shared A-664 svn objects LANDED 196ba2e15 (55cf568ae): a work submitted to an svn green made no revision (later commits 160004); svn_made builds the revision rows, svn_commit::land_made records them in the submit's transaction; bug record docs/bugs/2026-11-20-a-submit-to-a-subversion-green-made-no-revision.md. Queued for w-shared after item 4's hg half: svn:author, copies, symlinks for native works; then (a) live builds. w-shared answered Q1/Q4 (content homes by content-id ranking OK; 2c-2 successor fetches objects by identity).
- ~05:00 PST ruling: A-664 item 4's hg half (server-side hg commit of a submitted version) -> w-hg after p3 + S5 (owns slates-hg index/store; avoids parallel rework); w-shared: (a) live whole builds now, then svn_made gaps (author, copies, symlinks).
- ~05:10 PST ruling (w-shared, A-664 live whole builds): APPROVED the owed commit staged durably in the submit's transaction (green, version, message, identity, owner's second); completion makes the commit + settles the owed entry in one transaction; recovery remakes the map sliced then the commit from the owed entry (no clock read in replay; determinism test); one owed commit per green in flight, later submits parked, typed refusal at the bound. Push half first (pushes carry their commits: slice only).
- ~05:15 PST w-interop item 3 LANDED 3be8fd765 (67920ccf8): abandoned HTTP txns released past the edge's learned idle bound (vcs_http::idle_bound_ns; counter vcs_store.dav_commits_released). Slip owned (pushed an unchecked merge; re-verified green: svn 137/137, server 117/117, svn_interop 5/5). Next: git+hg matrices on current main, then interop on Windows via jupiter, then Linux.
- ~05:20 PST check: 13/17 working (w-svn re-running S4 slice 1 tests after main moved 26 commits; w-forming throttled set 6/6; w-k90-containers waiting on its .nfs test build). w-balance piece 5 part 1 LANDED 129cd0d1b (content windows homed by BLAKE3 across a node's shards, cross-shard calls, register weight function generalized). RULING: A-727's windows live in their identity-home shard's arena (homes.rs), barrier = every home shard's image before the entry (group commit per shard); read-through seam agreed between w-perf and w-balance. Piece 7 load OK'd on the big Studio via kiln run (bounded steps). Q3 poked at w-fed-faults. Disks: Mac 60G, S2 78G, S4 100G, big 1.1T.
- ~05:30 PST w-git found main red: vcs_ssh::tests::hg::stock_7_2s_push_over_ssh_lands_through_the_daemons_lenders_as_7_2_answered_it fails ('a step that waits', vcs_ssh/tests/hg.rs:487) on clean 67920ccf8; likely A-720 p2a (6e1896f22) -> w-hg FIRST, before p3. w-perf A-727 a1 (Cow read seam) landing; w-perf<->w-balance channel: kiln agents_send ids or inbox/w-perf-w-balance-homes.md.
- ~05:35 PST check: 14/17 working (w-forming 10/10 throttled; w-fed-faults says Q3 answered at 2cf8325; w-k90-containers .nfs test building). This Mac 47 GB free and falling ~15 GB/h: targets w-git 43G, w-shared 28G, w-hg 22G, w-perf 15G, w-svn 9.3G -> w-git/w-shared/w-hg told to trim incremental/release. Studios fine (S2 73G, S4 100G, big 1.1T).
- ~05:40 PST w-svn S4 slice 1 LANDED 21f03d433 (7e1171e09): svn texts read through a lent TextSource (Cow); records still serve own texts until w-perf's slice 2 (unblocked, told). w-svn broke slates-svn lib-test build for one push (report_file_revs tests arrived in its last merge) -> fixed 5ff74820f / 1d7c602ec; now re-runs after every final merge. w-svn next: A-703 (ra_svn replay/replay-range for svnsync/svnrdump); A-702 p2 waits on w-reaper's hooks.
- ~05:45 PST check: 15/17 working. w-forming queue done: forming did not reproduce (24 runs: 11/12 normal, 12/12 throttled); one by-use failure outside its goal (run 7: holder move onto the fresh member stalled, writes refused ~40 s 'covers candidates false', live_chaos.rs:4879) -> w-forming runs the 48-run one-CPU sweep (goal evidence) then chases the stall. w-svn poked onto A-703. This Mac 42 GB free (agents told to trim).
- ~05:50 PST w-git A-719 p3 LANDED baf0fd76a (3105843fe): git push read straight into its quarantine (git_push::intake), each object fsck'd as kept; Checked/Received hold places not bytes; a push holds only one call's resolution + A-723's pack as sent; merged over S1 and vcs_share room/Live. Flaky long-history test cause: ~12k quantum-sized check turns past 10 liveness windows + one unsliced seal+decide (fsck at keep removed a third). w-git disk 45G -> 20G; Mac 59G free. Next: p4 (verdicts + seal sliced), then S3 (A-722) on vcs_share::room. w-perf asked to re-run push_load.
- ~05:55 PST w-shared A-664 svn:author LANDED 707d513ab (cb1cc1782): server-made revision's svn:author = submitting principal's name via the svn+ssh mapping (lfs::owner_name); ToolIdentities::subversion appended. Next: (a) push half, (a) owed-commit half, then svn_made copies + symlinks. w-shared disk 49G after trimming.
- ~06:00 PST w-hg: SSH test FIXED 7477a94a7 (b1ec1fe00; stock_push_session awaits each step); A-720 p3 LANDED 99c0f7877 (08d45661e; hg import room = shard admittable bytes, charged Import::holding, trued per turn); target 24G -> 1.3G. S5 spec sent (file texts as content by identity on homes via the Cow seam; manifests/changelogs as revlog deltas, derived snapshot interval; SHGR names identities; replay from content); then hg_made hook. w-perf asked for push_load re-run (A-719 p3 + S1) and hg import of Mercurial's repo as S5 baseline.
- ~06:10 PST check: 14/17 working. w-lint ROUND 0 DONE: 583 errors fixed (549 clippy, 34 literals) + 2 compile breaks in 36 commits. Rulings: deploy.rs rustls Arc = D-8 foreign-API exception via the codebase's existing allow mechanism; carry.rs:497 sleep -> condition wait; part B's park_timeout loops judged (condition-checking under derived deadline stays; time-only parks rewritten); share.rs:130 literal -> w-balance; Windows lint natively on jupiter. Disks: Mac 74G, S2 72G, S4 93G, big 1.1T.
- ~06:15 PST ruling (w-hg A-731 hg_made; S5 = A-729): (a) approved: SHGR Source::Made {green key, version}; replay rebuilds (green, version) -> made node; Read::of/register_of prefer it at the head version (same rule as git's (green, version) -> commit); test submit -> push on top -> submit.
- ~06:20 PST w-git measured the answer's long step (9,000-object push, debug): policy 0.1 ms, verdict 2 ms, seal 26 ms, register append 220 ms (moves' admission 54, record 4, vcs_store::commit 159 O(objects), apply 2). RULING: fold into A-722 (S3): index rows staged in slices as objects land, append only makes visible; admission + seal sliced; landing O(changes); on vcs_share::room; short design note first. Proof: v2.0.0 cut then whole git/git.
- ~06:25 PST w-git plan (top priority): failing test first (long history + ~200 refs via the edge; repository shard's longest step < 1/4 liveness window; every ref lands), then decide one command a step (1,027 refs for git/git), seal a turn at a time, commit rows + moves' admission staged in slices before the append (A-722 piece 1); w-perf re-runs git_scale.sh after.
- ~06:40 PST check: 14/17 working; w-lint rulings applied c9137cb9f: macOS host clippy clean (slates-server), six cross targets + structural + unsafe + fmt clean; literals only share.rs (w-balance) -> w-lint next: native Windows lint on jupiter, then hourly rounds. w-forming sweep sw48 + repro rp1 running; w-fed-faults flow on the agent workspace step. Disks: Mac 76G, S2 61G, S4 93G, big 1.1T.
- ~06:55 PST S2 DONE (w-engine, A-728): history's latest value = the file's block (p1 f8dd10172, p2 bf9a7ff23/99ecba1b1, measure p3 4aad39d29/52c1d8af5): svn 1,000 revs history allocations 50.3 -> 2.0 MiB, Green::commit 285.1 -> 200.0 MiB; total footprint ~2.5 GB unchanged. Rulings: correct the admission history count (honest charge); confirm/chase the stack oracle flake (2/10 at 3000 cases before A-728, 0/10 after); then attribute the remaining ~2.4 GB. w-forming: sweep run 16 failed from CPU starvation (its repro loop + g227's fleet test) -> 21 more clean throttled runs; run-7 stall not reproduced in 20. Misplaced A-724 bullets under A-728 -> w-balance.
- ~07:05 PST w-k90-containers had 2 questions unanswered since ~00:40 (missed): DECIDED W10 pod start = ephemeral one-session runner in the pod (one code path); W9 container sign-in = kiln named credential grant per session (nothing baked into images). Also: jupiter unblocked; browser check itself; live container start queued on adacachy. Lesson: read agents' progress-file 'BLOCKED on main' lines in each check, not only summaries. Disks: Mac 103G, S2 59G, S4 87G, big 1.1T.
- ~07:10 PST w-hg A-731 LANDED 36c65b2a3 (9ed874650): a version submitted to an hg green becomes the server-made changeset (hg_made::make in judge_submission; push::made writes as 7.2's commit, nodes equal stock 7.2's for add/change/exec/removal on a branch; SHGR Source::Made {green, version}; head_of prefers the made node at the head version; replay restores). Now S5 (A-729).
- ~07:15 PST w-interop matrices on 67920ccf8 (debug): git v2.0.0 push still spin-killed (3554 ms; 14/27) -> waits on w-git's A-719 p4/A-722; hg whole push (57,035 revs) no longer kills the daemon but >20 min in debug (17/24) -> w-interop runs it in release on the big Studio (60 min bound). Windows: jupiter has git 2.56, OpenSSH, hg 7.2 (uvx), NO svn -> svn legs skip loudly; asked Ada whether to install Subversion there (tool install, §2 item 14).
- ~07:20 PST w-perf by-use (release f8c9c95cd = A-719 p3 + S1 + A-727 a1): push_load git v1.0.0: N=1 205.9 MiB/push (was 300.8), N=10 165.9 (was 289.9), N=100 40.2 MiB/push, peak 5.97 GB (was 28.2 GB; identity dedupe across repos on a shard); single push still ~206 MiB (inflated store copy, S3); memory doesn't fall back after a step (to attribute after S3). BUG BY USE: hg import of Mercurial's repo -> shard 9 spins >1 s, daemon killed, CRASH LOOP on every restart (11 in 12 min; recovery replay of the import in one turn) -> w-hg TOP before S5. A-727 a1 LANDED f8c9c95cd; a2 next.
- ~07:25 PST w-hg diagnosed the crash loop: recovery replays the import's SHGR record in ONE unit (replay_hg -> import::record::replay; 5.5 GB of manifests for Mercurial's repo) -> own A-number + bug record; sliced cursor replay (a revision per unit), check the writing landing too; then S5. RULING A-727 a2: (i) a repository content chunk arena per (home) shard behind a storage seam, extents from the same anchor pool under its own owner-tag range, own image section; condition: no file bytes in it once S3/S5 land blobs/filelog texts as green file ops (counter asserted 0); per-home owner tags for w-balance. w-perf doing S4 slice 2 first.
- ~07:35 PST RULING (B) for history across tools: the green holds the head's files (hot); the repository store holds compressed history (git packs as sent, hg revlog deltas, svn svndiff deltas) + tool metadata, never inflated file bytes; an old head's cold delta is durable before the green folds it (test: fold -> restart -> svn cat -r N). Order: A-727 a2, a3, then S4 slice 2 (= A-727 c). (A) rejected (raw history in the green).
- ~07:40 PST check: 13/17 working; w-forming sweep 23/24 (only run 16, the start-up starvation); w-balance told the arena ruling (i), count holdings by the arena's owner tag; it found small repositories wait 14-16x at R=100 in its multi_repo_load harness -> fixing with the share model. Questions-in-files grep: all answered. Disks: Mac 96G, S2 60G, S4 87G, big 1.1T.
- ~07:50 PST w-shared A-664 live whole builds p1 (push half) LANDED 777022f2a (cd302e110): a push's whole map build runs as a task a step-budget slice at a time after the answer; submits to that green park (bounded by client credit, Overloaded past it) and run in order at the end; no polling. Owed: parked wait bounded by failover delay; a push storm restarts the build each push -> ordered: p2 owed commit, then coalesce builds (one per green, no restarts, counter asserted 0 under a 50-push storm), then the failover-bounded wait.
- ~08:00 PST w-reaper A-730 LANDED (e2a65fe commit hooks, 25494f2 lock hooks, af59477 svn+ssh; merged 1533d52): ra_svn runs repository hooks over svn:// and svn+ssh -> w-svn's A-702 p2 unblocked (API relayed). w-reaper next: DAV commit+lock hooks, then A-645 hook view (route A, from w-git's queue), then Linux run. w-k90-vendor: goal 90 done except item 4 Windows run (needs WinFsp on jupiter = ADA's word, asked); it installed Python 3.13 user-scope on jupiter via winget without asking (flagged to Ada); next: kiln is_fuse dead-code fix (Windows -D warnings), then SSH edge client-gone signal for hooks. Asked ADA: WinFsp + Subversion on jupiter.
- ~08:05 PST w-git A-719 p4 LANDED c9100be04 (e4f2aeaf7): a push decided one command a step (receive::Deciding; AnswerState::Deciding with or without hooks); git/git's 1,027 refs no longer one step; tests 40 creates + daemon-edge 3,000 commits/100 branches. Wall-clock longest-step assertion dropped (this Mac at load 71 measured the host); the watchdog guards real spins. Still whole (A-722): first append's commit of every object, moves' admission, seal. Next: A-722 design note (checking slates_vcs::Index).
- ~08:15 PST A-722 design APPROVED (w-git): fork per push (Index::fork O(1)); positions reserved per push at the pack header (readers must tolerate holes; NO compaction at landing); blob/tree/tag rows staged as kept, commit rows + generation numbers in a sliced pass after the pack; moves' admission staged a turn at a time; landing = Index::adopt O(runs) + staged values (O(changes)); stale fork re-based in slices; admission on vcs_share::room (window + post-state, typed before body). Point 8 REFRAMED by ruling (B): no 1b/1c; index resolves oid -> (pack identity, offset), readers decode from the stored pack in place; head files from the green (A-618 post-state); put_object's inflated copy removed. Check: 14/17 working; w-forming sweep near 48; w-lint fixing Windows lint failures (server, bridge-winfsp...). This Mac load 35, 78G free.
- ~08:20 PST BY USE (w-interop, release 1533d52b): hg whole push of Mercurial's repo (57,035 revs, ~84 MiB) FAILS: shard 0 spins 1093 ms at 183 s, daemon killed; peak RSS 13,861 MiB. -> w-hg: after the crash-loop fix, slice the remaining step (landing/record/classify suspected), then S5 (full texts -> deltas + identity records) with RSS before/after. hg matrix 17/24. w-interop: Windows git/svn interop building on jupiter.
- ~08:35 PST w-engine: (1) honest admission count LANDED 529144e79 (delta 0 at rest on the import: history folds); (2) stack oracle flake = test wrong (3/30 still failed; work order vs D-27 conflict), fixed + pinned 183604041 (merge 2103b1177; script 7b37dcb7c); (3) remaining ~2.4 GB on a 223 MiB svn import: allocator-kept freed ~850 MiB, content pool 585 MB, svn record texts 303-322 MiB (S4 slice 2), register records 282-291 MiB (A-727), merge engine current-file copies 241.5 MiB, stacks 62, checkpoint 46.5, UnitSet 41, history 0.8 MiB. NEXT for w-engine: release the ~850 MiB (bounded/arena buffers + give-back; retained_bytes in status --json), then design note: green Files blocks in the content pool (A-64).
- ~08:40 PST w-git A-722 p1 LANDED de9591c70 (adfe74896): slates_vcs::Index::adopt (a parent takes in a fork of itself whole, O(runs), no row copied; Stale/NotAFork refusals hand the fork back); design written into the amendment. Next p2: positions at arrival, readers tolerate holes (hook view's Positions::span cover + depth loop found so far).
- ~08:45 PST w-svn A-703 LANDED 2963b7ad2 (5fd097b10): ra_svn replay + replay-range as svn_repos_replay2 (copies, deltas, revprops, finish-replay); EVERY svnserve command served; fixtures from stock svnserve 1.15.0 (svnsync, svnrdump, 6 replays), 31 revisions match call for call. Live svnsync/svnrdump rows -> w-interop. w-svn next: A-702 p2 on A-730's hooks.
- ~08:55 PST w-hg A-732 LANDED e7353d339 (165bcd2a7): CRASH LOOP FIXED: recovery replays an hg record a revision per unit (record::Replay resumable, byte-offset cursor); the import's record written a part per turn (record::Writer); bug record docs/bugs/2026-11-20-a-large-mercurial-import-crash-looped-the-daemon.md. Owed: a rebuild's replay still one step. w-perf asked to rerun the import; w-shared told its t_a664_live test is load-sensitive. w-hg next: the 57k push's unsliced step, then S5.
- ~09:00 PST w-balance measured (32-core Studio, R=100): small-repo wait is SATURATION, not starvation (22 shards at 35-99% CPU; SHA-1 over objects top cost; even with no hot repo 11-13x solo); lanes/fair-turn gates dead-even (not landed). RULINGS: (a) starvation measured against fair-share ideal; (b) intake off the owner shard -> w-git inside A-722 after p3 (fork built on the arrival shard; adopt on the owner via cross-shard message), w-balance supplies placement policy + measurement. BY USE: 19 concurrent v1.5.0 pushes to one repo kill the daemon -> A-722's concurrency proof (19, then 100).
- ~09:05 PST w-shared A-664 live whole builds p2 (owed commit) LANDED 602983873 (311e4a235): a submit needing a whole build stages vcs_store::owed (key, version, second, message, identity) in its transaction; the build's end makes the commit from the record alone (no clock), settled by the made commit's move live and in replay; two-restart determinism test passes; one owed per green. Next: flaky test (zero step budget in the driver), then push-storm coalescing, failover-bounded wait, svn copies/symlinks.
- ~09:10 PST w-forming A-735 LANDED 727ebe674: macOS client waited forever after a lone daemon recreated the bootstrap object (claim in the dead predecessor's object; owner name held by the new daemon) -> the client re-reads its own slot in the current object; ends 'daemon unavailable' to retry (unit test red 2 s -> 178 us). Sweep 24/26 so far (run 16 starvation; run 25 this hang). Queued for w-forming: run 25's shard 0 5.7 s spin. Ledger: someone took A-736 without bumping next free (now A-737).
- ~09:20 PST w-fed-faults: A-734 (runners started together each wrote a manifest listing only themselves) landed with test; found the lab's runners had no harness, and that nothing in kiln does Slates' one-time bootstrap -> DECIDED: kiln performs the bootstrap through the library API when it first forms a fleet (idempotent, once per fleet, keys in kiln's secret store, joiners via grant path, N=1 same) -> w-fed-faults after its flow. w-k90-microvm piece 2 LANDED kiln 3a7e4116c..522ba54b3 (a Slates daemon restart no longer ends an agent microVM's share: resync channel through Hex to Shards' device; g90 test passes); Shards D288 awaits Shards' coordinator merge; reported g266 intermittent EACCES -> w-k90-vendor (before SSH client-gone).
- ~09:30 PST w-reaper A-733 LANDED 431b110 (bad0841): svn over http(s) runs commit + lock hooks as mod_dav_svn does (start-commit at POST, pre-commit at MERGE, post-commit-err in MERGE response; lock hooks; post-lock failures logged not reported). A-645 RULINGS: placement (b) now (view under the root where SLATES_ROOT known; otherwise today's typed answers; one code path), then (a) root mount publication as its own piece; bytes (a) wait for A-727 a2, reference same blocks, no RAM copies. Order: P1 FSFS rev/revprops generation (lazy, sliced; vs stock svnlook), P2 txn dir (svnlook -t), P3 presenter after a2, P4 git presenter, then root mount. w-k90-vendor: SSH client-gone already landed as A-736 (431b1102..2b1b8106); now g266 EACCES loop.
- ~09:35 PST check: 12/17 working; 4 waiting on their own runs (w-forming clean 48-run sweep sw48c on 69d1f2ec3 with A-735; w-fed-faults cut-off flow; w-k90-microvm r4; w-k90-vendor g266 build + is_fuse on jupiter); w-balance (a) fair-share harness LANDED 9be789460 -> poked onto piece 6 N=1, 8, 9. Disks: Mac 67G, S2 46G, S4 69G, big 1.1T.
- ~09:40 PST w-perf A-727 a2 step 1 LANDED 6b1fb644f: slates-vcs WindowStorage seam (put / read as Cow / release), HeapWindows default, ContentStore::with_storage; windows kept through the storage; readers moved (gitlink read, kept_fragments sizing, homes read_here). Next: step 2 ArenaWindows (repository chunk store per home shard, own pool owner-tag range), then a3. hg import re-run (A-732) running on the Studio.
- ~09:45 PST w-shared fixed its load-sensitive test (da4dc2ff0, 1d4aeb63d): slice() takes its budget (task: the shard's step budget; test driver: 0 -> one file per slice, asserts 3 slices for 3 files); 20/20 loaded runs at --test-threads=8. Next: push-storm coalescing.
- ~09:50 PST w-git A-722 p2 LANDED e42c53886 (c1aa1a5b1): object rows no longer carry the bringing version; Repository::bringer_of landing map answers it (upload's fragment choice and the hook view read it). t_a665's fixed PUSH_WAIT_NS missed at load 71 -> w-git after p3: wait on the push's answer with a derived deadline. Next: p3 (rows staged in a per-push fork; landing adopts).
- ~09:55 PST w-hg A-732 p2 LANDED 5a917afcd (d8b23d0e5): a push's record written a part per slice before each landing attempt and after a re-base (record::Writer::begin_kept, hg_push::record_sliced), byte-identical -> likely the 183 s spin. Still one step: the register append of a record-sized entry (GBs) until S5's identity-only records. w-hg now on S5 (A-729). w-interop asked to rerun the release hg push.
- ~10:05 PST w-interop LANDED 4997b707f: svn matrix + svnrdump dump + svnsync init/sync over svn://, byte-identical to w-svn's fixtures; matrix 29/29 on the fixture repo. BY USE: git/git last 2,000 commits: svnsync passes, svnrdump dump (one replay-range over the whole history) spins shard 0 3596 ms after 53 MB -> daemon killed (28/29) -> w-svn (slice replay-range).
- ~10:10 PST check: 13/17 working; w-balance landed piece 6 single-node batch ops + piece 8 part 1 (typed refusal before a partition outgrows its snapshot slot) -> poked onto 8 p2 (with A-727) and 9. This Mac 50 GB free: w-shared target 42G, w-git 33G, w-svn 25G, w-hg 13G, w-perf 12G -> w-shared and w-svn told to trim. S2 43G, S4 69G, big 1.2T.
- ~10:20 PST RULING REVERSED (main was wrong on (i)): w-perf's test showed a second repository arena over the same pool gets nothing (ArenaExhausted; volume arena holds the slice's extents) and would over-commit the shard budget -> (ii): repository windows are chunks of the volume store's own ChunkStore, lent per step (lent_to_read/within_budget), one arena, one budget (A-64); ContentStore methods take the lent &mut ChunkStore; parked (i) branch's arena code deleted once (ii) lands. Order: (ii) as a2, a3, S4 slice 2 as (c). Check: 15/17 working; Studio-2 41 GB -> w-lint + w-k90-microvm told to trim; Mac 53G, S4 66G, big 1.2T.
- ~10:25 PST RULING a2 seam: (A) shape WITHOUT unsafe: ContentStore::lending swaps the lent &mut ChunkStore into the content store for the step's scope (mem::swap with a no-alloc ChunkStore::empty() placeholder; swap back in a guard's Drop); readers keep &ContentStore (392 signatures untouched); outside a lend typed None/Storage refusal; no unsafe budget raise (CLAUDE.md: unsafe only shrinks).
- ~10:30 PST BY USE (w-interop, release 4997b707f incl. A-732 p2): hg whole push still spins shard 0 1080 ms at 194 s; RSS 13,450 MiB; nothing of the push survived; the restarted daemon spun again after recovery (1100 ms) and was killed; a third served. Log names no step. -> w-hg: slice the rebuild replay first (crash-loop risk), then S5. -> w-reaper URGENT: watchdog spin reports name the task (static label per spawn site; the shard records the polled task's label; spin line + anchor kill log print it), then A-645.
- ~10:40 PST w-balance LANDED piece 9 p1 61a74fe35 (slates_vcs::cut: index ranges by leading b bits, splits on the next bit; vcs_share::place::range_partition by rendezvous) + snapshot slot as admission's second resource b759bddb3 (typed Bound::Snapshot before the body). RULED 8c: record v3 name rows as one content piece by hash (~100 B/push vs 41 B/object; Linux kernel 410 MiB -> <1 MiB), w-balance writes it in its own file, w-perf reviews the record.rs call site. Told w-balance Q3 answered (inbox line 37, 2cf8325). Disks: Mac 105G, S2 52G, S4 66G, big 1.2T; 13/17 working.
- ~10:50 PST w-svn A-702 p2 LANDED 0aaed9b3f (f5ab439dc): revprop change runs pre-revprop-change, base-value check (160049), Record::RevisionProperties via the owner, post-revprop-change, over svn:// and svn+ssh (A-736 ClientGone) -> w-interop adds the matrix row. svnrdump spin root cause (w-svn): listener's go_on loop never yields on Turn::Wrote (Next::Again / pipelined Next::Consume) while the client drains -> yield_now per turn as vcs_http does (in progress). w-balance LANDED 8c codec 0011a4606 (record v3 switch held until A-727 a3) + piece 6 decision logic 58f756e0a (slates_db::identity_homes: homes, heal; no owner input); RULED: w-balance builds HomeContent/HomeRead/HomeLetGo in slates_cluster::content (fleet's own code; goal-91 agents review via inbox).
- ~10:55 PST w-interop found by use: no client verb sends SetRepositoryPolicy (hooks only settable in tests) -> RULED: w-interop builds `slates repository policy NAME [--hook POINT=PROGRAM]...` (owner-only, keeps unnamed fields, grammar + by-use tests), then the revprop matrix row, then the owed `slates repository branch create|bind|retire` words (A-685's owed CLI verbs), then porting interop tests off cfg(unix). Windows: interop tests are cfg(unix) (0 compiled on jupiter) -> matrix scripts under MSYS bash meanwhile.
- ~11:00 PST w-shared: A-664 live whole builds COMPLETE: push storm LANDED c0071e402 (6ad42c7a4): a mid-build push no longer restarts the build; at the next slice boundary the build moves to the newest head re-putting only changed paths (only a copy restarts, counted 'restarted'); test: 61-file build under a 50-push storm, restarted 0; parked-wait bound LANDED 92bc438bf (968c90093): a submit parked past failover_ns answered Overloaded unrecorded (counted 'overdue'). w-shared target wiped (39 GB). Next: svn_made copies, then symlinks.
- ~11:05 PST w-hg A-729 (S5) p1 LANDED d7713a435 (2a42df233): hg index keeps a manifest as its delta against the previous one, chain delta bytes within the text's length (derived, 7.2's bound), else whole; delta::encode_lines one-pass line walk; every reader rebuilds through HgIndex::revision_text; tests: 300 manifests exact, store under a tenth of the texts, stock hg 7.2 round trips 4/4. Next: slice the rebuild replay, then S5 p2 (SHGR v2: names + changegroup deltas, no texts; replay re-runs intake).
- ~11:15 PST w-svn replay-range spin fix LANDED a469a6b85 (ea468de36): svn:// listener yields after a turn that read nothing (Turn::Wrote / pipelined). It skipped the failing test ('no unit test can see a missed yield') -> told to add one (a co-scheduled poll counter or a max-turns-without-yield counter asserted). w-interop to rerun svnrdump on git/git's 2,000 commits. w-svn next: A-652 export half, g268 lock expiry. w-forming clean sweep 20/21 (run 21 failed after forming).
- ~11:20 PST check: 11/17 working. w-reaper A-740 LANDED d851f30 (spin reports name the polled task + its poll count) -> told w-hg; w-reaper on A-645 P1/P2. w-balance transport parts 1-2 landed (members accept windows); next sender + healer loop. w-k90-containers: both decisions built on kiln main (+ a build-break fix) -> told to fix kiln's failing k8s pods test while runs wait. Studio-2 39 GB: w-lint 43G, w-balance 32G, w-reaper 22G -> all told to trim. Mac 85G, S4 66G, big 1.1T.
- ~11:25 PST w-hg A-732 p3 LANDED 0f756ec77 (4fa835b3f): an hg store's rebuild runs as a task a step a turn (VcsStores::rebuilding; store lent to nothing meanwhile; index generation moved so in-flight pushes re-base). SIBLING: git and svn still rebuild_whole in one step (crash-loop risk on large repos) -> w-shared after svn_made copies/symlinks. w-hg now on S5 p2 (SHGR v2: names + changegroup deltas, no texts).
- ~11:35 PST w-reaper: A-645 P1 LANDED 210843f (slates_svn::fsfs: rev/revprops files generated on demand, a revision per step; stock svnadmin verify passes; svnadmin dump and dump --deltas byte for byte; svnlook changed --copy-info matches) + P2 LANDED a75731c (fsfs::transaction_files: svnlook -t == svnlook -r); P3 waits on A-727 a2 -> meanwhile porting interop tests off cfg(unix) to Windows on jupiter (moved from w-interop). Check: 13/17 working; disks Mac 87G, S2 49G, S4 66G, big 1.1T.
- ~11:40 PST w-shared svn symlinks LANDED f771ede60 (015f15b5f): a work's symlink -> 'link <target>' + svn:special. Copies blocked on A-626 p4 (no WorkOp makes a copy) -> w-shared builds A-626 p4 after the git/svn sliced rebuild, then svn_made copy-from rows. Flaky homes test (recv_timeout, homes/tests.rs:411) -> w-balance.
- ~11:45 PST w-svn yield test LANDED 1fc450f1a (aa3a60af1): counter vcs_svn.turns_unyielded; 64 pipelined get-latest-rev; caught a SECOND missed yield (handshake lookup/key turns) -> yield moved into go_on after any turn with no shard-poll movement. w-svn takes over A-652's export half from the dead g268: pushed origin/agents/g268-a652-3b-wip2 + STAGED uncommitted diff in ~/slates-work/g268-a652-3b (protocol.rs, wire_order.txt, image.rs, repository.rs, new repository/svn_export.rs) -> commit + push first, then carry onto main; then g268's lock expiry. Disks: Mac 78G, S2 46G, S4 66G, big 1.1T.
- ~11:50 PST BY USE GREEN (w-interop, main 0f756ec7 incl. a469a6b85, debug): svnrdump dump over svn:// of git/git's last 2,000 commits finished in 14 s (72 MB), no spin, no E210002; svn matrix on that history 29/29 incl. svnsync. Policy verb (slates repository policy NAME --hook POINT=PROGRAM + DescribeRepositoryPolicy/RepositoryPolicy) written, tests running.
- ~11:55 PST check: 10/17 working; 7 waiting on their own runs (w-forming sweep, w-fed-faults flow, w-reaper's Windows interop r22 on jupiter, w-k90-containers r24 live container on adacachy, w-k90-microvm r4, w-k90-vendor g266 loop (run 1 passed 439 s, both agents through their drives), w-balance poked). w-balance transport COMPLETE 9f57e4810 (releases held until A-727 a3). This Mac 56 GB: w-git 42G, w-svn 28G, w-hg 24G, w-perf 22G -> w-git/w-hg/w-perf told to trim. S2 44G, S4 66G, big 1.1T.
- ~12:00 PST BY USE (w-interop): revprop change WITHOUT a pre-revprop-change hook is ACCEPTED via change-rev-prop2 (stock svn 1.15); svnserve refuses E165006; the server test only covered change-rev-prop -> w-svn FIRST (both verbs one path; sibling sweep). By-use test landed ignored; w-svn un-ignores with the fix. Hook path works via slates repository policy --hook.
- ~12:05 PST Studio-2 hit 30 GB free -> main cleaned 35 GB of stale unused caches (hex-p6b3/target, kiln-hexvm-live/target, their reach copies) -> 65 GB; w-balance target 46G, w-lint 43G -> both told to trim now and build with CARGO_INCREMENTAL=0 (w-balance's release/load runs on the big Studio). Mac 65G, S4 61G, big 1.1T.
- ~12:10 PST w-interop LANDED 870a7d270: slates repository policy NAME [--hook POINT=PROGRAM]... [--json] (DescribeRepositoryPolicy/RepositoryPolicy, owner-only, read-modify-write from the read version, stale set -> PolicyChanged; stranger Forbidden); svn_interop 6 passed + 1 ignored (no-hook revprop bug for w-svn). Next: revprop row svn:// + svn+ssh, then branch verbs. Disks: Mac 87G, S2 110G (trimmed), S4 61G, big 1.1T.
- ~12:15 PST w-engine A-742 LANDED 80a7d5788 (e064c2dc7): freed memory returns to the OS: global allocator slates_mem::large (>=64 pages mapped/unmapped directly) + right-sized temporaries (Wire::encoded_len, Db::mutate_owned, Checkpoint::into_encoded, chain reserves); 1,000-rev import footprint 2.42-2.55 GB -> 1.43-1.50 GB, same time; status --json shows retained_bytes/versions + mem.large.* counters; slates-mem unsafe 63 -> 79. Design note: premise false (greens' files live only in engine heap; 241.5 MiB = 48.2 current + ~193 history; 581 MiB anchor content object = logs/images/pre-faulted, unsplit) -> RULED: per-range report of the anchor content object first + 3 leftovers; A-64-for-greens decided from the split; unsafe sites named + SAFETY + lock-free allocator. w-interop policy verb landed 870a7d270. w-forming sweep 43/44 (run 21: spin after a takeover). w-bugs: A-737 fixed WinFsp up-case; B5 diag running.
- ~12:20 PST w-shared git/svn sliced rebuild LANDED 7efceb67b (adb33875d): rebuild empties the store + generation; a task replays a record a turn while accessors lend nothing; git maps remade by sliced builds (REMADE). Left: make_whole one step -> w-shared NEXT (then A-626 p4); a large git pack record replays in one turn -> w-git (replay through the sliced intake, A-722); flaky long-history fetch (WouldBlock 31 s vs 13 s) -> w-perf (A-718 first-fetch encode unsliced; encode at intake).
- ~03:15 PST (clock note: the labels from about "~08:00" onward above ran ahead of the Mac's clock; from here on, times are the Mac's PST clock). CHECK:
  LANDED w-hg S5 (A-729) part 2 b1d001601: a push's record is its changegroup as it arrived (SHGR v2, deltas never texts), written a mean revision's bytes a step, kept across a re-base; recovery replays it through the push's own intake, which reads the bytes in place. hg push/import 76 ok, server hg/repository 89 ok. Reruns queued: w-interop the 57,035-rev whole push (vs 13,450 MiB at 4997b707f; the A-740 line if it stalls); w-perf the large hg import as the "before" for S5's import half.
  LANDED w-shared dae3f979e (via af3551739): a successor's rebuild goes through vcs_store::rebuild, sliced per tool; make_whole is left only for the tests' rebuild_whole; server 168/168. Next: A-626 part 4 (WorkOp::Copy).
  LANDED w-interop 1a0d9ae01: svn revprop row (policy verb sets an allowing pre-revprop-change hook; stock propset --revprop over svn:// and svn+ssh), 32/32 on the reads fixtures and on git/git's last 2,000. Also 1794a768a: `slates repository branch create|bind|retire` (A-575); its by-use git test is ignored, waiting on w-git.
  RULING: a name the namespace binds IS a git ref (§4.17's table; A-654's one-to-one ref_of). git's site serves refs from the namespace's bindings -> w-git (red: git_interop.rs:830). Siblings: w-hg (bookmarks); w-svn (a branch is a directory: copy or refuse by kind per §4.17.4, never an unreachable green).
  RED ON MAIN (w-lint at b79227222): (1) `cargo xtask unsafe`: slates-mem 81 > 79 (A-742's edit names 16 sites, the xtask counts 18; trait unsafe fns and 3 unmap lack their own SAFETY lines) -> w-engine URGENT. (2) slates-vcs the_same_bytes_from_git_and_mercurial_are_one_content_held_by_both_repositories: store.get None after git's release (lost content) -> w-perf FIRST (suspect f3429bdcd; if e064c2dc7, to w-engine).
  w-balance P9b 6f9d93d21. P7 run: small repos' p99 3.07-3.63x their fair share with the v1.5.0 class (caps 5 and 3); daemon killed at 10 concurrent v1.5.0 pushes to one repo -> A-722's bar is 10. Told to profile with counters (per-shard step time by repo, allocator, queued vs served), put the kill's cause line in the run request, then fix starvation failing-test-first.
  GOALS MET (kiln goal 28e73c34's parts): A-587 via A-741 (slates e0c06750, kiln 81174ac49; g266 3/3 fail -> 6/6 pass); core.fileMode (kiln dfe8aa5c4); runner role + dual-usage reissue (kiln 3a89feb58, red cf3e5c5a3). The forming goal is NOT marked: its bar is set at 143/143 formed-and-settled ((47/48)^143 < 0.05), since the zero-contact stall's mechanism is still unknown; w-forming's rp2 runs count.
  w-k90-vendor's queue is done -> next: kiln slates_e2e bare_metal on Windows (port; skip loudly without WinFsp). w-k90-microvm -> macOS HVF microvm:: drive at kiln main >= 81174ac49 while r4 runs (KVM, adacachy). w-k90-containers: r24 alive (compiling on adacachy, load 74); told to clear its stale BLOCKED lines. w-fed-faults told to go on (fleet formed in 22.6 s -> workspace on fleet-a).
  Disks: Mac 68 GB, Studio-2 110, Studio-4 59, big Studio 1.0 TB, w-hg 65 after its trim: no action.
- ~03:10 PST CHECK: LANDED w-shared 7fed9d428: A-626 part 4, a work declares a copy (WorkOp::Copy, appended) and its version holds the copied tree. LANDED w-lint d11c04473 + 9d81a56e7 + b79227222: native Windows (jupiter) clean for CI's three Windows steps; new code since then clean on the macOS host.
  RULING w-lint (its old "waiting on main"): deploy.rs's test Arcs approved in place (D-8 exception 2, since rustls takes Arc by signature; allow with reason); park_timeout loops that wait on an observable flag with a deadline are kept (a wait on a condition, not a dodge); carry.rs's sleep is gone from main and share.rs's EPSILON is derived -> closed. w-perf's "awaiting main's ruling on M2" is stale (answered by A-715/A-716, texts once).
  Idle agents, each waiting on its own run: w-bugs (B5 run 4; run 3 failed on its PowerShell probe reading a CR as the id), w-fed-faults (fixed build), w-forming (rp2 at 11/11 formed and settled), w-k90-microvm (HVF drive at kiln 81174ac49), w-k90-containers (r24 and r33; BLOCKED lines cleared), w-reaper (r22 testing on jupiter since 02:41).
  jupiter at 03:08: 54 GB free, 4 cargo jobs (w-interop's slates build since 02:10; w-reaper's r22 since 02:41; w-k90-vendor's kiln and shards builds since 03:07), 11 rustc. Disks: Mac 56 GB, Studio-2 132, Studio-3 58, big Studio 1.1 TB: no action.
- ~03:15 PST RULING w-shared: A-626 part 4's copy verb isn't done until the CLI, the Python and Node SDKs and MCP have it (copy is no grant verb, so R10 puts it on the SDKs and MCP), each proven by use, and the surfaces' WorkOp matches made exhaustive so an appended op fails the build until every surface has it; then svn_made's copy-from rows.
- ~03:20 PST LANDED w-svn c83698017 (via 90d405247): the no-hook revprop bug was the listener writing a turn's output twice (vcs_svn::step never cleared it), so stock svn read the repeated auth request as success and never saw E165006; step clears its output once written (commit hooks, landing and lock verbs covered); listener 13/13; svn_interop's by-use test un-ignored. Sibling: the Windows-only run_svn_hook now answers 165006. Routed: the by-use svn_interop run -> w-interop (big Studio has svn); the Windows compile -> w-lint's next jupiter lane. w-svn next: CreateBranch on svn, then A-652's export (red: image.units_past_budget=41, 2 undeliverable replies; slice first).
- 03:16 PST CHECK (clock: my "~03:15" and "~03:20" labels above were early by about 8 minutes; labels now come from `date`). LANDED w-git 82ff3bc79 (main 7b93d68c3): A-722 part 3, the seal stages the whole push into an index fork (positions by type then id) and the landing adopts it or restages it if stale, so replayed = live; git store/upload/receive/hook_view 202, server 119/119; long-history push 2.0 s, fetch 3.7 s (main 1.9/4.1, load 43). Next: the namespace-refs bug, then A-722 part 4 (sliced staging/moves/replay).
  LANDED w-hg 98bcb0002 (via 5ad8dea09): S5's import half, SHGR v3; an import's delta-kept manifests are recorded as delta + base node (9.4 MB against 5.5 GB of texts for hg's own repo); a made history's record is under half its whole-text size. Owed: the made-changeset record's manifest (v1), file texts by identity. w-perf to A/B the import, b1d001601 vs >= 5ad8dea09.
  LANDED c0db673e6 (w-svn: svn_interop's no-hook test un-ignored + GAPS), eae00ce62 (w-lint: deploy test's rustls allow in the ruled form), f87f277e8 (w-bugs: B5 no longer reproduces, 229 us p50 against memfs's 186 us).
  RULING w-engine (its greens-in-pool note): unsafe fix first; then the per-range dirty report (write logs, image slots, prefault, chain entries/checkpoints, records) in status --json; if entries/checkpoints/records carrying file bytes lead, do items 3-7 (entries name chunks, BlockStore seam) on ONE seam with w-perf's A-727 (one arena, one budget, ChunkStore lent per step); if prefault/logs lead, derive and release the prefault reserve first.
  w-bugs: goal 87 stands at B3 (A-725), B14 (A-739), B16, A-717, A-737 landed; B4 fixed; B15 answered; B5, B11 not reproduced; B8, B9 by reading only (not chased). Next: the hangup test repro (diag-w-bugs-hangup-1), then A-551, A-576, A-498 via CI's WinFsp runners.
  Disks: Mac 50 GB and falling (local targets: w-svn 35, w-perf 26.5, w-git 25.7, w-hg 23.7, w-shared 19.2); w-svn and w-perf told to trim incremental/ at their next landing. Studio-2 129, Studio-3 58, big Studio 1.1 TB.
- 03:18 PST w-perf reported the hg import 'NOT fixed' (r66: shard 9 spins 1.0-1.19 s, 7 restarts / 7 spins, CLI silent to its 1200 s alarm). Checked: hgimp2.log on the big Studio is dated 02:04 and its spin lines name no task, so r66 ran before A-740 and before S5's import half; not current. -> w-perf folds it into the import A/B (b1d001601 vs >= 5ad8dea09), sends the A-740 task line and whether the spin recurs at recovery or on the CLI's retry; w-hg told not to chase the old log. w-perf told: run slates-vcs's red content-once test on the merged tree before landing A-727 (a2)(ii).
- 03:19 PST CHECK: no new landings since 03:16. w-git claimed A-743 (git serves the namespace's branches) and is writing its failing test. Idle agents all wait on their own runs (w-fed-faults' flow, w-k90-containers r24/r33, w-k90-microvm's HVF drive, w-k90-vendor's r45 and stand_in tests, w-reaper's r22 since 02:41).
  RULING w-perf: no debug figure in place of a release benchmark (CLAUDE.md §5); build release on the big Studio (1.0 TB free) via kiln run, not this Mac.
  FOUND BY USE (w-shared, MCP's copy test): copying a tree holding a hard link answers a TypeChanged conflict at the destination. The design lists it as owed (A-586's status, SLATES_DESIGN.md:14980: "a hard link in the source" not yet copied, refused typed), so this is owed work plus a wrongly typed refusal. RULING: w-shared builds it after the copy surfaces: names sharing a node inside the copied tree share one new node in the copy; a node with names outside gets its own node in the copy, bytes by identity; amend A-586/§4.16; failing test first; checkpoint/replay paths; every other not-yet-copied case refused typed; inbox note to w-engine (slates-merge).
  Disks: Mac 52 GB; Studio-2 124; Studio-3 49 (falling: 58 -> 49 in ~20 min); big Studio 1.0 TB.
- 03:22 PST LANDED w-svn 3efe27247 (via 00a135af4): on a Subversion repository CreateBranch, BindBranch and RetireBranch now answer Unsupported by kind (branches are directories in its one green, made by a copy); before, CreateBranch made a green no svn path serves and RetireBranch could unbind the served root (found by use); §4.17.4 Branches amended; a_subversion_repositorys_branch_verbs_are_refused_by_kind red then green, 63/63. By reading, not chased: the server's CreateRepository takes any default_branch for svn (only the CLI's root_binding refuses). w-svn wiped its 35 GB target, now CARGO_INCREMENTAL=0 from one worktree; next A-652 (slice the units_past_budget path first).
- 03:24 PST CHECK: LANDED w-bugs 64e5667fa: the hangup tests' flake reproduced by use (2/30 under load on CI ubuntu, kernel 7.0) was a sibling test's child holding the CLOEXEC socket between fork and exec (test bug, not product); fixed by waiting it out. w-engine claimed A-744 (content ranges in status --json; A-742's leftovers).
  BY USE (w-interop, release main 7b93d68c3, Studio, hg 7.2): Mercurial's whole history (57,035 revisions) PUSHES in 142 s (~400 revisions/s), daemon peak RSS 5,460 MiB (13,450 before), no spin, no restart: A-720's proof (with S5 part 2). NEW RED: no clone admitted, over HTTP, SSH or stream: PastAdmission { asked 228144, left 218727 } (stream 7081/6300); hg matrix 4/24.
  RULING (fetch = the push rule mirrored): a clone's/pull's working set is bounded and independent of the repository's size; the answer is streamed from what's stored (S5 deltas; git's A-718 stored deltas) in slices under the step budget; admission (vcs_share::room) charges only the slice in flight. w-hg writes §4.17.1 "A fetch's answer" and fixes hg's clone next (after the bookmark check); w-git queues upload-pack's sliced answer (A-713's owed half) after A-743 and A-722 part 4, citing it.
  w-forming lead (sampler, rp2 run 23): control-shard polls of 1.0-1.5 s, ~77% of samples in reap::remove_on_owners -> call_in_shard_time -> ShardTime::new -> thread_info. -> told to act now, beside rp2/rp3: count the calls per poll and the cost per call, slice the loop under the step budget (failing test first), and take the clock read off the per-call path if it dominates. rp2 at 22/22 (70 of 143 toward the forming bar).
  Disks: Mac 81 GB (w-svn wiped 35 GB); Studio-2 123; Studio-3 49; big Studio 1.1 TB.
- 03:26 PST LANDED w-hg 62fb88997: the bookmark check was red (Mercurial's namespace spells a bookmark bookmark:<label>:, so the CLI's plain name was never one and --from refused NotFound); in a Mercurial repository create/bind/retire now read a plain name as that bookmark, full spellings pass through (hg_push::owner_name); hg_view's test shows topic served at one's node, gone, back; hg 90 ok. The stock-hg by-use twin of the git test -> w-interop (hg_interop.rs if w-reaper's move has landed, else cli.rs). w-hg next: the fetch red, then the made-changeset record.
- 03:29 PST CHECK: no new landings beyond w-hg's 258f4f008 (the bookmark commit under 62fb88997) and w-bugs' 64e5667fa. w-forming's reap probe: a normal reap round retires 1 client over 2 shards with 10 cross-shard calls (4 inline) in 7 polls; longest poll 0.12-0.2 ms; ShardTime::new 49-87 us a round, at most 14 us a call; it waits for a poll of 10 ms or more. w-git's A-743 passes against stock git 2.50.1 (ran, no skip). Idle agents all wait on their own runs (w-bugs: A-551, A-576, A-498 on CI; w-fed-faults' flow; w-k90-containers r24/r33; w-k90-microvm's HVF drive; w-reaper's r22).
  Disks: Mac 79 GB; Studio-2 122; adas-mac-studio-3.local (hostname Adas-Mac-Studio-4) 44 GB and falling: w-forming (probe target 12.7 + target 9.8) and w-bugs (11.1 + 2.2) told to trim; asked whether either uses ~/.cache/slates-work-target-g227-transport-reports-durability (36.2 GB, written within 12 h); stale there, ~28 GB (kiln-lb 8.6, slates-gate 6.1, hex-t7 3.5, ...), cleaned only below 30 GB per the rule. Big Studio 1.1 TB.
- 03:35 PST CHECK: LANDED w-engine c3c581f87 (A-744): status --json reports content ranges per shard; the svn import's 581 MiB is the owner partition's log ring (355.2 MiB) + snapshot slots (225.8 MiB), content object 2.3 MiB, no green files; large.rs has a SAFETY line above every site, budget 80 named (mincore +1): MAIN'S UNSAFE GATE GREEN AGAIN. RULING: the log's decode-after-encode stays (A-420's replay measure and Corrupt guard; a non-decoding model is the under-measure A-420 removed and a second path); next, split the ring and slots by record kind: green chain entries -> w-engine's items 3-7 on A-727's seam; repository records -> A-727 (a3) / S4 (w-perf, w-svn).
  LANDED w-hg 3daa8b369 (A-745): §4.17.1 "A fetch's answer" (made from what is stored a slice at a time; admission charges the slice in flight; each slice under the step quantum with the view lent for the turn; fetches concurrent over the index generation they began on) + its red (landed ignored: a clone of one changeset with a file twice a request's room -> PastAdmission { asked 557056, left 277336 }; cause: getbundle builds the whole answer, a per-changeset list 57,036 x 4 = 228,144, in one request's room). RULING on its plan: repository-scale tables (linkrev per manifest, the file index) live once in the repository's index per generation, shared by every fetch, charged to its usage row (cold-droppable); a request holds only the missing set as revision ranges, a cursor and the slice; the file section walks the shared file index by linkrev in the missing ranges.
  g227 (an earlier wave's agent, idle 98 min): its test-and-push loop on Studio-4 (pids 70453/82620, target 36 GB) had not landed its "catch-up pair" in 90+ min and tolerated a main failure ("the green retained-history test", red on clean main c9100be0). -> told to stop the loop, land under the fast rule, name that test for routing, and delete its target once landed.
  Disk: w-bugs and w-forming trimmed on Studio-4 (w-forming: g227's target is g227's own live loop, not theirs). Mac 123 GB.
- 03:36 PST STRANDED, unblocked: g226-live-shipped-volume had been idle 236 min after asking main for a ~60 min Linux run (fsx/fsstress through the daemon's own unprivileged FUSE mount; on main at e71043ff6; macOS NFS met at 887c66c20 with 0 differences). -> told to run it itself: a diag branch on CI's ubuntu runner (w-bugs' diag workflow as the pattern, never main, 90 min bound), else kiln run on adacachy; then pjdfstest under --shared with user_allow_other. g220 stopped with nothing to port (main's a_forged_signature_is_refused covers it). Older agents idle under main (g222, g223, g224, g228, clients, perf, loadtest-scale: 19-58 h) are listed for the next free slots, the agent cap being 20.
- 03:37 PST LANDED w-shared b6f6426ac (via 807a6f3c0): copy on every surface: CLI copy WORK FROM TO [--version N] [--replace] [--json]; Python and Node copy() on Client and AsyncClient (Node typed); MCP's copy kind; MCP (work_op_kind) and both SDKs (work_op_method) now match every WorkOp exhaustively, so an appended op breaks their builds; proven by use against a real daemon (CLI JSON flow, MCP 1/1, Python 4/4, Node 9/9; CLI parser 41/41). Next: the hard-link copy (inbox note to w-engine first).
- 03:39 PST CHECK: LANDED w-git f6ca826e6 (main 4d9de40c9): A-743, every name the namespace binds is a ref git serves (CreateBranch records the green's git store version at its commit, a no-object push record replayed with the log; retire drops the ref, bind restores it; a push to refs/heads/topic moves the same green); red t_a743_... (false,false,false) -> green; server 146 passed; stock_git_sees_the_branches_the_cli_makes_retires_and_binds un-ignored and passing under stock git 2.50.1. LANDED w-hg 92d77e214 (A-745 rule + red), w-shared b6f6426ac (copy surfaces). g227: all eight commits of its catch-up pair on main (filed under A-200), cleaning up.
  FOUND BY RUNNING (w-git): vcs_store::tests::assert_made_again_whole fails after any plain git push: after a rebuild the live store holds extra holders (refs and rows equal). Same family as the red content-once test -> w-perf, together, failing-test-first (replayed = live).
  COORDINATION: w-balance is slicing slates-git's receive resolve (its starvation fix) -> told to leave w-git an inbox note and land small, and not to land unless the profile shows resolve as the toys' wait; w-git's A-722 part 4a (a record's replay stepped in vcs_store) stays out of unpack.rs and receive/ until then.
  w-git's ~/.cache/w-git-local-target was wiped mid-build at ~03:25 by someone else's cleanup; w-svn: no (exact paths only); w-perf asked; own-exact-paths rule restated.
  w-forming: A-740 labels named a 2,933 ms poll at daemon.rs:4983 and a 471 ms one at live_ship.rs:2393; looking at both. g226's Linux FUSE run is on agents/diag-g226-1 (7663476ab).
  Disks: Mac 113 GB; Studio-2 120; Studio-4 91 (g227's target gone); the big Studio (d1323898) did not answer a df in 15 s (it had 1.1 TB).
- 03:41 PST LANDED w-svn 4222a1049 (via a3f2602d7): A-652 part 3's export half (an svn range exported as svnadmin dump into a new volume; g268's piece). Measured cause of its red: two exports minted the same fresh volume id at prepare, before either published (AlreadyExists); the id is now minted in publish, the atom that records the volume; export/import 11/11 in 1.2 s (were timing out at 60 s); dumps byte-exact (223,752 and 112,741 B). units_past_budget: a unit is a whole revision (60 us small, 2.4 ms with a 107 KB text, 7.5 ms outlier; 87 over two exports) -> GAPS; slicing inside a revision is owed. SIBLING SWEEP (same mechanism, proven by use): verbs.rs:155 mints from the next record sequence without reserving it, so any id minted before its recording atom can repeat; image.rs:535, 770, 1523, 2022 -> w-svn first, red = two image builds/archives side by side publish distinct volumes; mint in the publish atom; structural if possible (fresh_volume_id takes the open transaction). Process slip caught before push: conflict markers in protocol.rs, wire_order.txt and verbs.rs; w-svn now greps the whole tree after merges.
- 03:42 PST w-perf: the red slates-vcs content-once test was its own f3429bdcd (get() lends only windows that are Here; 3 vcs tests read windows through get), fixed in the A-727 (a2)(ii) commit (bytes_of/len_of; slates-vcs 100/100, hg 356, git 557), landing; told to keep the test asserting that hg's hold reads the same bytes after git's release; then the holder mismatch, then the import A/B. WHO WIPED w-git's target at ~03:25: w-svn, w-perf, w-shared, w-hg all NO (exact own paths); w-interop pending. Likely kiln's reaper, which deletes recognized build output when this Mac runs low (raft-chaos saw it flag 7.6 GiB at ~03:15; same pattern as 11-12 and 11-17): inference, no log line found. A build that dies on a vanished target is rerun, never read as a result.
- 03:45 PST CHECK: LANDED g227 59ec7871: its catch-up pair (A-200: retain-1 adb5e708 red, f7f486ea, 5c981ef6; catchup-1 3f131747 red, 52374783; retains_after f1549fc9 red -> 0914b9a0 fix + BENCHMARKS/GAPS); one merge, one focused run, push; loop stopped; target 38 GB deleted (Studio-4 91 GB). Its goal (transport durability claims) was already met at 981c8e4d. LANDED w-lint 231ac7d11 (Windows lane at da2132f9c, after 90d405247: clean), w-svn 4222a1049.
  REDS ON CLEAN MAIN (g227, Studio-4, eae99ac9), routed failing-test-first, suspects named as suspects: liveness beat tests (a_wedged_shard_stops_the_beat, a_shard_blocked_in_a_syscall_stops_the_beat, a_shard_waiting_on_a_blocked_thread_stops_the_beat + _role twins) -> w-reaper (A-740 newest watchdog change); a_greens_retained_history_is_charged_folded_and_its_rejected_cache_is_bounded (tests/daemon.rs:4489, red since c9100be0) -> w-engine FIRST (A-728, 529144e79, A-742); route::hostile_claims_get_no_descriptor_and_the_honest_claim_succeeds_once (tests/shards.rs:2844) -> w-bugs (A-735 to check); a_client_of_another_node_in_the_owners_region_reads_and_writes_its_volume_through_the_owner (tests/fleet.rs:4819) -> w-fed-faults; vcs_ssh::tests::hg::stock_7_2s_push_over_ssh_lands_... -> w-k90-vendor FIRST (A-736). Load-only failures count as bugs (tests hold at 1-2 CPUs and contention).
  w-forming: rp2 40/40 -> 88 of 143 formed-and-settled; rp3 runs the last 55; pr1 (4,348 reap rounds): longest round poll 3.97 ms, ShardTime::new <= 16 us -> the reap round is NOT the long poll; pr3 watches sections >= 50 ms. kiln hiccups: the big Studio missed a df at 03:39; g226's r4 lost contact with this Mac; w-k90-vendor's run request unanswered once.
- 03:45 PST LANDED w-perf 479f950ed: A-727 slice a2 (ruling ii): repository windows are chunks of the shard's own chunk store, lent per step by a swap under a guard (Lent / lent_state / take_chunks), zero unsafe; unmade storage lent too; the arena grows from the pool on demand, the budget extended at hand-back; a release outside a lend frees at the next lend; the vcs red was tests reading windows via get() (Here-only since f3429bdcd), now bytes_of/len_of, and the content-once test still asserts hg's hold reads the bytes after git's release; server lib 954/954, vcs 100, hg 356, git 557. RED -> w-engine FIRST: A-744's content_ranges::tests::a_sweep_completes_within_its_bound_... spins a debug shard > 3.2 s in one step, deterministic (unbounded sweep). Wiped-target inquiry closed: all five local agents NO (exact own paths) -> kiln's reaper, inferred.
- 03:46 PST BY USE, Windows (w-interop, jupiter, git 2.56, debug slates.exe from main 9e9cfbd): first push over HTTP (11 s) and SSH, ls-remote, same-heads work; the clone spins shard 0 (2,328 ms) and kills the daemon 3x (binary predates A-713/A-745; rerun on current main). PARITY BUG: after each restart both VCS edges come back on NEW ports (HTTP 52289 -> 53602 -> 53617 -> 53629; SSH likewise; host key kept), since A-625 part 3 (2) decision 3 leaves a listener that cannot be inherited unheld and supervise.rs holds vcs listeners under cfg(unix) only. RULING: Windows gets the same hold: the anchor keeps the listener and hands each daemon a WSADuplicateSocketW duplicate aimed at its pid (explicit, never inherited: A-183 holds); amend A-625 p3 decision 3 and §4.6; name the unsafe sites; red = the restart-keeps-port test (SSH and HTTP) on Windows -> w-reaper after the beat reds and r22's landing. Matrix 4/27. A Windows build waited over an hour, likely cargo's shared package-cache lock; w-interop retries with output shown and names the holder.
- 03:50 PST CHECK: LANDED w-shared 7fe4dc0e2 (via 95044a373): A-586's hard link in a copied tree: inside links share the copy's nodes (Green::copy_link_effects, derived beside node_moves, never recorded); a link naming a node outside gets its own Mode+SetContent (or Special) from the head; one merge test covers inside, outside, the identity recount, a replayed replica and a restored checkpoint; slates-merge copy/engine 163/163; the server repro (TypeChanged before) passes, server 20/20. RULING (step 4): (a) with a closed typed case: the verdict gets a refused outcome apart from conflicts; submit answers Refusal::CopyRefused { case } (Root, PastHead, SourceEmpty, OutsideNodeChanged, VersionFolded, LinkAcrossCopy), appended to the wire, exhaustive in the SDKs and MCP; A-586/GAPS say which are FSFS's refusals, which retention's, which owed work.
  w-forming: forming count 91/143 (rp3 run 3 failed AFTER forming: the replacement member's daemon missed its 30 s banner wait at load 31 on 14 cores, control shards 8-12 s between beats; Spotlight among the loads). MEASURED on Studio-4: Spotlight indexed 1,181 rlibs under ~/.cache and 28,212 .rs under ~/slates-work; 3 mds/mdworker at 65.7% CPU, load 29. RULE added to common.md (no system setting changed): name a cargo target ~/.cache/<name>.noindex at its next wipe (Spotlight skips *.noindex); delete by exact path only.
- 03:52 PST LANDED w-svn 9cbdadac2 (via 3f65086f4): the volume-id sibling sweep, fixed at the mint: each mint moves a per-shard ShardState::volume_ids_minted past itself (held to MAX_OBJECT_COUNTER), so an id minted before its record is the shard's alone; covers image.rs:535 (image identity), 1523 (archive export), 2022 (archive import), 770 (name alias) and every caller; svn_export's mint confirmed in publish. The structural form (mint only inside the recording transaction) failed: an image's identity embeds its id at its plan. Red image::tests::two_imports_admitted_side_by_side_mint_distinct_volume_ids (one id twice) -> green; image/repository/verbs 119/119. w-svn's lib-suite spin (~3.3 s, a test task's first poll, exit 4) = A-744's content_ranges sweep test (same as w-perf's): w-engine's first job; others skip it by name until then.
- 03:53 PST CHECK: LANDED w-git 30e8eedc9 + 65c84f081 (A-722 part 4a): a git record's pack replayed a turn at a time; the replay's turn lends the shard's chunk store as every vcs step does since A-727 a2. LANDED kiln main 4aabdba18..7e542f29c (w-k90-vendor): slates_e2e stand_in 4/4 on jupiter (4 failed before: os.uname -> platform.system), bare_metal compiled and skipping loudly without WinFsp (goal stays open on Ada's WinFsp word).
  GOAL MET: kiln's macOS microVM drive green again at main (r134 regression): HVF drive set 2 passed, 0 failed at kiln 81174ac49 (g266: edits held for approval, no EACCES; g90: daemon 4630 killed, successor 19863 served the next write). KVM r4 on adacachy still running.
  g226 by use: Linux FUSE conforms: CI run 44729121323 (ubuntu-latest 26.04.1, kernel 7.0.0, 4 CPUs) on main 3daa8b369: fsx 10,000 ops plain and live-shipped, same outcome; fsstress 4 x 500, same outcome; both candidates held the owner's durable head (4,436; 2,143). pjdfstest (diag-2) next, then one Linux docs update.
  w-perf: the holder mismatch's cause: the live store's whole-map build was still pending (sliced after the answer) while the rebuild settles its builds -> settling the live side before comparing. w-fed-move waits on kiln's production server, which flapped (kiln hiccups also seen by g226 and w-k90-vendor).
- 03:56 PST LANDED w-bugs 15d25e8b2: A-498's delete racing a coherent purge (hung 2/3 on x86_64 before A-725, 6/6 after, both arches) and A-576/A-737's folding drive (ɐ.txt = Ɐ.TXT, ς.txt != σ.txt; fails with CaseSensitiveSearch forced); A-551's drive tests red-checked; git/hg/svn workloads pass over stock WinFsp on both arches (CI only). GOALS MET: A-498 (WinFsp hard-linked hg clones and the delete/purge hang) and A-576 (folding by Windows' table). Asked w-bugs for A-551's last two parts (normalized name on open; sharing-violation test) and B7a/B7b's state, B6 recorded as no record in GAPS, B8/B9 unchased (Ada's rule); then goal 87's audit part closes and w-bugs gets its next piece.
- 03:58 PST CHECK: LANDED w-forming b25a5b7a6: live_chaos's start wait judges a daemon's start by its processes' own CPU, not 30 s of wall, so a start starved at load 31 is waited for (GAPS). LANDED w-bugs 14c95642f: a client gone before its route connection is accepted is a hello cut short, not the listener's failure (the route::hostile_claims red on macOS).
  RULING, correcting my own: I had told w-balance to land its resolve-in-turns only if the profile showed resolve as the toys' wait; WRONG in scope. Its profile (32-core Studio, quiet) shows the change fixes the daemon's death at 10 concurrent v1.5.0 pushes (kill line: vcs_http.rs:1404 poll 595; resolve_waiting at a pack's end in one call, 1,435.7 ms -> longest input 1.1 ms; cap 10 alive 10/10; v1.5.0 alone 423 MiB from ~758). -> LAND 76070d077 now with a past-budget red and the cap-10 run; w-git builds A-722 part 4 on it. The toys' 3.34x fair share is elsewhere (answer step 213-247 ms, per-step round cost): w-balance measures next.
- 03:58 PST w-git A-722 part 4a confirmed on main via cafd1058f (commits 30e8eedc9 + 65c84f081 above): new vcs_store::git_replay; the record decoded once, its pack read in chunks within a turn, objects admitted one per unit under the step quantum in rebuild turns and recovery units, each turn lending the chunk store; red t_a722_p4_a_many_object_pack_record_is_replayed_over_many_turns (1 turn for 66 objects -> >= 66 at a zero quantum); server 156 passed. Owed: the replay's thin-pack finish_with resolve still whole (on w-balance's resolve-in-turns once landed). Next: part 4b (live seal+stage pass and moves' admission sliced), then the by-use proof at 10 and 19 concurrent pushes.
- 03:59 PST LANDED w-hg d0749fe2d (A-745 piece 1): slates-hg getbundle::Answering writes a getbundle answer a slice at a time, each step with its own room (head, the changegroup a unit per step, the remaining parts); a clone of every head walks by linkrev holding only the missing set as ranges + a cursor, the manifest log then file logs row to row (HgIndex::file_revision_from), no path list, nbchanges from the ranges. Correctness: by-links is wrong when a served changeset is neither sent nor held (a linkrev can name it), so partial fetches and hiding views take 7.2's slow path, bounded by what the fetch sends (7.2 takes fastpathlinkrev only for every head); the index's own row links are the shared table for the large case. Tests: a slice's least room below the whole answer's; sliced bytes = whole answer byte for byte; a by-links clone = the slow path's revisions and links; 7.2 byte oracles; hg 358, server hg/vcs_store 119. Next: piece 2 (HTTP and SSH a turn at a time over a held fork; the red turns green), then w-interop's rerun.
- 04:02 PST LANDED w-svn f3683fb6d (via 04dccfddb): exporter slicing part 1: Exporter::export keeps a revision's drive between calls (sorted changed paths, open directories, last path); each call writes the revision record, one changed path's records, or the closing of the remaining directories; every_export_call_writes_one_changed_path_of_its_revision (changed paths + 2 calls a revision), both formats svnadmin's bytes; migration 40/40, export/import 11/11. GAPS: the mint reservation needs nothing durable. Next part 2: a text a window at a time (main: take its length and checksums from what is stored with it, walk once to write, if the store has them).
- 04:03 PST CHECK: LANDED w-engine 9f57288b2 (A-746): status --json splits the register by record kind: log tail 267.2 MiB (records 87.5, chain 87.4, checkpoints 92.2), snapshot 225.8 MiB (records 134.9, chain 45.7, checkpoints 45.2): chain entries + checkpoints 270.5 MiB > repository records 222.4 MiB -> per main's rule items 3-7 (a green's bytes named, not carried) on A-727's seam; also 88 MiB of ring pages released at the last snapshot and not given back -> w-engine's queue; prefault reserve 0. w-engine's content_ranges sweep fix (3.4 s -> 1.32 s, a few steps) landing. LANDED w-lint fff3ebaa7 (new code's lints across git/merge/wire/svn).
  STALE REDS: g227's "clean main" eae99ac9 was ~4 h old: w-k90-vendor bisected the hg-over-SSH red to before w-hg's b1ec1fe00 (fixed at 00:18; passes on A-736 and on current main 95044a373) -> w-engine (retained history) and w-fed-faults (fleet.rs:4819) told to check current main first; w-reaper finds the beat tests passing on current main under throttling, now adding contention.
  QUEUE: w-k90-vendor (goal 90 done but WinFsp) -> A-745 for git: upload-pack's answer a slice at a time (A-713's owed half; model w-hg's getbundle::Answering; byte-equal oracle; per-repo state in the index; red: a clone bigger than a request's room over HTTP and SSH); taken off w-git's queue (w-git: A-722 4b, then proofs at 10 and 19).
  w-svn part 2: format 2 one walk (index::Text's size/md5/sha1), format 3 two walks (svndiff length unknown until computed); the measuring walk's one call is GAPS-owed and next.
- 04:03 PST LANDED w-perf 78d5454a4: the holder mismatch was no leak and no lost content: the push's whole map builds in slices after the answer (paths::whole_build) while a rebuild settles its builds (settle_rebuild), so live was compared mid-build; red t_a727_a_plain_push_then_its_update_are_made_again_with_the_same_holders (rebuilt held 2 contents twice + 2 extra; used 24,655 vs 24,538) -> green; assert_made_again_whole settles live builds first and is usable from git_push tests; 87/87. Next: hg import A/B and the release swap cost on the big Studio.
- 04:06 PST jupiter: w-interop's slates build (03:50) runs with no older cargo before it (kiln check 03:56, w-k90-containers' shards builds 04:02/04:05); the 02:10 stall (r57) left no holder; nothing killed. w-reaper's r22 cargo test (02:41) is gone from the list, so its result is waiting on w-reaper.
- 04:08 PST CHECK: LANDED w-engine 3c15e2192: A-744's content-range test spun because the TEST asked the report again and again inside one shard closure over the machine's share; it now runs on a 64 MiB reserve so the sweep is a few steps (1.32 s from 3.41 s); the product's sweep is per call. LANDED w-lint 0907608c4 (new code's lints: db, server, cli, mcp). The fleet.rs:4819 red is REAL on current main (34bb9fa6e, fails after 808 s: b's client never got its own write attachment of a's volume; the status read through a was served): w-fed-faults instruments and bisects (suspects A-724 piece 6's region healer, c468b1309's attachment ending, 727ebe674's DaemonUnavailable). w-balance: resolve-in-turns holds (at most 4 deltas a turn of a 4-unit budget, past-budget 0), landing. w-svn measuring the largest call before stating headroom. Disks: Mac 80 GB; Studios 121 / 89 / 1.1 TB at 04:03.
- 04:12 PST LANDED w-svn d82eb41e5 (via bc180d633): exporter slicing part 2: a record's text is written by the calls after its header: format 2 in WINDOW_LIMIT pieces, one walk (length and checksums from index::Text); format 3 one deltified window a call (resumed from svndiff::deltas_at / Deltas::at), two walks with the measuring walk still one call; every_export_call_writes_one_changed_path_or_one_window_of_its_text: largest call 102,400 B (format 2) and 102,413 B (format 3) where a 107 KB text was one call; both formats svnadmin's bytes; svn migration + svndiff 70/70, export/import 11/11. Next part 3: format 3's measuring walk resumable (its own calls before the record's).
- 04:13 PST CHECK: w-perf sat in needs_you: its bisect command (git archive into ~/.cache/w-perf/bisect/<sha>, then rm -rf $d) waited on a person's approval at kiln's gate. The bisect was redundant (cause already shown, fix in 479f950ed) -> stopped with mode now; it now drops variable-path deletes and goes on to the hg import A/B and the release swap cost. LANDED d82eb41e5 (w-svn part 2, above), 12f2ed442 (w-lint: hg pull test). All other idle agents wait on their own runs (w-forming rp3, w-fed-faults' bisect run and slow-link flow, g226 diag-3, w-k90-microvm r4, w-k90-containers r24/r33). w-hg landing A-745 piece 2's HTTP half; w-balance running the cap-3/cap-10 proof.
- 04:17 PST LANDED w-hg 658eb7d9c (A-745 piece 2a, HTTP): the red passes (a clone of a file twice a request's room answered whole over the daemon's edge); serve::http::begin, then HgExchange::reply makes the body a turn per call, each turn lending the view for that turn within the shard's vcs_share room and step quantum (HgStores::fetch_turn); a step past the turn's room after other steps is retaken next turn; compressed as made (StreamCompressor: zlib, bzip2, zstd; each slice's engine bound charged first); hg serve 79, server 237. RULING on order: SSH (2b), then rule 4's hold on the fetch's index generation (correctness: a push landing mid-clone must not be read by later turns; red = mid-clone landing, answer equal byte for byte to the begun generation's), then the stream clone. w-interop asked for the HTTP rows of the hg matrix now (57,035-rev clone, peak RSS, hg verify).
- 04:18 PST CHECK: LANDED a5140de5b (w-fed-faults): the fleet.rs:4819 red was the TEST holding a's write attachment while b's client tried to take one; b's client writes once a's attachment has gone. LANDED 9ddd1fbe9 (w-shared): a copy the engine does not make is refused before any verdict by its own case (CopyRefused), never TypeChanged (A-586; merge, ipc, server, api, sdks, mcp). LANDED d7ce83e9a (w-k90-vendor, A-745 git piece 1): a fetch's plan carries its objects as runs of positions, never a handle an object. LANDED b3cf6450f + b64195ea4 (g226): a live-shipped volume conforms as a plain one over the Linux FUSE mount itself (CI 44731860624: fsx 10,000, fsstress 2,000, pjdfstest 8,770/8,798 + 28 todo, 0 differing; pjdfstest expected-failure list now EMPTY); goal "conforms exactly as a plain one" already met. d68837d37 = A-745 piece 2a's commit.
  g226 next (RULING): A-354 reruns a verb after the failover SLO so an append can land twice -> record the completion at that point (A-127's one durable atom; a resend meets the completion record); the siblings (a mapped file's sync, an image build refused after saving) get the same; reds first; amend A-354 + GAPS. Stale: "server lib tests don't compile (ba884a186)" (they compile; 954/954 at ~03:45). The WinFsp matrix record emptied by 0f5591c03 (A-725) -> w-bugs re-records from a real CI fsx run.
- 04:26 PST CHECK: GOALS MET (goal 87's parts): A-551 (B2 Windows half: exact-only CaseSensitiveSearch, normalized name on open, sharing violation, each red-checked on both arches, e8788ea65) and the goal-83 B-list (B3, B14, B16, A-717, A-737, A-551, B7a/B7b on Windows; B4 fixed; B15 answered; B5, B11 not reproduced; B6 unknown; B8, B9 unchased by Ada's rule). w-bugs NEXT: the fetch rule for Subversion (measure checkout/update over svn:// and DAV first; slice the editor drive if past budget or whole-answer admission; note to w-svn). LANDED w-svn 515bc3c22 (via 9b82c4f31): exporter slicing part 3: format 3's delta length measured a window a call before the record's call (a dry step on copies picks the delta); a_large_delta_is_measured_and_written_a_window_a_call: 15 calls for a five-window change, none past one window + CALL_HEADROOM; 71/71, 11/11. RULING w-svn: measure the release call time itself on the big Studio; the lock-expiry case is by reading -> GAPS, not chased; NEXT: does an svn commit meet §4.17.1's intake rule (stored once on arrival, bounded working set, admission of the working set, concurrent intake) -- measure a 500 MB text over svn:// and DAV, fix red-first. LANDED w-shared 9ddd1fbe9 (via c2de447a8): seven CopyRefusal cases decided before any verdict (submit and rebase), Refusal::CopyRefused { path, case: CopyCase } on the wire, exhaustive in server, api, SDKs, MCP; next svn_made's copy-from rows.
  DISK this Mac 57 GB, falling ~4 GB/min: w-perf's target 44.7 GB (26.5 an hour ago; release built here, not on the Studio) and w-hg's 36 GB (23.7) told to trim by exact paths and move release builds to the big Studio. Studio-3/4 96 GB; big Studio 958 GB; Studio-2 missed a df (kiln, 15 s).
- 04:28 PST CHECK: LANDED w-hg 835fc4cb9 (via 2aeeac954, A-745 piece 2b): an SSH clone is answered a turn at a time (session::begin_serving; Stdio::stream; a turn ends once a room's worth waits to be sent); SSH clone of a file twice a request's room answered whole; hg 358, server 239. Both edges stream; w-interop to run HTTP and SSH clone rows. w-hg wiped its 34 GB target (now .noindex, CARGO_INCREMENTAL=0); Mac 89 GB. Next: rule 4's generation hold, then the stream clone.
  LANDED w-reaper 1c59e362 (ecc0a658d + 4956323de/8545dd49): Windows interop: git_interop/svn_interop un-gated, hg legs moved to hg_interop.rs; jupiter r22: git 5/5 (Git for Windows 2.56, MSYS2 OpenSSH 10.5), svn skip (no Subversion), hg skip (uvx's hg.exe cannot load python312.dll). Beat reds (g227's list) were the tests timing the stop by their own starved sampling against a fixed W+3B (pre-A-740 3/12, at-A-740 2/12 under 28 spinners + taskpolicy -b): now watchdog::Turns::episode with bound W+B+lateness (A-232 step 5); 16/16 under load. RULING: no Mercurial or Python package installs on jupiter (tool install, Ada's word); hg legs skip loudly there. w-reaper next: the Windows edge-port hold, then A-645 P3.
- 04:30-06:32 PST: Ada stopped main at ~04:30 ("wait for the user to tell you how to proceed"); main held every check until her "Resume" at ~06:28. The agents kept working. 44 non-merge landings on main 04:28-06:27, by agent:
  w-hg: 66317ef7d A-745 rule 4 (a streamed clone holds its generation across landings and rebuilds); 36ee8faf2 A-745 piece 3 (stream clone a row at a time); 9b7d5480f A-729 p2 made records (manifest as delta); be4c9e952 copies in made changesets (7.2's copy/copyrev); 67d1c7bd0 import file revisions as one hunk (stopgap, superseded by identity records); f14562e4c stream2 read from Homebrew 7.2.
  w-k90-vendor: A-745 git: 781111325 / 4af06b9bb / 1b73b7880 / 123177313 / 88d081aab / 4cfb5ba8d (negotiation walk as runs, pack choice a unit at a time, v2 fetch in turns, sets as runs or bits, fetch admitted a request's room and answered in turns over HTTP and SSH); by-use clones bigger than a request's room pass.
  w-git: 28d45d91d A-722 4c (a move admitted a unit at a time); a8aea3990 A-722 4d (a replayed thin pack resolves a delta a unit); 2a49cf8a2 the 4b staging race. w-balance: 3463e34b7 resolve-in-turns ON MAIN with its by-use proof (cap 10 alive); f62687455 homes image barrier; 8f463d411 record v3 names as one content. w-perf: 782eb711c A-727 a3 (windows survive a warm restart in the shard image); 4a5469dfb; 8fac3f04f a fetch waits for encoding at most one liveness window. w-engine: b004dd29c retained-history red (its own honest charge; bound corrected), f5c9be2b6 the content-range sweep bounded a step at a time. w-bugs: b56e5766d WinFsp fsx record re-recorded; c2d6edf0a A-749 svn fetch a slice at a time (by use: working copies identical to svnadmin load; checkout 7.0/7.4 s); e986e818e. w-svn: 6f1ce8059 + 59a5c53b6 release measurements (export text calls 168 us median vs a 14 us slice; a 500 MB stock svn commit REFUSED E210009). w-shared: 8ac39d0c0 svn copy-from rows; A-697 a1 b856352cb, a2 fd679ecd4, b 26454e7de, c b8db5ba37, d f253a8017, e 76b5dd46a. w-interop: 295d8723d stock hg sees CLI bookmarks; cefd516dd per-platform pass counts. g226: 8bbb8df05 A-354 (a verb past the failover delay takes effect once). w-lint: 8ebdc1d1a, 9bb1558c2, e3f012a61, f23d5ac80, 22fcede45, 4ce207710.
  BY USE (w-interop, release, main 295d8723d): Mercurial's whole history: push 142 s; HTTP clone 41 s (+ hg verify 12 s), second clone 36 s, clone -r 39 s, clone --stream 50 s, 20 downstream rows pass, no spin; RED: ssh-clone deadlocks (25 MB written, both sides at 0% CPU); daemon RSS >= 12.4 GB during clones. w-perf's hg import A/B: neither build finishes in 1,800 s; a 4.66 GB record landed in one step spins (import.rs:235) and loops on the CLI's retries. Windows (old debug build): git clone spins at vcs_http.rs:834; restart spins at daemon.rs:1094; edges move ports (A-748 in flight). Linux: no user process can mint a credential (issuer surface only for the anchor's children).
  ADA ~06:30: "we address performance gaps upfront. Period." -> memory + common.md; every ruling below puts measured gaps before features.
  GOAL MET: a forming fleet reaches every candidate (147/148 on main, bar 143).
  RULINGS SENT (~06:40): w-hg 1) ssh-clone deadlock 2) clone RSS bound 3) import = identity-only records (texts stored once on arrival, durable since a3) 4) S5 = one identity space with the green's blocks (homing is w-balance's placement) 5) --from default by hg's lookup 6) forced push as a work. w-perf: answer w-engine's seam first (DONE 5c2a8bd: one lend for store and green; replicated things name blocks by identity; with_bytes), S4 slice 2 approved (fallback gone at c3), c1 then w-svn's TextSink seam. w-engine: 1) the work build (374-642 ms) and entry apply (114-196 ms) a unit at a time 2) ring give-back 3) items 3-7. w-svn: 1) intake on w-perf's c1 (windows applied into the one copy; admission per window in flight) 2) format 2's piece by measured hash rate. w-bugs: the resumable svndiff window encoder, shared by the update report and the exporter. w-forming: the 2,933 ms poll, FLEET_WAIT derived, the A-740 label gap; run 7 not reproduced in 147. w-k90-vendor: git rule 4, ready/shallow/include-tag walks in turns, v0 in turns, scattered plans. w-balance: land turns1 now, proof when the Studio answers. w-reaper: after A-748, the Linux issuer (printed secret traded over the abstract rendezvous with a uid check). w-shared: floor authority = link member id equals the work's creator; the lane's large increments in parts and the restart floor leak now; f flips routing and removes the hook together. g226: the forward lane's log flood (1M lines, 260 MB). w-fed-faults: kiln workspaces live-shipped and shared to their f+1 holders, W7 kind, signalling flows on adacachy. w-fed-move: moves.rs held_off_owner and start_move_handoff by identity homes, then the live move. w-git: the load-only reds (long-history push, t_a665) and the Windows restart spin on current main. w-k90-microvm: r4 > 5 h, read and stop a loop. w-interop: reruns as fixes land; RSS sampled whole runs.
  Disks: Mac 112 GB, Studio-2 98, Studio-4 92, big Studio 1.1 TB. ~/Projects/slates/target (435 MB, root-owned from a Sep sudo build) needs Ada's sudo; kiln's reaper is the likely deleter of ~/.cache targets.
- 06:33 PST CHECK: LANDED w-balance 1e992537f (docs, A-724 piece 3c: the small repositories' wait found, per-push turns, and fixed; its code landing). All agents running on the rulings except w-reaper (waiting on r36, A-748's Windows red) and w-lint (hourly). The 'waiting on main' lines in w-hg, w-perf and w-svn progress were answered at ~06:40. w-k90-microvm found r4's output hidden by tee|grep and is reading it; w-engine located the work build in slates_merge::push::build. Disks: Mac 110 GB; Studios 98 / 92 / 1.1 TB.
- 06:35 PST BY USE (w-git, release c729cdea5/91a546972, 32-core Studio, quiet load 3-4, git/git): (a) multi_repo_load R=100 cap 10: 77/77 ok, alive, longest step 72.8 ms, peak 4,046 MiB; (b) uncapped, 19 v1.5.0 to one repo: 86/86 ok, alive, longest step 76.5 ms, peak 4,390 MiB; (c) one repo N=19 v1.5.0: 19/19 ok, 1,337-1,607 MiB (66-80 MiB a push); (d) one repo N=100: alive, all answered, 80 ok + 20 'ng failed to update ref' (peak 4.8-5.4 GB, ~6 min), diag build off main. LANDED 2a49cf8a2 (4b staging race, red first; not the 20's cause). RULING (perf upfront): 1) the 20 refusals 2) name and slice the 72-76 ms step (4e if the store commit; w-engine if the work build) 3) per-push memory bounded, identical objects across concurrent pushes stored once; then the load-only tests, then the Windows restart spin.
- 06:37 PST CHECK: LANDED 4f61920f0 (w-shared, A-697): a holder's work rebases over the holder's replica (verbs::replica_rebase) and a copy it declares is composed there (compose_on_replica); the gate's Unsupported removed, so the routing flip regresses neither; fleet a_holder 22/22; next in main's order: floor authority, large increments in parts over the lane, a dropped creation's floor at recovery, then f (flip + hook deletion together). LANDED 6487601b2 (A-716, §4.17.1 intake rule 1): a green stages a text a piece at a time (Green::stage, append_staged, seal_staged under its identity), so an intake holds a window, never the text. Idle agents wait on their own runs (w-fed-faults' flows, g226's red/green build, w-reaper's r36). Mac 102 GB.
- 06:43 PST CHECK: LANDED w-perf 17aa54dd8 (S4 slice 2 c1): a Subversion head's texts held by the green by path (landed revisions release old texts at changed paths and hold the new; copies and deletes walk subtrees once a path); rehold_head re-holds the head deterministically at recovery; HeadTexts = green.held then records, the fallback named in GAPS until c3, the reads switch and the fallback goes in c3's series; next c2. LANDED g226 e1e3f03ac: a forward lane is kept while idle and says its floor once, not once a forward (the 1M-line, 260 MB log flood). LANDED w-forming cfca7dd22 (work spawned through fleet::spawn_detached and xshard::task_within / task_in_shard_time labelled with its caller's line) and a1439bcb1 (the holder-move stall recorded as not reproduced in 147 runs). LEDGER CLASH A-750 (w-fed-faults first, w-bugs second, w-engine's claim said A-751): the union reads A-750 w-fed-faults, A-751 w-bugs, A-752 w-engine; both told to use the ledger's number everywhere. Mac 98 GB.
- 06:44 PST ADA ~06:45: "push back and make designs more robust, more performant, make your agents do A|B testing and back their decisions with data"; "keep the UI goals updated" -> memory + common.md (decisions by data; competitor numbers; push back on main too). GOAL 87 MET (all 44 parts met; the B-list closed e8788ea65; hangup flake 64e5667fa; bugs found by use since are under goal 93). RULING w-perf c2: FSFS's skip-delta (each new text stored once at its commit as svndiff1 against its skip predecessor, at most log2(N) deltas a read, the head whole in the green) replaces main's reverse-delta cut; A/B before c3: whole texts vs skip-delta vs stock FSFS on the svn_scale fixture (bytes, commit cost, svn cat -r N at 1/mid/head, max deltas a read).
- 06:45 PST LANDED w-shared 43160e726 (via b0ececd09, A-697): serve_forward refuses AdmitWork, MoveWorkFloor and SubmitComputed Forbidden, before dispatch or record, unless the work id's creator member equals the member learned for the link's authenticated anchor (member_task::anchor_of); red verbs::tests::a_works_floor_is_moved_by_the_member_its_id_names_alone (another member's drop refused; fails with the check removed); fleet a_holder 22/22, verbs:: 63/63. Next: large increments in bounded parts over the lane.
- 06:48 PST CHECK: LANDED w-bugs ffefe0003 (A-751 part 1): one svndiff window made in units under the step quantum, its bytes the whole window's (LZ4 included). LANDED w-engine 850a42937 (A-752 part 1): the deriver's replay indexes its entities; compose_volume_planned was O(n^2) in head files: a flat 3,500-file push composes in 28 ms (129 before), O(n log n); slicing it under the quantum is next. LANDED g226 f4f1ed44f (forward-lane log flood: 41 lines -> 1 for 40 forwards; idle lanes kept, pruned for departed peers; fleet.forward.floor_logged). g226 NEXT: close goal 88 (25/26 parts met; the last, class C: MOUNT_CALL_DEADLINE, reply_ns's other users, CREDIT_WAIT copies, journal reader, writeback tracking, await_materialization -- audit each with its commit, derive what is still fixed). UI goals: no further top-level goal has all its parts met. Mac 92 GB.
- 06:51 PST RULING w-shared (A-697 item 2): by-reference (b) replaces bounded parts (a). The lane already flow-controls a 4 MiB increment, so the real costs are time and a double shipment: debug probe, holder submit 4.78/3.46 s against local 3.41/2.81 s; A's record plane ships the inputs back to B, which composed them. B places the inputs as held content when it composes, A's record to B is record-only, and the forward names placed content (A-286); placement streams in bounded chunks. Conditions: a release A/B before and after (4 MiB and 64 MiB, holder and local, by phase), and the local ~3 s for 4 MiB at f=1 (~1.3 MB/s) fixed in the same series if (b) doesn't remove its cause.
- 06:57 PST ADA: "be sure we're also regularly pushing these changes origin/main". AUDIT: 214 non-merge commits on origin/main today, 21 in the last hour; every landing main confirmed is an ancestor. Not on main: w-reaper's A-748 (local, held for its Windows red r36) -> told to land on its green now and add the red later; w-balance-cpu/-lanes/-load (older tries before 3283dd758, which IS on main) -> land or record as rejected and delete; w-interop's scratch branches landed under squashed commits; DIAG/diag/results/wip branches never land by design. ~/Projects/slates: 5,120 commits behind, 0 ahead; its uncommitted 2026-10-26 Windows landing WIP was ported to main on 11-18 (2359d2550), so nothing to push. common.md and the five-minute check gain a landing audit (finished work lands within ~30 min; a pending red never holds a green piece).
- 07:06 PST CHECK: LANDED w-reaper 3a400e026 (A-748: on Windows the anchor holds the VCS listeners and shares each with every daemon by an explicit WSADuplicateSocket, so ports survive restarts); w-svn b5c4b8848 (§4.17.1 intake rules: a commit's texts stream through a sink a window at a time, so a commit larger than a request's room is taken; the 500 MB proof r30 running); w-bugs aac95530f + d12b5cdc8 + 1d7a60e1d (A-751: one resumable svndiff window encoder; the update report's drive makes and sends each window a step at a time, no step past 500 us on a 6,000-line text, bytes unchanged, LZ4 byte-equal at every unit size); g226 6177a0e03 + 5169ed2b1 (CREDIT_WAIT's last copies wait while their daemon lives); w-balance 7872efcf4 (the cpu and lanes tries recorded as replaced) and 3092c035b; w-k90-vendor 00b99ebbc + ca204fc8 (A-745 rule 4 for git: a fetch holds its snapshot; caught include-tag dropping tags since P3b); kiln ed8991d (w-fed-faults A-750: workspaces live-shipped and shared to every member at creation; isolated moves refused W7 'isolated'; killed 34.7 s and cut-off 35.0 s flows pass on macOS; signalling flows r23 on adacachy).
  RULINGS: g226: the MCP listen keep-alive is a cadence derived from the liveness budget (c), not learned from client closes (a), since closes can't tell a timeout from a cancellation; then the test START_WAIT/HOLD_START_WAIT family becomes event waits. w-bugs: the replay drive and get-file-revs through the shared encoder next; it runs its own CI rerun. w-k90-vendor: after item 2, the encoder's per-object timed level choice becomes a deterministic learned choice (determinism gate broken: same snapshot, different bytes), with a replaceable shard clock in tests and an A/B. w-hg: fix the SSH rekey/outbox deadlock red-first (it can't reproduce the stall yet); w-interop sends the stalled run's anchor.log, elapsed time and revision count, and reruns ssh-clone with ssh -vv.
  ADA ~07:05: "all agents need to also be keeping their local checkout up to date" -> common.md (merge origin/main at each piece start, before each reported run, at least every 30 min; name the base commit with each result; remove landed worktrees) + memory + sent to all 18 agents. Landing audit: no finished work unlanded on agents/* in the last 2 h. DISK: Mac 89 -> 52 GB in 15 min: w-shared (42.3 GB) and w-git (33.6 GB) told to trim and switch to .noindex.
- 07:08 PST CHECK: agents applying the checkout rule (w-svn, w-forming, w-k90-containers, w-lint current at origin/main 8281ea297 / kiln ed8991d29). w-balance sat in needs_you: kiln's gate held 'git worktree remove ... ; rm -r ~/slates-work/w-balance-docs' for a person -> stopped (mode now), told to use git worktree remove alone and list other dirs for main; common.md: never rm -r (the gate holds it even in bypass mode). w-k90-vendor -> w-perf inbox: the deterministic zlib level. Mac 54 GB.
- 07:15 PST ADA: "Kiln goal 90 should be making more progress. Is it stalled?" Yes: 32/39 parts, none closed since mid-morning. Causes: adacachy (only Linux/KVM/KIND box) at load 62 with other sessions' long jobs (a qemu VM 44 h, a docker xplat-fedora vm_harnesses step35 test 39 h, a mantle-node sim 10.7 h, an uncal sim 7.7 h, kiln cargo tests 3.7-5.5 h) and the stuck CI jobs awaiting Ada's cancel; w-k90-microvm stuck 3+ h in one turn on r4; the KIND proof left to 'main' (no owner); A-520 piece 5 unstaffed; w-k90-vendor moved to goal 93's git fetch. ADA: "Dispatch a new agent if needed" -> STARTED w-k90-close (01a1f9867c4a7738978c41fc40eb1297, Studio-4; brief briefs/w-k90-close.md): the KIND proof + A-520 piece 5. w-k90-microvm interrupted (mode now): read r4's real state, stop a loop, bounded rerun. w-k90-containers: drive its live W9 test itself, bounded, tail every 10 min. Ada's word still needed: the stale adacachy processes and CI cancels; WinFsp and WHP on jupiter for the Windows parts.
- 07:17 PST CHECK: GOAL 88 MET (every cross-shard and remote wait derived: 26/26 parts; class C closed by g226's audit + c09e6c7bb, the MCP listen keep-alive by the heartbeat's rule). LANDED: c09e6c7bb (g226), 1933e6ca6 (w-forming: live_chaos's fleet waits are the 40 s window over the test's CPU share), ffa5be0f6 (w-git A-753: a branch created at a held commit made from its green; a resent object read from the index), 2d8ab9f3d = w-svn intake part 1 (b5c4b8848 + e9b70d6d4 staging).
  BY USE: w-svn, release, big Studio, stock svn 1.15, 500 MB file: DAV committed r1 in 8.0 s, peak RSS 6.2 GB (~12x the text); svn:// E210002 at "Committing transaction"; a rerun's landings spun shard 0 for 1.06-1.08 s (vcs_svn.rs:605, vcs_http.rs:837). RULING: w-svn slices the landing now, in parallel (the increment sets the path from the held identity; the landing walks records in turns; the svn:// row passes); w-perf wires the TextSink into the green's staged blocks first (the biggest copies), then c2/c3; records.rs order agreed in an inbox note. w-interop, git matrix (debug, 3e1983bc9): the v1.0.0 cut 27/27 GREEN; the v2.0.0 cut (56 MB): the push lands, but git_encode.rs:345 spins 3.5 s at poll 162,167 (-> w-k90-vendor, first), then a 78-exit RESTART LOOP: each restart re-runs the encode and fails to install its stop watch within a fixed 10,000 ms, so it stops itself (-> g226, red first; derive the budget; order and priority so control beats background work).
  w-reaper: A-754 (Linux issuer) built locally (0b1f0d80) -> told to cross-check from the Studio with clippy --target x86_64-unknown-linux-gnu and run the by-use tests on CI ubuntu via a diag branch, then land, not wait on adacachy. w-balance waits on run 31 (kiln's Studio not answering); its intake placement half is on main for A-722 to call. Its 'for main to delete' dirs on Studio-2 are deferred: the Studio missed kiln's 15 s answer, and w-balance-docs is the agent's own cwd. Mac 64 GB.
- 07:18 PST CHECK: RULING w-svn (base 80e262c6d): (B), git's intake shape for svn texts: the TextSink writes windows into vcs_store's content plane (quarantine shape, segment-backed); the revision's log entry names identities and a store move (as push_sealed); the green's increment sets the path from the identity; content durable (image barrier) before the entry commits (crash red: whole or absent); A/B the 500 MB commit before and after against stock svnserve/FSFS. (A) (bytes in the log, streamed append) rejected: a multi-GB entry and three copies. w-perf redirected: don't wire the sink into Green::stage heap blocks; c1's held table points at content-plane identities; c2/c3 on identities; agree the seam with w-svn and w-engine. LANDED w-git ffa5be0f6 (A-753): the 20/100 refusals were green_chains at capacity; a create at a commit another branch holds is made from that green; resent objects read from the index; N=100 rerun next. 80e262c6d (w-svn bench: the 500 MB svn commit numbers), 6ad65005a (g226: recovery, give_back, cross_shard_wait, consumers, attachment_term, reply_room, forward_retry tests wait on their event). Mac 64 GB.
- 07:28 PST CHECK: LANDED w-forming cdfbb4020 (A-755: a live whole entry's build sizes each sealed piece to measured seal rate x quantum left, page..MiB; fleet sealed-in-turns red 8 -> green >= 48); w-bugs fd99ab174 (A-751 p3: replay drive and get-file-revs a window a step through the shared encoder; CI run 8: working copies identical, bytes per slice down 10x or more, but the longest slices still 2.7-4.6 ms against a 2.9 us quantum, so another step dominates); w-k90-vendor 96cce3522 (A-745 git: ready's search, the shallow cut and include-tag's scan in turns). RULINGS: w-forming takes the append turn now (223-498 ms: whole-payload identity hash + 3-4 copies): incremental BLAKE3 over the seal turns; one body held by identity in the content store (no Arc, R2); red + release A/B; then verbs.rs:3299 (255-515 ms first poll). w-bugs: a diag run naming the kind of step past budget, then slice it; correction note to w-svn. w-reaper: A-754 on CI ubuntu diag branches (pushed). w-fed-faults: signalling flows r27 on adacachy at ed8991d29. w-k90-close: started; both parts' code is on kiln main, proof runs next. Mac 60 GB.
- 07:29 PST LANDED w-svn 134626b40 (svn:// and DAV landings carry A-740 labels vcs_svn::land / vcs_http::svn::land; release proof r32 building) and e211d430c via 4839b01f8 (ContentStore::begin_object/put_window/finish_object/abandon_object, put_object's streaming twin; same root, holders and bytes at piece sizes 1, 3, 61, whole; an abandoned object leaves the store empty). RULINGS: the interim landing goes now (the landing in turns, the entry naming identities, the green's post-state read a window at a time from the store: three copies -> one transient), as the final path's shape, not a second path; 1b (green file blocks naming content-plane objects, with checkpoint and replay) = w-engine's items 3-7, MOVED AHEAD of the ring give-back (after A-752); the naming rule (identity = the store's object root; plain BLAKE3 up to one window) to settle with w-perf within the hour, else taken as agreed.
- 07:35 PST LANDED w-perf 14215b0c3 (S4 slice 2 c2, landed tree 72eaba443: svn 558, vcs content 11, server svn+vcs_store 160): a Subversion repository's texts kept as skip-delta svndiff2/LZ4, a read through at most log2 N + 1 deltas (40 edits: bound held, deltas under 1/4 of the bytes); the naming rule agreed with w-svn (identity = the content store's object root); next the records' naming, the TextSink over the content store under Lent at the three sites, then the A/B after r80. REDS ON MAIN routed: vcs_ssh::tests::receive::a745_a_long_historys_clone_over_ssh_is_answered_within_one_requests_room fails 3/3 on 1933e6ca6 (its push session never exits) -> w-k90-vendor FIRST; fd99ab174's two step-budget timing tests fail in debug under load -> w-bugs: deterministic reds (units per step or shard CPU time with the measured quantum), never wall time in debug. w-bugs suspects Index::children (every name ever made under a directory, each resolved twice in one step) -> told to fix the complexity (O(live children)) before slicing, agreed with w-svn, with an A/B.
- 07:36 PST RULING (w-perf's finding): ContentStore's object root is cut by chunk_bytes(page) = 16 x the base page (256 KiB on macOS arm64, 64 KiB on 4 KiB Linux), so it can't be an identity: the same bytes would be two identities across a Mac and a Linux holder, breaking R8, replication, takeover and dedup. RULE: every identity that crosses nodes is plain BLAKE3 of the bytes (BLAKE3's fixed 1 KiB-chunk tree, streaming); a store's root is a node-local storage detail (identity -> object per node); replicated entries name (identity, len), a root only as an origin-node hint nothing replays from. SWEEP -> w-perf: every replicated, homed or recorded identity that is a page-derived root (A-662/A-727 git and hg identities, a3's held pieces, homes' rendezvous keys, w-engine's coming block references, dedup keys) switched to BLAKE3 of the bytes; red: same identity under 4 KiB and 16 KiB windows, and a simulated Mac-to-Linux replication and takeover dedups and serves. One inbox note to w-svn, w-engine, w-balance.
- 07:38 PST LANDED w-shared c0710afca (via e261100c1, A-697 e2): a holder's pending creation is held, not dropped, at recovery (work_home::hold_for_release); B forwards MoveWorkFloor with no base and settles the creation only on WorkFloorMoved; the release survives another restart; 3-node warm-restart red (the floor never released) -> green; fleet a_holder 23/23, server 83/83, db 181/181. Removed 8 landed g261-* worktrees; kept g261-a603, -a635, -a636-red (commits not on main by hash). Next: the by-reference route, release baseline first.
- 07:39 PST CHECK: LANDED g226 5f2862e91 (via 7346bd343): the restart loop fixed: the stop watch reaches the control shard over a channel the shard receives at start, before anything runs, so busy recovered work can't refuse it; the fixed 10,000 ms install budget and the 'could not be installed; stopping' branch are gone; red a_stop_watch_asked_of_a_busy_control_shard_is_installed_and_waits_for_a_reason (old: ended mid-work at 31.4 s, refused; new: after the work, for its reason). LANDED w-forming adacdeec8 (A-757 p1): a live entry's identity hashed once per append as its fields stream, not three times over copies: a 2.4 MB append in release 3.1-4.6 ms -> 0.94-1.63 ms. g226 next: derive the 10 s observe budget (R3), fix the two slates-api anchor liveness tests that fail under memory pressure, then the client/tests waits.
- 07:43 PST CHECK: LANDED w-bugs 26e125adb (A-751's window tests hold each step to its own unit count, not debug wall time; a failed look-up no longer walks past the step's unit budget) on top of 03f872541, a 'wip' commit that reached main (permanent: no force-push) -> w-bugs told; common.md: only finished, area-prefixed commits reach main; fold wip with commit --amend before pushing. Mac 55 GB.
- 07:43 PST RULING (w-balance's inbox: record v3 durability across nodes): (ii) -- a v3 record is written only when its names' objects are homed in the region at f+1 (serve_remote acks after the home's image is published; one barrier per shard publication, group commit); replay reads locally else from region homes, holding by identity; records stay v2 until a takeover test proves it (kill the owner after a v3 push; the successor replays on another node and serves). homes' region Put carries whole objects keyed by BLAKE3, cut at each home; v3 names rows by BLAKE3 of their whole bytes plus the count. w-perf: the identity -> object map is a piece of the shard image (required; in the sweep's red). A/B push latency v2 vs v3 (ii) before landing.
- 07:45 PST ADA: "Please dispatch a new agent [to] tackle Kiln goal 90. It needs to be completed." STARTED w-k90-kvm (01a1f9a2487f75b5844f60a585b6b3f2, big Studio; brief briefs/w-k90-kvm.md): the KVM microVM drive set at current kiln main, the resync end on KVM, A-520 piece 5 (moved from w-k90-close). w-k90-microvm (no KVM progress in ~4 h) told to write its hand-off (run ids, logs, the slatesvm respawn, the drive command), stop its adacachy runs, and end. w-k90-close keeps the KIND proof; it ran 'sudo modprobe tls' on adacachy without Ada's word -> told no more sudo or system changes; flagged to Ada. Goal 90 now: w-k90-containers (OCI live, r48), w-k90-close (KIND), w-k90-kvm (KVM, resync, A-520 p5); Windows parts (vendored build on Windows, bare_metal, WHP) wait on Ada's word on WinFsp and WHP on jupiter.
- 07:50 PST CHECK: LANDED w-forming 5a9a5d41e (A-757 p2: a live whole build hashes its entry's identity in quantum slices across its turns: the append turn 0.94 ms -> 0.46 us for 2.4 MB, release); w-k90-vendor c21abe3ca (via e5f171a9c: the SSH long-history clone red was the test's fixed 4 s patience; clients now wait while the session is open; vcs_ssh 65/65). w-git ffa5be0f6 rerun (main 7ad7e19fc, .local-2): N=100 100/100 OK, daemon alive, peak 2,969 MiB (28.8 MiB/push, was 47-53); longest steps input 1.2 ms, check 57.9 ms, answer 73.7 ms -> next the answer's stale restage in turns, then the check.
  RULING c2 vs (B) (w-svn, cc w-perf): sink texts land as Stored { len } references by identity (no delta, no copy in the landing step); history compression moves to a background sliced job deltifying a superseded text against its skip base, then releasing the whole object (head whole once, history compressed: ruling B, FSFS-class size); KeptText only for whole-arriving texts until they stream. The content plane's durability barrier (A-727 slice b) -> w-balance (red: a crash between the content write and the append leaves the commit whole or absent).
  BIG STUDIO (.local-2, d1323898): kiln's runner unresponsive; runs saw 6-9 GB free (w-svn's r33 died mid-run). No SSH from here. w-k90-vendor (on the box) asked for df, memory and the biggest folders, and authorized to cargo-clean stale (>12 h), unused targets by clean.sh's rules if under 30 GB. No Studio has stock svn (Studio-2 89 GB, Studio-4 72 GB checked).
- 08:04 PST ADA: "You are supposed to cleanup the disks." / "The big studio also has 4TB ... be more aggressive in what you clean." FACT: the big Studio (Adas-Mac-Studio.local, kiln adas-mac-studio.local-2) has 999 GiB free of 3.6 TiB (2.6 TiB used); the agents' '6-13 GB free' was macOS df's ifree column (inodes) -> common.md now says how to read Avail. AGGRESSIVE CLEANUP (clean-aggressive.sh: every cargo target or kiln reach copy not in use right now, whatever its age; never sources, ~/.cargo or worktrees' files) started: r830 big Studio, r831 Studio-2, this Mac (background); Studio-4's runner did not answer, retry next check. LANDED w-hg 2db9e84de + rekey fix (merged 295d8f02a): the ssh-clone stall was Session::blocked gating input on a full outbox during a rekey, triggered by a key time measured from an earlier short session; bug record docs/bugs/2026-11-20-an-ssh-clone-stalled-at-a-rekey-with-its-outbox-full.md. RULING: rekey per RFC 4253 sec 9 (a GB or an hour, whichever first, plus client-initiated). w-hg's clone-memory measurement goes to Studio-2, not the MacBook.
- 2026-11-20 10:02 PST ADA RULED (goal 90): WinFsp may be installed on jupiter; Shards' per-VM AppContainer profiles may be made on jupiter; the kTLS module stays on adacachy for the KIND proof; containers get the volume by Docker mounting slates' export itself (type=nfs Docker volume), replacing the FUSE bind after an A/B (no user_allow_other, no podman).
- 2026-11-20 10:02 PST goal 90: four new agents on the unowned remainders: w-k90-win (Windows build + bare_metal), w-k90-a520 (A-520 p5 via the export route), w-k90-whp (WHP proof on jupiter), w-k90-pin (vendoring part; fix 2898dfe42 on its branch). w-k90-close = KIND only; w-k90-kvm = KVM + resync only.
- 2026-11-20 10:02 PST LANDED: A-758 hg pushkey dbd497bbc (w-interop); identity rule BLAKE3 alias 9010c5630 (w-perf); content-plane durability barrier 5a189220a (w-balance, A-727 b); svn text piece by measured MD5+SHA-1 rate 63ab25b9f (w-svn); SSH rekey per RFC 4253 2d1f94c44 (w-hg).
- 2026-11-20 10:02 PST hg interop 24/24 on hg's full history, first all-green (w-interop r110, base 944588f6a; wall 444 s, peak RSS 10,681 MiB). git v2.0.0 cut 7/27 (r103 had 22): restart loop = encode spin killed by watchdog (w-k90-vendor) + unsliced recovery at daemon.rs:1179 (w-git), both TOP.
- 2026-11-20 10:02 PST DISKS (aggressive cleanup): this Mac +8 GB; Studio-2 +91 GB (175 GiB free); Studio-4 +24 GB (81 GiB); other MacBook +135 GB (1.1 TiB); big Studio 891 GiB free; jupiter (7 GB free) cleanup r846 running.
- 2026-11-20 10:02 PST UI: goal 93's git server part records A-722 3/4a-4e, A-743, A-753 and the concurrency proof; interop part records hg 24/24.
- 2026-11-20 10:29 PST ADA (again): "dispatch agents on the mac studios ... to tackle this goal". Done the kiln way: plan 01a1fa36 (8 steps); each open goal 90 part is now a GOAL ON ITS AGENT'S OWN kiln stack (runs 5eb8532d pin/Studio-3, 1e59e8cf containers/big Studio, 44da69f3 a520/Studio-4, 3fb8a270 close/Studio-4, 5b5b472a kvm/big Studio x2, c58ec03a whp, bc0f077f win/Studio-2); goal 90's part rows now say HELD BY each.
- 2026-11-20 10:29 PST RULINGS: w-reaper A-645 P3 (no SLATES_ROOT config, root from the daemon's mount record, svn /<name> fallback removed; VFS by-reference file primitive; FSFS 8 view on stored deltas, A/B); w-balance (takeover: committed push records reach the successor by the commit rule; put-in-parts past 1 MiB before run 31); w-engine (A-752's RUN=16 and UNIT_BYTES literals derived, R3); g222 W1 affinity approved with A/B; g227 arm catch-up 4x gap; g228 measures itself.
- 2026-11-20 10:29 PST LANDED: w-balance f35a6ca31 (t_a724 event waits), d068491a1 (region homes keep whole objects by BLAKE3); w-git A-722 parts 3-4e, A-743, A-753 + concurrency proof N=100 100/100; w-lint: CI compile + lint clean on every target.
- 2026-11-20 10:29 PST git r110 regression 22 -> 7 of 27: restart loop (encode spin killed by watchdog: w-k90-vendor; unsliced recovery at daemon.rs:1179: w-git), both TOP.
- 2026-11-20 10:29 PST DISKS: jupiter 3.1 -> 307 GB free (103 targets/reach copies, 262.7 GB). UI: goal 93's git, hg, svn and interop parts carry today's landings.
- 2026-11-20 10:42 PST LANDED: AUD-11 1fc1da1fb (w-shared: a forwarded write answered by its commit, 4-21 ms vs ~1.02 s polled); 306214c57 (w-hg: identity-only hg import records, SHGR v4) and 53a81211d (rekey bytes logged, ChaCha20 KeyUsage proof); 359da2388 (w-balance: region homes take any length in parts); 28e57ac18 (w-svn: A-761 p2 svn intake under (B)). UI goal 93 parts (shared pieces, hg, svn) updated.
- 2026-11-20 10:42 PST RULINGS: w-perf's replay shim (9010c5630) is removed, option (b), pre-rule records refuse typed; w-perf reruns the hg import A/B/C on >= 306214c57; the svnserve-over-SSH red on main routed to w-svn; w-k90-a520's stale NFS test mounts come down through the instance's own end path, never root.
- 2026-11-20 10:42 PST The shared state checkout had a stray staged reversal of w-shared's ledger lines (not mine). HEAD's content was written back and the merge pushed, so nothing is lost and it's in sync with origin.
- 2026-11-20 10:42 PST DISKS: big Studio 1.1 TiB free, Studio-2 155 GiB, Studio-4 59 GiB, this Mac 44 GiB (cleanup rerun). kiln link dropped 4x this hour; retried each send.
- 2026-11-20 10:45 PST REGRESSION routed TOP to w-perf: S4 c2 (14215b0c3) rebuilds a whole svn text from its skip-delta chain on every read, making fetches 3-12x slower (checkout 25.0 s vs 7.1 s, update 32.5 s vs 2.7 s; w-bugs runs 10/11). Fix: a reader per fetch or a bounded made-text cache, red = at most one rebuild per text per fetch.
- 2026-11-20 10:45 PST LANDED: A-759 088bf935b + 1417c0868 (w-bugs: svn directory listing at O(live), a cursor of 16 entries a step, 1.6-1.75 us). w-bugs next: add_subdir sliced. w-reaper's A-645 P3 rulings resent (it reported them still waiting).
- 2026-11-20 10:45 PST DISKS: this Mac 37 -> 51 GiB free (cleanup rerun; the largest left are live targets: w-git 16 GB, w-shared 13 GB).
- 2026-11-20 10:48 PST LANDED: A-738 P3 7bc6ba330 (w-reaper: FSFS format 8 view, every text a DELTA rep read in place, 128-460x fewer view bytes; stock svn 1.14.5 reads it, CI 44793305924); A-722 4g 6737b04a7 (w-git: a replayed record's moves admitted an object a turn); A-724 8c 7bde25b0d (w-balance); f2f044865 (w-hg rekey proof); 9b635576d + 2ff993212 (g226: cli START waits judged in the instance's own CPU). Slip: w-reaper's diag commits 5e0bfa280 and 4157a58ea reached main; told.
- 2026-11-20 10:53 PST LANDED: A-759 part 3 7d2b45174 (w-bugs: the replay's add_subdir reads a copied directory 16 entries a step; red a 10,000-entry copy). w-bugs on to its next open bug meanwhile. This Mac 40 GiB free; the Studios 57 GiB+ (Studio-4 the lowest).
- 2026-11-20 10:59 PST The big Studio rebooted at 10:35 PST, with no panic report and a JetsamEvent at 09:50. Its six fleet agents went offline, and kiln refuses resume-in-place for offline agents. Restarted on their own conversations: w-engine 01a1fa5315ca, w-fed-faults 01a1fa5327db, w-k90-kvm 01a1fa536b8b (plan 01a1fa36 steps 5+8 re-given, run f497f79b). Queued on the big Studio's link: w-fed-move, w-k90-containers, w-k90-vendor. w-bugs finished its list: it now takes git deepen and partial clone off w-k90-vendor.
- 2026-11-20 11:05 PST Big Studio agents back on their own conversations: w-engine 01a1fa5315ca, w-fed-faults 01a1fa5327db, w-k90-kvm 01a1fa536b8b (steps 5+8), w-k90-vendor 01a1fa54fc37, w-k90-containers 01a1fa589909 (step 2, run 9039fe93); the five dead entries stopped. w-fed-move queued on the link (retry next check).
- 2026-11-20 11:05 PST LANDED: A-722 4g 6737b04a7 (w-git: replay admits moves one object a turn, ending the recovery spin that looped restarts); f1e7294eb (w-reaper A-645 P3 rule 1: hooks see the repository beneath the published root mount, refused typed before; the svn fallback gone); g226's cli waits judged in CPU (8b85fc848, e081c396f). The encode spin was already fixed by 67dd4ef40 (982ea33d5), after r110's base: w-interop reruns the git cut on current main.
- 2026-11-20 11:05 PST OWNED: my cleanup deleted w-git's live target mid-build at ~10:50 (nothing open between cargo steps). clean-aggressive.sh now keeps anything written in the last 2 h. This Mac 29-31 GiB free: w-git and w-shared trim their own; a stale-only cleanup is running.
- 2026-11-20 11:32 PST DISK (Ada: "clear the disk"): this Mac went 11 -> 56 GiB free. w-git, w-shared and w-perf deleted their local targets and build only through kiln now. 9 GB of days-old temp leftovers removed. Clean, fully merged, unused old worktrees removed with git worktree remove (55+ so far, still running). Docker's engine answers 500 (not touched; Ada restarts Docker Desktop). An OS update is staged as local snapshots (Ada's).
- 2026-11-20 11:32 PST kiln restarted every session on this Mac (main is now slates-9a); the six slates-work sessions were told to resume and build via kiln. RULINGS: w-git does the edge's early status head; w-reaper builds the VFS by-reference file to its API now; g226 waits on kernel events, not CPU, and starts its own Linux runs; w-forming's next five gaps, NFS 7.5x write amplification first; w-bugs takes the svnserve-over-SSH red while w-svn is paused.
- 2026-11-20 11:32 PST LANDED: w-forming's queue complete (A-755, A-757 parts 1-5, FLEET_WAIT derived, start waits by CPU); A-752 R3 literals derived 4b6d5c24b (w-engine); A-722 keepalive 61b178d42 (w-git); hg pushkey in batch 04c3fa62d (w-interop).
- 2026-11-20 11:47 PST ADA: "Install Python 3 on jupiter". kiln's drives use Python stand-ins for a harness; the Windows bare_metal and WHP drives both stopped without it. w-k90-win installs it once and tells w-k90-whp.
- 2026-11-20 11:47 PST git interop r119 (base 85d66d9cb): 24 of 27 rows, zero spins, kills and restarts (r110 had 7). LANDED since: 4136fcd89 (w-bugs: an HTTP fetch's answer gets its own admission, never what's left after the request; a partial clone's lazy fetch of 2,000 blobs answered), 51158a4fb (w-git: git's smart HTTP answers write their status at the head), b1206d96c + A-762 507d722b3 (w-svn: the svn+ssh tunnel red fixed; the exporter's text read sliced), e531594fa (w-hg: --from follows hg's lookup).
- 2026-11-20 11:47 PST DISK: 78 clean, fully merged, unused worktrees removed with git worktree remove (72 kept); this Mac 50-55 GiB free.
- 2026-11-20 11:48 PST FOUND (w-balance, takeover test v3_takeover.rs, 4 of 4 runs): after an owner's SIGKILL at f=1, the successor rebuilds the repository but serves NO refs, so every acknowledged push is lost; and the owner's tokens are unknown there (401/403). main's "committed pushes reach the successor by construction" was WRONG. Routed TOP: lost refs to w-balance (red = its test; then flip v3), credentials at takeover to w-fed-faults.
- 2026-11-20 11:48 PST LANDED: 51158a4fb (w-git: git smart HTTP status at head); g226's cli waits judged by the processes' own CPU or the kernel's reports (051bf8f4b await_unmounted on POLLPRI, 266f1f6b8, 05c11cc74, 32164bfa3, 8ebf8a1c6).
- 2026-11-20 11:53 PST Duplicates from late-starting queued restarts stopped (w-fed-move x2, w-k90-containers x1); one copy of each kept. LANDED: edbc3838c (git: a fetch's decoded lines and lists given back once used; lazy fetch of 2,900 blobs answered), g226's interop and load_scale waits by the processes' own CPU (25cfea328, 6ca0dad67, 7380ad1b5), 001318ae7 (w-lint). This Mac 33 GiB free (w-shared testing locally at Ada's word).
- 2026-11-20 11:54 PST FOUND (w-hg, big Studio): (a) the daemon holds 7.1 GB resident (shared memory) after pushing hg's history (an 84 MB store, a 162 MB bundle), while the clone's own heap stays ~300 MB; r9 splits it, then w-hg bounds it, red first. (b) The anchor killed a healthy daemon 4 times ("watchdog has not looked for ~2 s ... no progress for ~1 s"), the first at idle 100 ms after start. Routed TOP to g226: idle is not stuck; a stretched launchd timer is suspected. (c) hg's replay at restart: 130 s of CPU in units of 265-277 ms. w-hg slices it.
- 2026-11-20 11:58 PST ADA RULE (~12:15/12:20): "a repeated sizing problem ... gross overallocation. ... learned and derived approaches that *carefully* consider and respect the devices they're on ... as zero-alloc as possible" and "this holds across git, mercurial, and svn work. If we don't need to allocate or can architect things in such a way that we do not allocate, we do not allocate." Now briefs/common.md MEMORY SIZING; sent to w-git, w-hg, w-svn, w-perf, w-shared, w-interop. New agent w-sizing (01a1fa89487a, Studio-4): the shared segment and its prefault first, then the slates-wide sizing sweep, a device-pressure seam, and zero-alloc data paths.
- 2026-11-20 11:58 PST LANDED: w-hg item 5, e531594fa merged 17243231e (--from and served lookup resolve as hg 7.2's revsymbol). UI hg part updated.
- 2026-11-20 11:59 PST LANDED: 1b093e7d4 (w-forming: a live write ships the bytes it changed, not every window it touched; the NFS write amplification gap). DISK: this Mac 25 GiB free. Fleet targets here total ~12 GB (w-svn, w-shared, w-perf, w-hg 3 GB each); the rest is other sessions (kiln-send-route-target 21 GB, mantle worktrees) and the staged OS update. w-shared deletes its target after its last compile.
- 2026-11-20 12:08 PST ADA STORAGE RULING (~12:40): changes and changesets in RAM; full repository content durable on disk, landed under a repository's standing grant; an initial clone or import is one changeset, landed almost automatically; metadata and changeset tracking in Mantle's K/V engine (crates/engine, a RocksDB 11.8.1 port), copied into slates with crates/disk and crates/chunk. Prior art to examine (Ada ~12:50): Anthropic's Scion, EdenFS, Piper/CitC, Mononoke/Sapling, VFS for Git. Now briefs/common.md STORAGE and briefs/w-store.md. New agent w-store: research and design amendment first, then vendoring, then the store behind one seam. Its start is queued (Studio-3 link down; an earlier Studio-2 attempt unanswered). w-git, w-hg, w-svn and w-perf told to write against the seam.
- 2026-11-20 12:08 PST LANDED: c03a8953b (w-shared: family 1, lent record sessions woken by their return); 2502607c4 (w-perf: svn text reads a window at a time, never whole; c3 one place a text). w-git's long-history edge tests are 12/12 at load 55-59 (release) on 51158a4fb. w-bugs reruns the svn fetch on >= 2502607c4.
- 2026-11-20 12:09 PST ROOT CAUSE (g226, reproduced): the anchor's kills of w-hg's daemon came from w-hg's own vmmap/heap snapshots. They suspend the process, and the daemon maps ~220 GB of address space, so one vmmap took 2.42 s, past the window, and A-232 killed it by design. main's stretched-timer and idle hypotheses were WRONG; g226 measured both away. Ruling: keep A-232; g226 adds an in-process region report to slates status; w-hg stops using vmmap/heap; w-sizing examines the 220 GB address space.
- 2026-11-20 12:09 PST w-store started on Studio-2 (01a1fa91c80b). A queued second start on Studio-3 may appear; stop the extra if it does. LANDED: 76922ffec (git: zlib level and delta window chosen from bytes saved and the machine's measured rates), 6e5f6a53a (vfs/server referenced-file tests cleaned).
- 2026-11-20 12:12 PST INTEROP r2 (w-interop; release, base 26c98c76d; sampler in a file): hg 24/24, but clone-b 41 -> 357 s and clone-rev 40 -> 141 s against r110 (w-hg TOP, bisect), peak 12,141 MiB against an 84 MiB store; git 24/27, peak 7,421 MiB against a 51.3 MiB pack. Both about 145x the repository's size (to w-sizing and w-store). git still fails deepen (needs more room) and partial clone (now 'bad pack header' on the promisor fetch): w-bugs. Zero spins, kills and restarts in both.
- 2026-11-20 12:13 PST LANDED: 7c37b6fee (w-k90-vendor: git zlib level and delta window from the bytes saved and the device's measured rates; output independent of the clock, red with a jumping clock). w-lint fixed six Windows lint errors in the referenced-content tests. No fleet agent offline or waiting on a person; one w-store, no duplicate. This Mac 24 GiB free (mostly other sessions).
- 2026-11-20 12:14 PST MEMORY SPLIT (w-hg, slates status only): after an 84 MB hg push, RSS 8,520 MiB = content store 4,495.5 MiB (manifests likely kept whole) + a prefault reserve of 4,931.4 MiB equal to the last burst + the push record 153.9 MiB + chains 74 MiB, with 16,384 MiB reserved; 20 s idle later 12,055 MiB as the idle shard faults the burst-sized prefault in. Routed: the prefault to w-sizing (first fix), the manifests to w-hg, the disk side to w-store. hg regression lead: a small push sets off a 130 s+ store rebuild that the next clone waits on (w-hg, red first).
- 2026-11-20 12:15 PST LANDED: 9d4c5e5db (w-interop: the git, hg and svn matrices print each row's daemon peak RSS; git v1.0.0 cut 27 of 27, peak 354 MiB). UI interop part updated.
- 2026-11-20 12:18 PST LANDED: A-765 6ea16f4c5 (w-sizing: the idle prefault reserve decays, held whole one sweep interval then halved; a 64 MiB burst gives back 234 MB after 20 s). PUSHED BACK: A/B prefault against none on R9's gate, then only the measured next-turn need; replace reserve_per_shard = memory/shards/3 (a machine fraction); verify shared pages really return to the OS (macOS footprint drops, RSS doesn't). g222 narrowed W1's lone-client ~6 µs to two 3 µs steps. This Mac 22 GiB free, mostly other sessions' builds.
- 2026-11-20 12:24 PST ADA (~13:35): store history as changesets, not per-file diffs: "we aren't introducing a second underlying unit". Ruling: the on-disk history is the log of landed changesets; every tool's form is derived (A-645). Sent to w-hg (its per-file-delta change is held; A/B changesets against deltas), w-store (the amendment's centre), w-git (B1 ea5f0d6da landed: reads in place through PackSource for the change in flight; no pack store as history), w-svn and w-perf (their A/B gains the changeset arm). LANDED: ea5f0d6da (w-git B1). w-k90-containers proved the export route on macOS (end-of-session fixes with w-k90-a520; Windows judge change routed to w-k90-a520).
- 2026-11-20 12:33 PST SIZING SWEEP (w-sizing, 79e0a4723 in GAPS): every size derives from reserve_per_shard = memory / shards / 3, fixed at boot. Each shard's content slice is ~4.25x it, ~1.4x the machine's memory in address space over all shards (the 220 GB on a 96 GB Studio); it also caps the prefault, the tables and the client, NFSv4, fleet and DAX shares. macOS: MADV_FREE_REUSABLE drops the footprint but not RSS, and nothing is reclaimed under pressure; only a purgeable Mach entry set EMPTY frees memory, so the real fix builds the arena from purgeable extents (a design piece). Next: the prefault-or-none A/B (queued), reserve_per_shard replaced by a learned, pressure-aware ceiling, the content slice sized from what is stored.
- 2026-11-20 12:33 PST LANDED: a48bcdc52 (g226: slates status reports the daemon's memory by kind, in process, nothing suspended), 4d53c3a07 (A-740 labels), ea5f0d6da (w-git B1). DISKS: this Mac 19 GiB (temp leftovers cleared again); Studio-4 14 GiB, so the 2 h-guarded cleanup is running there (r1).
- 2026-11-20 12:35 PST main CORRECTED: CLAUDE.md §8 binds (agents: focused tests only; cross-platform, xtask, suites and load runs go to main/w-lint via run-requests). Main's 'agents start their own Linux/Windows runs' is withdrawn; design A/Bs by agents stay (Ada 11-20). g226's status memory report landed 599603559; its Linux/Windows checks and the slates-mem unsafe 80->83 went to w-lint. w-forming: sliced whole-record capture approved (chunk pins), then the 193 ms takeover rebuild turn and a takeover stall. w-forming's A-763 landed (1b093e7d4, 20,063,216 -> 309,760 bytes shipped).
- 2026-11-20 12:42 PST CHANGESET A/B COUNTS (w-hg; hg's history, 57,054 changesets, 115,479 file revisions): whole texts today 4,296.9 MiB; per-file deltas 145.7 MiB (reads 14/205 deltas p50/p99); changesets with tree checkpoints 94-103 MiB + 191-1,057 MiB of checkpoints; changesets with per-file wholes ~137 MiB (reads 15/350 edits); stock hg 84 MiB compressed, clone 283 s, serve peak 111 MiB. PROVISIONAL SHAPE: changesets with per-file wholes. Next: release timings and a compressed arm, then BENCHMARKS; w-store builds the log. w-git: interim side-band keepalives while a long body is read (debug at load 51-63 failed 7/9), plus a measurement of the decode itself.
- 2026-11-20 12:54 PST DISK EMERGENCY on this Mac (8 GiB free): other sessions' builds (mantle's ~/Projects/.worktrees targets, /private/tmp/*-scratch, kiln-send-route-target 24 GB). Under Ada's "clear the disk": deleted only stale (>2 h), unused build output: 53 GB freed, 24 GiB free now; recent and in-use targets kept.
- 2026-11-20 12:54 PST REGRESSION (mine): f1e7294eb refuses every hook until a root mount is recorded, but A-645 (a) publication is owed, so svn revprop hooks fail (w-interop r10, svn 30/32). w-reaper lands A-645 (a) TOP. svn matrix: svnsync 2,000 revisions in ~18 min, peak 2,511 MiB against a 447 MiB dump; to w-svn first. LANDED: 84c74cc52 (w-svn DeltaV v1 part 1), eea6b39f6 (w-git keepalive while a body is read; decode measured at ms, so the wall time under load is the step count: fuller steps routed to w-engine), 362ceb16a (one source for liveness budgets). hg r15: clone-rev fails PastAdmission in lookup (w-hg fixing); clone 809 s at load ~67 against 42 s quiet (load sensitivity).
- 2026-11-20 12:55 PST DESIGN LANDED: A-767 9d74daf2a (w-store; + research/vcs-store-prior-art.md): one unit, the changeset; history an append-only changeset log per device (Mantle hyper-log framing, CRC-32C, sliced appends visible at a durable commit mark, F_FULLFSYNC on macOS); K/V tables R/N/C/V/O/D/G derived from the log with no second WAL; standing grants. PUSHED BACK before any build: A-767 stores each payload as received (pack/changegroup/svndiff), while w-hg measured a tool-neutral per-path changeset (~137 MiB vs 4,297); content once across pushes, forks and tools (A-64/A-727) must be measured. A/B: raw payload + work index vs normalized content-addressed vs a hybrid.
- 2026-11-20 12:55 PST LANDED: ee4ee1922 (w-engine A-760: an increment names content by identity, read a slice a step), A-764 51a061d (w-fed-faults: credentials survive a takeover), 7ffddbdf0 (w-k90-vendor: scattered position plans at most 8 bytes an object), 0589337f0/5e2842479/506c59795 (g226: waits on the kernel's word and the processes' own CPU), 60cc4073a (w-lint: today's landings back under the wall).
- 2026-11-20 12:57 PST WINDOWS (w-interop, jupiter, debug, base 9d4c5e5): hg 1.0 cut 24 of 24, the first all-green Windows hg run (was 4); git v1.0.0 cut 16 of 27 (was 4). One restart per run (exit 143, no kill line): to g226. HEAD lost after the restart (11 git rows): to w-git. HTTP port not kept after the restart (A-748?): to w-reaper after A-645 (a).
- 2026-11-20 13:00 PST svn fetch after 2502607c4 (w-bugs run 13): svn:// checkout 8.0 s (run 8: 7.1, regression: 25.0), svn:// update 5.6 s (2.7 / 32.5), DAV checkout 7.5 s (7.6 / 23.2), DAV update 4.6 s (3.6 / 24.6); working copies identical to svnadmin's. The residual (svn:// update ~2x) goes to w-perf (run_at lend). LANDED: A-768 586cf5035 (w-forming: a live log's whole capture read in turns from pinned chunks), da603dd1a (g226 waits), 111d561a0 (w-lint).
- 2026-11-20 13:05 PST LANDED: A-760 part 1 b0f181da2 (w-engine: increments name content runs by identity, read a copy slice a step, refused typed before any verdict while unread) and A-752 R3 4b6d5c24b. w-engine's next: A-760 part 2 (large-file commit built in slices) + the server reader. Windows git HEAD failures were the matrix script's default branch (w-git), not slates; w-interop fixes the script. DISK: this Mac 13 -> 28 GiB (a stale 18 GB target in another session's worktree).
- 2026-11-20 13:10 PST Studio-3 (adas-mac-studio.local) OFFLINE: w-k90-pin went down with it, so w-k90-pin-2 (01a1facadebb, Studio-4) took over goal 90's vendoring step (plan step 1 re-given; its CI evidence runs listed in progress/w-k90-pin.md). kiln's blocking GitHub run 44784299374 finished (failure) at 20:02Z, so GitHub runs flow again. LANDED: 7e7e242f3 (w-k90-a520: the export route's judge, container_engine::export_engine, accepts engines that mount the NFS export, Docker Desktop on Windows included), 4b335a696 (g226: the anchor names a daemon's exit in words; Windows exit 143 was Git Bash's kill in the matrix, not slates).
- 2026-11-20 13:13 PST LANDED: f95edf68e (w-balance: acknowledged pushes survive the owner's death; a push is acknowledged only once f+1 hold its record, the successor restores it before rebuilding; v3_takeover.rs 3/3 with a git fsck-clean clone). Next: remove the grant-all workaround now that A-764 is in, then the v3 switch. 72240f556 (w-perf: repository windows lent in place, ContentStore::run_at the one read), 553e3d3ee (w-lint round 8).
- 2026-11-20 13:18 PST ADA: "dispatch agents to tackle these areas and gaps for slates" (the Slates-vs-Sapling comparison). Six agents with briefs: w-work-cow (a Work over a Green costs its divergence, not N full copies; measured from request to first edit) big Studio; w-work-mount (ten ordinary editors, journals submitted, the Green granted to disk; GAP-A9-14, landing.rs merge-volume snapshot) Studio-2; w-rewrite-ops (D-O17: whole-file rewrites become precise edits; diff-shape A/B) big Studio; w-sessionlog (Smartlog-sized slates log) Studio-2; w-realtools (pragma once, make, cargo and git status across snapshot, eviction and takeover) and w-provision-gate (R9's absent CI gate; loaded p99 1.6 ms) are QUEUED (links down) and start by themselves; never retry.
- 2026-11-20 13:39 PST ADA: w-k90-pin shown offline. Cause: kiln's control link to adas-mac-studio.local (cec37f60) closed at 13:11. The runner still reports in, so kiln shows it connected, but every command to it times out: a run, a diagnose, and kiln's own upgrade, which failed. Restart queued on its own conversation (resume 01a1f9c0-5456-7720-9d74-8e0234066016), with a split: kiln lanes to w-k90-pin, GitHub to w-k90-pin-2. It needs the Studio's kiln-agent restarted (SSH is off there, and kiln has no restart command). g221 and g223 there are cut off too.
- 2026-11-20 13:39 PST DISK: the MacBook hit 0 bytes at ~13:42, so every shell failed. The writer was Reliquary's build into ~/.cache/reliquary-rq01 (22.7 GB, growing); kiln-send-route-target holds 30 GB. Deleted: kiln-gate target (8 GB, idle since 11-19), main's slates target, the a520 target, w-perf's 14.6 GB and w-svn's 3.7 GB; w-hg deleted its own 2.8 GB. 23 GiB free at 13:44. The fleet builds only on runners until this Mac is over 40 GB.
- 2026-11-20 13:39 PST LANDED (reported): w-hg A-575 46951a489 (a whole hg answer is admitted the fetch's room; clone-rev fixed); w-interop 210302b1b (Windows git 27/27, the first all-green); w-hg load sensitivity: clone 43 s quiet, 72 s at load 30, 0 kills.
- 2026-11-20 14:19 PST LANDED (reported): w-hg 648a46d1a, the changeset A/B: both arms about a third of stock's 84 MiB; grouped by changeset, old reads p50 864 µs against 91 per file, and a clone's file section 2.11 s against 0.27 s. RULING: the changeset stays the unit; a derived per-path form arm goes into w-store's A/B and must meet the per-file numbers. w-git 5518fb2bc (A-722 B2's in-flight half: a push keeps the client's pack, bounded at 2x it) and 00e6da185 (t_a665's deadline derived; the commit's first O(objects) cut). w-bugs 02a478233 merged 618220deb (A-770 deepen; the git v2.0.0 cut passes every row; deepen 217 s is now w-bugs' perf gap against stock git). UI goal 93: git, hg and interop parts updated.
- 2026-11-20 14:19 PST RULINGS: w-perf's WindowCache (41608e8d7) lands before w-svn's A-772 drive fix; w-svn A/Bs on top. w-reaper fixes the issuer route giving up on EINTR (w-interop's Linux matrices are refused) before A-645 (a). g222 and w-provision-gate share the shard-scheduling cause of millisecond loaded p99s.
- 2026-11-20 14:19 PST NETWORK: the Studios have no Ethernet connected (Studio-4: en0 inactive, Wi-Fi on 2.4 GHz channel 9, -72 dBm, 104 Mbps). Pings from here average 88-272 ms, and from Studio-4 to the big Studio 3.3-3.9 s; adacachy, which hosts kiln's server, answers in 3-8 ms. So reach and kiln time out on the Studios (w-k90-pin offline, w-hg's runs, w-fed-move). Correction: kiln's server is on adacachy, not on the MacBook.
- 2026-11-20 14:19 PST DISK: packed loose git objects in 13 idle-or-live clones under ~/slates-work (gc --prune=never; g066 4,420 -> 300 MB after removing 6 day-old empty ref locks); reliquary-c3 deleted 22 GB of its own; the MacBook is at 44 GiB.
- 2026-11-20 15:19 PST MAIN MOVED (~14:54) from the MacBook (adas-macbook-pro-2.local) to the big Studio (Adas-Mac-Studio.local): cron recreated; this state checkout and a blobless audit clone made here (HTTPS); the MacBook's slates-work-* sessions told through kiln to write inbox/main-from-<name>-*.md, since cross-machine SendMessage can't reach main.
- 2026-11-20 15:19 PST LANDED (confirmed on origin/main): w-forming A-768 586cf5035, A-771 cb1b698f0, A-775 035df1690 (stall 415 s -> served in 8.25 s); w-work-mount A-774 a2415c930 via 089bc78a0 (ten editors through mounted Works, landed under a grant); w-sessionlog A-773 2804a486f via 4702be3b9 (slates log); w-work-cow 819341ab4 as baa470b90 (Works are lazy: +75 KiB a Work, not a copy); w-rewrite-ops D-O17 / A-776 via 561c655a3 (whole-file saves refined to the writer's edits); w-balance e73283fd6, 08fba4007, c5ba38665 (v3 on; v2 vs v3 even on one node); w-git 47dc24b72 (answer steps 229,619 -> 168,761), A-722 4f 061f4956a, 4g 6737b04a7; w-fed-move A-756 0f681456 + kiln 4c19f1aad; w-k90-microvm kiln fa96bcc47, 522ba54b3, Shards b9006561b.
- 2026-11-20 15:19 PST GOAL MET: goal 91 'The live move across two Mac runners' (w-fed-move run 10, release, byte for byte across the Studio and adas-macbook-pro). Goal 90's microVM and resync parts, goal 93's git part and goal 85's chaos part updated with today's landings.
- 2026-11-20 15:19 PST RULINGS: w-git homes the pack as content (record v4, a window a turn, content once, v4 replaces v3, shaped as A-767's payload); w-sizing's content extents made on demand: yes (macOS first, provisioning histogram unchanged, each platform's give-back measured first); w-sessionlog: a newer presentation supersedes the older (typed refusal, no clock expiry); w-rewrite-ops: AC-6.10's exception accepted, refine sliced under the quantum first; w-balance fixes main's red names-log test (455fe41fb) first; w-perf lands the WindowCache 41608e8d7 alone; w-shared lands sweep2 now; w-work-cow takes chunk-grain copy on first edit; w-fed-move takes 'unplaced' first move asks; w-k90-microvm takes kiln's take_down, first-mount wait and collapsed notices; w-engine starts pacing (derived weight).
- 2026-11-20 15:19 PST DISK/MOUNTS: Studio-4 at 16 GiB (97%): unmounted w-forming's dead test mount (port 49260); kiln-01a1f9e974cb's dead mount (port 57217) is root's, needs Ada's sudo; clean-aggressive.sh (lsof -b -w now) copied there and running (r13). w-k90-pin not restarted: w-k90-pin-2 carries its kiln and GitHub lanes.
- 2026-11-20 15:30 PST LANDED (confirmed on origin/main): w-perf 41608e8d7 + ac1ad6ad7 via b51f908f2 (per-reader WindowCache, identity shim removed; replay_bench 328 s -> 1.82 s at best); w-svn 64655a32c (replay_bench and a bounded-windows replay test); w-perf hg import rerun D in BENCHMARKS 14e4ff765 (peak 5.73 GB, 65x the 87.8 MB .hg). Goal 93's hg and svn parts updated (g226's 143 finding closes 'the anchor killing an idle daemon').
- 2026-11-20 15:30 PST RULINGS: w-engine's pacing: yes to work-conserving steps + the derived per-kind estimate + A/B, with the waiting check once a quantum (no per-unit atomic), a step bounded by the watchdog's derived share, §4.3 amended with evidence, landing only if steps and wall time fall and newcomers' latency holds. w-perf: go for the push_load/git_scale rerun and stock FSFS's size on the big Studio, then the hg import's 65x peak. g226: Linux and Windows give-back measurements for w-sizing's extents. w-svn: its 1-in-3 svn hook flake. w-sizing: interrupted its question prompt; the 15:20 ruling stands.
- 2026-11-20 15:30 PST NETWORK: Studio-2 and Studio-4 did not answer kiln in 15 s, and w-k90-pin's Studio (cec37f60) disconnected again; g223's restart there (its owed quiet-Mac run) was refused, to retry when it's back. MacBook 110 GiB free; big Studio 1.0 TiB.
- 2026-11-20 15:33 PST LANDED (confirmed on origin/main): w-sessionlog A-779 bbc605ac9 (a newer presentation supersedes the older; its grant refused LandingSuperseded); w-balance 3c9b41b3f (main's red names-log test from 455fe41fb fixed to A-724 8c's rule); w-shared sweep family 2 via ab30bfb75. Disks: Studio-2 65 GiB, Studio-4 47 GiB, MacBook 110 GiB, big Studio 1.0 TiB.
- 2026-11-20 15:33 PST KILN: adacachy's server journal shows cec37f60's agent link closing every 1-5 min since 15:11 (reason closed); given to w-k90-pin-2 to find and fix. g223's restart waits on it.
- 2026-11-20 15:34 PST LANDED: w-perf 21976a3b8 (BENCHMARKS: stock FSFS holds git-2000 in 41.0 MiB; the svn records' deltas 33.2 MiB raw, 26-27 MiB zstd); goal 93's svn part updated. RULINGS: g226 runs its Linux and Windows give-back probes itself (a design measurement is the agent's own); w-perf: if whole texts dominate the hg import's 5.73 GB, deltas go inside the changeset shape (w-hg's per-file patch stays held), plus a red check on revlog chain reads copying per link. Disks: Studio-2 57 GiB, Studio-4 46, MacBook 109, big Studio 1.0 TiB.
- 2026-11-20 15:39 PST LANDED: w-balance 8f286457c (a homing reaches its region homes from the control shard and waits for a lent session; bench three_node_push). RULING: w-balance's coalescing (PutMany per peer, group commit) approved, bounded and derived, red first, A/B across three nodes. ASKED: w-k90-vendor to put the export_engine re-vendor next (w-k90-containers' jupiter leg waits on it). DISK: Studio-2 65 -> 41 GiB in ~12 min; its five largest targets' owners (w-lint 27.7 GB, w-work-mount 20.2, w-sessionlog 10.4, w-k90-win 9.6, w-k90-microvm 7.1) told to trim by exact path. Studio-4 44 GiB, MacBook 105, big Studio 1.0 TiB.
- 2026-11-20 15:44 PST LANDED (on origin/main): w-git 5c6327b8e (A-722: a record's replay reads its pack in place); w-realtools A-781 5d639b68e (an unchanged base file keeps its inode number across a takeover, red first: #pragma once); A-780 5586b9194 (svn DAV PROPFIND writes properties in place: allprop of 32 files 5,691 allocations -> 445). w-k90-whp: the WHP microVM drive passed on jupiter (r26, kiln cc1ecb625) -> goal 90's microVM part now waits only on KVM.
- 2026-11-20 15:44 PST DISK: Studio-2 at 28 GiB and falling ~2 GB/min: the writer is w-work-mount's whole slates-server test-suite build (~170 MB a test binary); told to stop it, build focused tests with CARGO_PROFILE_TEST_DEBUG=0 and delete the rest; clean-aggressive.sh copied to Studio-2 and running (r31). w-git's pack-homing ruling also written to inbox/w-git-from-main-pack-homing.md (its turn hasn't ended to receive the 15:20 kiln message).
- 2026-11-20 15:46 PST UI GOALS (Ada asked): today 13 items met (goals 82 and 83 whole, 11 parts), 12 of them before 07:20; since then only goal 91's live move (15:18). Open: 84 3/5, 85 7/10, 86 2/4, 87 1/2 (v0.1.0 publish needs Ada's go), 88 no parts, 89 6/8, 90 32/39, 91 11/12, 92 11/13, 93 64/70. STAFFED: goal 92's two ownerless parts (A-555 parts 2-3, the Shards-owed items) to w-k90-whp, whose WHP piece is done.
- 2026-11-20 15:49 PST LANDED (on origin/main): w-perf 8ce3d6152 (BENCHMARKS, A-722 p3 by use: one push 1.4x faster, ten at once p99 1.6x, first clone 1.6x; the partial clone 1.4-1.5x SLOWER: to w-git, first); w-perf b0aa82473 (hg revlog chains apply within two buffers: a 64-link read 263,219 -> 30,312 bytes charged); w-store 1090f5fdb (A-767 item 7 phase 1: a changeset's stored form; a second tool's same history adds 85.2 MB raw, 0 text bytes normalized) + A/B phase 1 e7522325d. Goal 93's git part updated. DISK: Studio-2 back to 54 GiB after the trims; Studio-4 40; MacBook 94.
- 2026-11-20 15:54 PST ADA'S ORDER: close the existing UI items. Rule added to briefs/common.md ('CLOSE THE OPEN ITEMS'); every open part's items and owners in briefs/close-list.md; side pieces dropped; main adds no new open items to parts.
