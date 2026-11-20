# g079 -- UI goal 85 (kiln 17ad60b6e049816993c4a27fb6f48986): every open slates bug fixed, failing test first, red-checked
# Resumed 2026-11-18 (after goal 84 met). THE RULE (final): ONE piece -> failing unit test (red once) + fix + bug record +
# design/GAPS -> `cargo test -p <crate> --lib <filter>` + `cargo fmt` + `cargo clippy --locked -p <crate> --all-targets
# -- -D warnings` + `cargo xtask check` -> fetch, merge origin/main, diff --stat, `git push origin HEAD:main` -> END
# (progress, run-requests, few-line report). Branch with `git worktree add -b`. No checkout/switch/stash/reset/commit-tree.
# Builds: Mac Studio d1323898 (sync:false, GitHub clone of a pushed branch, own CARGO_TARGET_DIR under ~/.cache there).

## Bug list (main's evidence, 2026-11-18). One bug per piece; main starts each next piece.
- [x] 1 (A-485, 940f00615 + docs a9b65631c, main 5fef5b1ef; red r471, green r474) `a_move_whose_target_dies_at_the_switch_is_served_by_its_successor` (crates/server/tests/fleet.rs, A-291
  move flow) fails every run: 4/4 macOS (r242/r277 at 3022c6894 and 2a5bf7264) + Linux (r327). b's successor never
  serves the files; last read NotFound (Linux) / HomedElsewhere{region 0} (macOS). Counters on a: move.handoff_begun 1,
  move.released 1, move.frozen_refused 2,374, forward.batch_done 1,605,259 (macOS). Main checks r342 at d02505d3b.
- [-] 2 (g230's now) `a_volume_moved_to_another_member_is_served_there_and_its_old_owner_forwards`: move back to creator loops
  Moving / MoveRefused{Unplaced}; failed at 3022c6894 x2, passed once at 2a5bf7264; main bisecting A-435 vs A-437 (r341).
- [-] 3 (g225's now) moves::seal_adopted seals twice when its answer is lost to a STOPPED shard (g251's finding, GAPS): record the
  seal on the owner shard so it is idempotent.
- [-] 4 (g229's now) `a_location_round_asks_a_peer_whose_session_was_out_once_it_returns`: intermittent OwnerUnreachable
  (owner_location budget_spent 1, gave_up 1), failing at 2a5bf7264 too (predates A-437).
- [-] 5 (g071's now) Windows: the fleet manifest reader needs /etc/resolv.conf (g267's finding): a Windows runner cannot join a fleet.
- [-] 6 (g083's now) recovery `a_boot_give_back_holds_its_shard_a_slice_of_cpu_a_step` failed once under load (adacachy r271), passed on
  rerun: does it judge a slice by the wall clock?
(g083 has A-476; g085 and g264 the landing/kiln ones.)

## Earlier (goal 83 list, before this assignment): B1 landed 4b7646fe7; B7 landed 7481466bc (A-419, main 590e1a300).
B7's siblings open in GAPS: A-365 adopted object's unnamed in-flight range; Windows prepare_range before the charge.

## Log
- Starting bug 1. Worktree: ~/slates-work/g079-a485 (branch agents/g079-a485-move-dies, from origin/main).
- Cause found (A-485): A-398 split the install into a per-copy walk (fleet::install_on_copy -> takeover::resolve_held_object)
  and left moves::follow_moved_objects at the end of the per-copy step, after its early returns: it ran only for a copy routed
  to a retired host, so at a move's switch no holder's copy followed the move (route + authority stayed with the live old
  owner), the dead target's takeover listed nothing, both survivors confirmed without the volume, the old owner released.
  Main's r327 evidence agrees (no takeover.adopted / materialize.* on c). Fix: per-copy moves::follow_moved_object in every
  path of resolve_held_object (whole pass removed). Red r471 (Studio): routing stayed old owner. Green + clippy + xtask r474.
- Landing commit 940f00615 on agents/g079-a485-move-dies (not pushed yet); scratch branches agents/g079-a485-red/-green.

## 2026-11-18 ~23:40 UTC: bug 1 LANDED (A-485), ENDED
- main 5fef5b1ef carries 940f00615 (fix + test + bug record + GAPS + design A-485) and a9b65631c (docs: bug 2 link stated as
  not shown). Merges 1e5389496 and 5fef5b1ef (each: the amendment log's end conflicted, main first, A-485 last; the second
  merge brought no code).
- Unit runs (Mac Studio d1323898, debug): red r471 -- the new test failed, copy routed to the old owner (left owner ...868,
  right target ...871); green r474 -- the test ok, takeover::tests + moves::tests 17/17, clippy -p slates-server
  --all-targets exit 0, xtask check: structural ok, 45 literal findings all in other agents' files (none in A-485's);
  rerun on the merged tree 1e5389496: 17/17.
- Filed (run-requests.md 23:55Z): the target-dies fleet test on macOS + Linux, the move/install-walk fleet tests, and bug 2's
  test once on this main.
- Cleaned: scratch branches agents/g079-a485-red/-green/-merged deleted, red worktree removed, Studio clones/target deleted.
- Next (claimed in bug-claims, NOT started; main starts it): GAPS "a kept FUSE mount of a volume that was not recovered is let
  go but never unmounted, answering ENOTCONN until unmounted by hand" (Linux bare metal, goal 88). Other open items of mine
  from goal 83's list: B7a/B7b (A-419's siblings), B13, B2, B12, B4, B9, B8, B10, B3, B5, B11, B14-B16.
- Worktree kept: ~/slates-work/g079-a485 (branch agents/g079-a485-move-dies = main 5fef5b1ef, clean).

## 2026-11-18 ~23:59 UTC: A-509 (UI 85) -- kept FUSE mount of a not-recovered volume answered ENOTCONN until unmounted by hand
- Decision (design: A-102 forbids an unmount; §4.8 NotRecovered): serve the kept mount on, every request EIO (FORGET by nothing),
  the anchor keeps the device so a later restart that rebuilds the volume serves it again; an unheld mount is left, record ended.
- Code: verbs::keep_unrecovered_mounts (recovery routes such a volume's FUSE mounts), fuse::refuse_unrecovered (+ turn_with branch,
  fuse.unrecovered_refused), bridge-fuse channel::refuse_unserved. Test (Linux unit): fuse::tests::
  a_kept_mount_of_a_volume_recovery_could_not_rebuild_answers_each_request_eio.
- Worktree ~/slates-work/g079-a509 (branch agents/g079-a509-unrecovered-mount, commit a219480af, not pushed yet).
  Scratch: agents/g079-a509-red (7c9071aed, the change minus the turn's branch), agents/g079-a509-green (67db94732).
- Runs: adacachy capped unit r97 (red) running; then green in the same clone (ff to green).
- LANDED: main 52adc8dd1 carries a219480af (A-509), merges ca4bc8406 and 52adc8dd1 (each: the amendment log's end, main first).
  Red (adacachy r97): ["ended","ended"]; green: 1/1, bridge-fuse channel 5/5; merged-tree rerun 1/1. Scratch branches deleted,
  red worktree removed, adacachy clones/target deleted. Filed: clippy/xtask (main's), the Linux FUSE flow incl. recovery-later.
- ENDED. Next (main starts it): my goal 83 leftovers -- B7a/B7b (A-419 siblings), B13, B2, B12, B4, B9, B8, B10, B3, B5, B11, B14-B16.

## 2026-11-19 ~01:30 UTC: A-526 (UI 85, part 01a1f12c) -- B7a LANDED; B7b not taken (different cause)
- main 09ff2848a carries a29d8f40c: ShardsBacking.inherited (set by adopt_vmm) + owes_clear; the fill clears an owed range first.
  Test bridge-core dax_backing::tests::a_range_a_dead_daemons_fill_left_part_way_reads_the_volume_once_filled_again: red r135
  (Studio; the final zeros assertion failed), green 6/6. Merges with main twice (the amendment log's end). Scratch deleted.
- B7b (Windows: prepare_range commits before the charge; a Budget refusal leaves a committed range uncounted) does NOT share
  B7a's cause, so per main's rule it was left for its own piece (needs a Windows-runnable unit test: on Windows the bridge-core
  lib test can assert the session's charge vs the object's committed ranges).
- ENDED.

## 2026-11-19 ~02:10 UTC: A-530 (UI 85, part 01a1f131) -- B7b LANDED
- main ff94f8848 carries 11dd2df57: dax_backing::begin_range over RangeStart (charge, then prepare, then clear; undo on a
  later refusal), StartingRange does the steps on the store. Test a_range_is_charged_before_anything_is_made_for_it (every
  host): red r(Studio) with the old order ["prepare","clear","charge"], green 7/7. Scratch branches deleted (retried after a
  GitHub timeout), red worktree removed, Studio clone/target deleted. Windows runs filed for main.
- ENDED. Left from goal-83's list: B13, B2, B12, B4, B9, B8, B10, B3, B5, B11, B14-B16.

## 2026-11-19 ~03:00 UTC: A-535 (UI 85, part 01a1f12c) -- B13 LANDED
- main abd413701 carries 555c4851c: api mount::available (macOS, BSDs) = loopback_available(capture_open(bpf_devices()));
  MountMissing::PrivateLoopback; contract 43. Decision recorded (GAPS, A-535): both GAPS candidates rejected with reasons;
  evidence: all five Macs /dev/bpf0 crw------- root wheel, no access_bpf. Test red (Studio) then 12/12. Scratch deleted.
- ENDED. Next in order: B2, B12, B4, B9, B8, B10, B3, B5, B11, B14-B16.

## 2026-11-19 ~04:10 UTC: A-543 (UI 85) -- B2 part 1 LANDED (+ §4.12 contract-43 line for A-535)
- main 3cc9ce3f0 carries 279af73ba: vfs Volume::stored_name_no; bridge-core Bridge::stored_entry (default: as asked),
  VolumeBridge/ScopedBridge impls, stored_path; test stored_path_tests::every_spelling_... red (Studio) then green.
  §4.12's contract log gained CONTRACT = 43 (A-535) in the first merge commit (main asked: g293 found it missing).
- Part 2 (Windows-only: CaseSensitiveSearch for exact volumes only, the stored path as WinFsp's normalized name, the drive
  test) filed in run-requests.md for main to write/compile, per the rule. B2 stays open in GAPS until part 2.
- Pushing took four merges (main moved each time; the amendment log's end each time). Scratch deleted.
- ENDED. Next in order: B12, B4, B9, B8, B10, B3, B5, B11, B14-B16.

## 2026-11-19 ~05:20 UTC: restarted g079 (after a usage-limit stop) -- B12 (A-550) part 1 was ALREADY LANDED by my predecessor
- main a613562eb (merge) carries 6703be419: crates/server/src/held_send.rs (SendLedger, Resume, resume: Untouched/Whole/Torn)
  + test a_held_connection_is_served_on_only_on_a_records_boundary (red with today's rule, green, Mac Studio) + bug record
  docs/bugs/2026-11-18-linux-nfs-loopback-connections-were-dropped-at-every-daemon-restart.md + GAPS + design A-550.
  The predecessor stopped before filing part 2 in run-requests.md and before this file's entry: both done now.
- Part 2 (Linux-only, main's): the ledger in anchor RAM, tcpi_bytes_acked + SIOCOUTQ at adoption, Linux connections held,
  a Torn one ended before it is read, then the Linux kill run of crates/cli/tests/nfs_held.rs.
- B-list: recovered whole from the predecessor's transcript (its 2026-11-18 ~19:27Z progress file, g079-fsync-after-kills-nfs.md);
  the 11 remaining lines (B12, B4, B9, B8, B10, B3, B5, B11, B14, B15, B16) written to bug-claims.md, GAPS lines re-found
  on main a613562eb. Main told ("B-list written").
- Main's clippy gate: three unwraps in A-543's test helper stored_in (bridge-core lib.rs:1070/1071/1074, unwrap_in_result).
  Fix on branch agents/g079-a543-unwrap (worktree ~/slates-work/g079-a543-unwrap), commit 2889b917f: setup moved to
  with_main_rs (no Result), stored_in names the path alone. Unit run on the Studio next, then merge and push.
- LANDED: main 5c0e4f43b carries 2889b917f (the A-543 unwrap follow-up) with merges fe48f08e3 and 5c0e4f43b (no conflicts;
  main's 01e4137be and c6bf19cbd touch nothing bridge-core builds on). Studio d1323898 (debug): at 2889b91 and the merged
  fe48f08, `cargo fmt -p slates-bridge-core -- --check` exit 0, `cargo test --locked -p slates-bridge-core --lib
  stored_path_tests` 1 of 1 ok. Clippy recheck filed for main (I ran none).
- Filed in run-requests.md: A-550 part 2 (Linux-only) + A-550's clippy/xtask gates + the bridge-core clippy recheck.
- amendments.tsv: A-543 and A-550 rows updated from "assigned" to their landings.
- Cleaned: Studio ~/.cache g079-a543u-* and the predecessor's slates-work-target-g079, -g079-xwin, g079-serve (2.7 GB;
  g079-logs/sink.log kept, 3 KB); GitHub branch agents/g079-a543-unwrap deleted (merged); worktree removed.
  Kept: ~/slates-work/g079-a550 (clean, = a613562eb); GitHub agents/g079-diag-nfs-trace (diagnostics, not in main).
- Slip: the Studio rerun command carried a fallback `git reset --hard FETCH_HEAD` in the throwaway clone (a no-op after the
  fast-forward); against the preamble's rule, not repeated.
- ENDED. Left from goal-83's list, all in bug-claims.md with files and tests: B4, B9, B8, B10, B3, B5, B11, B14, B15, B16
  (B12 part 2 is main's).

## 2026-11-19 ~05:50 UTC: A-550 part 2 (UI 85, B12 continued) -- main: Linux-only code is mine to write; main compiles what I can't
- Scope: the send ledger in anchor RAM; tcpi_bytes_acked + SIOCOUTQ (SYN counted or not, kernel source cited); Linux
  connections held across a restart, a Torn one ended. Tests: pure judgment in every host's --lib (Studio); Linux-only --lib
  in a capped adacachy systemd-run --user unit; nfs_held.rs on Linux stays main's run.
- Worktree ~/slates-work/g079-a550b, branch agents/g079-a550-part2 from origin/main.
- Design (decided, from the code and the kernel source):
  - Kernel count (rt netsys, Linux): W = tcpi_bytes_acked (TCP_INFO bytes 120..128, get_option of exactly 128 bytes,
    which refuses a shorter kernel copy) + SIOCOUTQ (= TIOCOUTQ, sockios.h:27; one new unsafe ioctl, budget 106->107),
    read acked/queue/acked and accepted only when the two acked readings agree (retries bounded by the first queue
    reading: each retry needs >=1 byte acked). No SYN: a passive child starts snd_una = write_seq = snt_isn+1
    (tcp_minisocks.c:577/594 master, 540/557 v6.12), bytes_acked only += ack-snd_una (tcp_input.c:3941; 3652),
    SYN_RECV writes snd_una directly (tcp_input.c:7381; 6851), the listener is zero-allocated (sock.c:2337) and
    cloned (sock.c:2521). FIN adds 1 (tcp_output.c:3882). Admission also checks a fresh accepted socket counts 0.
  - Ledger: a SharedObject (memfd) held by the anchor as a copy kind (record: tag, generation, slots, stride; copy id
    0xFFFF<<48 | low 48 bits of the generation, disjoint from every encoded handle). Header word = high water of slots
    claimed; slot = [connection, selector, committed0, in_flight0, committed1, in_flight1] on its own cache line;
    a write fills the idle record then flips the selector (Release), so no torn record is ever named.
  - Start: judge each handed connection with the newest valid inherited ledger (resume: Untouched/Whole serve on,
    Torn or no record ends); fill a fresh ledger (generation+1) with the adopted connections (committed=W); hold it;
    release every inherited ledger; then serve. held_copy_bound += 2 on Linux.
  - Wiring: admit (NotOffered + anchored + ledger + count 0 -> claim slot, hold), send_run/send_callbacks
    (begin before, commit after), Ending frees the slot.
- Built on scratch branch agents/g079-a550-part2 (worktree ~/slates-work/g079-a550b); landing squashed onto origin/main
  b7081c388 as bfa9160e0 on agents/g079-a550-part2-land (worktree ~/slates-work/g079-a550c).
- Runs (all --lib + fmt): Studio d1323898 at e1f163b: rt handed_tests 3/3, server held_send 7/7, fmt clean; red check
  (selector flipped before its record): the crash oracle FAILED at the first send's commit, store 0 (End(Torn) where the
  stream stood whole at 5 bytes), file restored. adacachy (Linux 7.2.9-2-cachyos, capped unit r142): rt netsys 4/4 incl.
  an_accepted_connection_counts_every_byte_written_and_no_syn (0, 1,000, 1,000), server held_send 8/8 incl.
  a_ledger_object_outlives_its_writer. Merged tree bfa9160: Studio fmt 0, rt 3/3, server 7/7; adacachy r309 running.
- LANDED: main 63c55ffd5 carries bfa9160e0 (A-550 part 2, squashed from the scratch branch) and merges c5c3a15f9 and
  three more (main moved four times; no conflicts; main's newest touched only daemon.rs doc lines and other files).
  Merged tree c5c3a15: Studio fmt 0, rt 3/3, server 7/7; adacachy r309 at bfa9160: rt netsys 4/4, server held_send 8/8.
- Filed: run-requests.md (Linux kill run of nfs_held.rs x86_64+arm64, clippy rt/server, xtask with the unsafe budget,
  xwin check). amendments.tsv A-550 row and bug-claims B12 line: landed both parts.
- Cleaned: Studio and adacachy clones/targets/logs; GitHub scratch branches deleted; worktrees g079-a550b/-c removed.
- B12 ENDED. Next (main, 2026-11-19): A-598, goal 92 -- slates-hg compressed streams (bundle2 Compression GZ/BZ/ZS,
  bundle1 HG10GZ/HG10BZ/HG10UN), my own module; g291 is on A-588 in the same crate.

## 2026-11-19 ~03:50 UTC: A-598 (UI 92, Mercurial piece 1's rest: compressed streams) -- relaunched g079, started
- Readback of THE RULE sent to main. Predecessor's uncommitted fixtures (5 compressed bundles stock hg 7.2 wrote, script
  header, SHA256SUMS) committed on scratch branch agents/g079-a598-hg-compression (4a241cff0; worktree ~/slates-work/g079-a598).
- Landing worktree ~/slates-work/g079-a598b, branch agents/g079-a598-compressed from origin/main e85c17f9f; fixtures copied
  by git show, `shasum -a 256 -c SHA256SUMS` 5 of 5 OK.
- Checked: bzip2 0.6.1 (build = false; default = libbz2-rs-sys 0.2.6, default-features off, rust-allocator only, no libc)
  and libbz2-rs-sys 0.2.6 (build = false, only .rs sources) are pure Rust. flate2 1.1.11 default rust_backend.
- 7.2 source read (sdist sha256 a250227e...): readers accept Compression=_truncatedBZ and HG10ZS (no writer makes them);
  bundle2's reader ignores bytes after a compressed stream; zstd reader reads one frame; `_forwardchunks` drops the
  Compression parameter after inflating -- the shape of compression::plain.
- Design: new module crates/hg/src/compression.rs (engines; inflate under the admission incl. each stream's window;
  plain bundle like _forwardchunks; compress_bundle); bundle2.rs: Compression GZ/BZ/ZS read by stream_params, decode
  refuses a still-compressed stream, a repeated Compression refused; bundle1.rs docs.

## 2026-11-19 ~04:45 UTC: A-598 LANDED (UI 92, Mercurial piece 1's rest: compressed streams) -- main 6ff5d4c4c (48689506f..6ff5d4c4c)
- Commits on agents/g079-a598-compressed: 6251febd5 (compression.rs + tests + fixtures + bundle2/bundle1/lib), 787e35429
  (merge: beside g291's A-588 in slates-hg; main's A-607 flate2 entry taken as is), fa0480f15 (workspace bzip2 entry),
  48288216c (first build's one error: proptest's prop_assert! stringifies its condition into a format string, `{ .. }`
  broke it; the lock's 4 packages by hand, without cargo's 5 windows-sys 0.60->0.61 re-pointings), 645376d68 (property
  tests made non-vacuous after the red run), 8af3d9192 (docs: §4.17.3 status + piece 1 whole, A-598 amendment, GAPS,
  research §16), merges 62ef33205 and 6ff5d4c4c (the amendment log's end both times, main first, A-598 last).
- What it does: compression::plain makes a compressed bundle plain as 7.2's _forwardchunks does (bundle1 HG10GZ/HG10BZ
  -> HG10UN + inflated cg; bundle2 Compression GZ/BZ/ZS -> param dropped, core inflated; plain bundles borrowed);
  each stream's decoder memory as its header sizes it (zlib 32 KiB, bzip2 4 x block, libzstd's buffers) and every byte
  of inflated room charged to the admission first -> bombs refused PastAdmission by kind; compress_bundle/compress as 7.2
  writes, caller's level within each engine's range. bundle2: GZ/BZ/ZS read by stream_params, a still-compressed stream
  refused by decode, Compression twice refused. Stricter than 7.2: trailing bytes, `_truncatedBZ`, HG10ZS.
- Runs (Mac Studio d1323898, kiln, debug, each arm its own target dir): red (stubbed entry points) 14 of 16 new tests
  failed; green `cargo test --locked -p slates-hg --lib` 174/174 at 645376d68 and on the merged tree 62ef33205; fmt clean.
  libzstd's own ZSTD_sizeof_DCtx equals the charge for 7.2's frame: 2,490,432 bytes. LESSON: a first red/green pair
  sharing one CARGO_TARGET_DIR is invalid (cargo names a workspace member's build by its path within the workspace; the
  green clone ran red's binary) -- own target dir per arm, always.
- Filed: run-requests.md (clippy, xtask, doc for slates-hg; cargo check --locked on Linux/Windows x86_64+arm64).
  amendments.tsv A-598 row: landed.
- Cleaned: Studio ~/.cache/g079-a598 (1.3 GB), g079-a598-fixtures.*, g079-a598-src (predecessor's; contents landed,
  SHA-256 matched); GitHub branches agents/g079-a598-red, -compressed, -hg-compression deleted; worktrees g079-a598 and
  g079-a598-red removed. Kept: ~/slates-work/g079-a598b (clean, = main 6ff5d4c4c); Studio ~/.cache/g079-logs (4 KB).
- Left (others' pieces, in GAPS): inflating in slices within the step budget (piece 5, the inflater made resumable
  there); the 0.2 wire answers compressed at the learned level (piece 4); headerless bundle1 and HGS1 not read.
- ENDED. Main starts my next piece.

## 2026-11-19 ~04:55 UTC: A-619 (UI 93, Mercurial piece 4 part 1: read serving's logic) -- started
- Row A-619 already in amendments.tsv (next free A-620). Worktree: ~/slates-work/g079-a619 (branch agents/g079-a619-serve
  from origin/main). Module: crates/hg/src/serve/ (mine). g291 is on A-611 (revlog reader) in the same crate.
- Plan, landed in parts, each merged at once:
  1. serve: Graph trait (changelog DAG: len, node, rev, parents, branch, closed, phase; bookmarks); discovery answers
     (heads desc-rev, known, between, branches, branchmap), listkeys (bookmarks, phases + publishing, namespaces),
     getbundle's choice (common filtered by hasnode, heads default all; missing = ::heads - ::common, rev order),
     phase-heads (publishing: all public; else by phase + heads(only(draft, public) and public())), binary bookmarks.
     Tests: naive DAG oracle on generated histories (proptest); a test-only port of 7.2's setdiscovery client with
     counters (rounds, queries); golden = A-569's ssh-clone/ssh-pull captures (repo rebuilt from the clone's getbundle).
  2. changegroup assembly (cg 02/03: changelog, manifests by c.manifest minus common links, files from outgoing
     manifests at c.files paths minus common links; full texts or held deltas), bundle2 parts in 7.2's order
     (changegroup, bookmarks, listkeys, obsmarkers relevant, phase-heads); new captures clone/pull/incoming.
  3. slates hg serve --stdio's loop over the SSH v1 codec.
- 7.2 sources: /tmp/g079-hg72 (sdist a250227e...; extract wireprotov1server.py, discovery.py, setdiscovery.py, exchange.py).

## 2026-11-19 ~05:20 UTC: A-619 part 1 LANDED (UI 93, Mercurial piece 4's read-serving logic) -- main f141e2843 (4c7208ab0..f141e2843)
- Commits (branch agents/g079-a619-serve, deleted after landing): a44f81636 (serve: Repository trait, discovery, pull,
  tests, capture script), dcf296e4f (first build: bookmarks iterator not Clone; takefullsample takes no heads; script
  puts evolution in the server's hgrc), 0e9984a55 + e590764de (golden probe test; 8 requests), 2974c8bee (docs: §4.17.3
  status + piece 4 line, A-619 amendment, GAPS, research §18 after main's §17), merges 4cec4aea6 (4 conflicts: lib.rs,
  design, GAPS, research), a doc fix, f141e2843 (third merge).
- Runs (Studio, own target dir per arm): red (every public answer stubbed empty) 7 of 7 failed; green slates-hg lib 182/182
  at e590764de, 197/197 on merged trees 4cec4aea6 and f141e2843 (the landed tree), fmt clean. Golden: a probe of stock
  `hg serve --stdio` 7.2 (heads, known, between, branches, branchmap, listkeys x3): 8 answers byte for byte.
- SLIP, caught before push: in the second merge my resolver's assertion failed (the conflict began mid-line) but the next
  line of the command (git add + commit) was not chained to it, so a merge with conflict markers was committed locally;
  fixed in a new commit before any push. Root: the FIRST merge's resolver (no assertion) had kept main's old line "four
  departures" beside its new "three departures" (main had edited the entry's last line, and the conflict began there).
  LESSONS: chain resolver && add && commit on ONE line; assert each side starts where expected; after resolving, run
  `git diff origin/main --stat` on the doc and read the removed lines (only mine may differ).
- Fixture transfer: the Studio cannot push (no GitHub credentials). The probe's four ASCII files were printed there as
  Python reprs and rebuilt here (/tmp/g079-scripts/probe_fixtures.py), each SHA-256 equal to the run's.
- Filed: run-requests.md (clippy/xtask/doc for slates-hg's serve). amendments.tsv A-619: part 1 landed, part 2 owed.
- Cleaned: Studio ~/.cache/g079-a619 (1.6 GB) + g079-hgserve.*; remote branches agents/g079-a619-red/-serve; worktrees
  g079-a619-red and g079-a598b (+ their branches). Kept: ~/slates-work/g079-a619 (clean, = main f141e2843).
- LEFT (A-619 part 2, main starts it): the changegroup a choice names (cg 02/03: changelog; manifests named by the missing
  changesets minus those linked to common ones; files from their manifests at the paths they touch; full texts or held
  deltas), the bundle2 a getbundle answers (parts in 7.2's order: changegroup, bookmarks, listkeys, obsmarkers relevant,
  phase-heads), golden over the script's sessions (clone, pull, incoming -- captured on the Studio, but their ~12 KB of
  binary bundles need a transfer path: printed reprs/base64 checked by SHA-256, or a way for the Studio to push);
  obsolete's listkeys dump; then `slates hg serve --stdio`'s loop and HTTP v1.
- ENDED.

## 2026-11-19 A-619 part 2 (UI goal 93): getbundle's answer -- LANDED main c314b4d8b (7896dbda9..c314b4d8b)
- Policy readback: THE RULE (cargo test --locked -p slates-hg --lib + fmt on the Mac Studio only, own CARGO_TARGET_DIR per
  arm, failing tests first, merge at once), lint-habits.md (11 habits) read and followed; implementer role, main coordinates.
- Built: crates/hg/src/serve/getbundle.rs (Store trait over Repository: changeset texts, manifest/file logs with
  position/parents/link/flags/text, file_node, markers; Asked; getbundle; changegroup; relevant markers) and
  getbundle_tests.rs; serve::Refusal gains NoBundle2, NoCommonExchange{what}, Store{what}; part 1's test helpers
  pub(super) and captured(state, bookmarks) factored out of probed().
- Read from 7.2's source on the Studio (uv run --with mercurial==7.2), and it changed the draft: _prunemanifests sends
  EVERY manifest outside ellipsis (draft pruned by link -- wrong, fixed); bundle2 asked for by any HG2 bundlecap, every
  bundle2= item merged (update); no b2caps -> no changegroup part; version chosen before the missing check; listkeys is
  csv (empty item = namespace); sortedmarkers sorts the whole tuple (draft used 3 fields, fixed); HGTAGSFNODES only from
  the tag cache (never sent here); bundle2 aborts become an error:abort part (owed to the loop).
- Fixtures: main's path (kiln code_run tar|base64 of the Studio dir, decoded here), 20 of 20 files SHA-256 checked
  against the Studio's SHA256SUMS.
- Commits: dc4a0765d (green), 6dcdd64be (first build's one error: an `impl Iterator + '_` that also held `node`;
  edition 2024 captures all lifetimes, bound dropped), de2b03c69 (non-vacuity case), d2d5b559a (docs: §4.17.3 status +
  piece 4 line, amendment A-619 (part 2) at the log's end, GAPS, research §19), merges d57a5e16b (GAPS + design x2,
  resolver asserted both sides, chained) and c314b4d8b (amendment log end).
- Runs (Studio): red (getbundle/changegroup stubbed) 8 of 8 then 9 of 9 failed, 199 passed; green 207/207, 208/208
  (load 8.4), merged tree 214/214 (load 7.5); fmt clean every time.
- Golden: clone/pull/incoming getbundles asked again of a store rebuilt from 7.2's answers: same part ids/params, payloads
  byte for byte, changegroup by meaning with node hashes; incoming's listkeys phases byte for byte.
- Model: 7.2's packer stated literally (read_delta_parents against p1/p2 or whole) vs this module on generated histories:
  equal on manifests and every file revision the client lacks; hand-built case where 7.2 resends a held revision.
- SLIP avoided: a GAPS rewrap touched 75 lines of a shared paragraph; restored the file from HEAD (git show) and redid the
  edit locally (17 lines).
- Filed: run-requests.md (clippy/xtask/doc, Linux/Windows for serve/getbundle). amendments.tsv A-619: part 2 landed,
  part 3 owed. Cleaned: Studio ~/.cache/g079-a619b, -a619b2-red, -a619b2-green; red worktree + branch (local, remote).
  Kept: ~/slates-work/g079-a619b (clean, = main c314b4d8b).
- LEFT (A-619 part 3, main starts it): `slates hg serve --stdio`'s loop over the SSH v1 codec (hello's capabilities,
  between, protocaps, batch's cmds, known/heads/branchmap/listkeys/getbundle dispatch, error:abort for bundle2 aborts,
  out-of-band errors otherwise), the replay of all three sessions whole; then HTTP v1; held deltas; obsolete's dump.
- ENDED.

## 2026-11-19 A-619 part 3 (UI goal 93): slates hg serve --stdio's session -- LANDED main f820c1b3a (15fc0bf7f..f820c1b3a)
- Policy readback unchanged (THE RULE; lint habits); implementer role.
- Built: crates/hg/src/serve/session.rs (serve(store, input, admission, stdout, stderr) -> Read<Step>: one SSH v1 request
  a step from 7.2's source of _runsshserver/dispatch/batch/hello/protocaps; 7.2's 21-command table; capabilities of what
  is served; getbundle refusal on bundle2 -> ERROR:ABORT part with 7.2's words; unreadable request -> oob error + end);
  serve::Refusal now Copy. Tests beside getbundle's (they share its store): golden replays the 3 captured sessions whole,
  hostile table, abort part, session end, arbitrary-input property.
- Runs (Studio, own target per arm): red (step stubbed to end at once) 4/5 then 5/5 after the property was strengthened
  (a do-nothing stub never panics; now it must end only on an empty line); green 219/219; merged trees 233/233 and
  254/254 (A-641, A-630 parts 2/3 in crates/hg); fmt clean.
- Merges: three rounds as main moved (A-641, A-630, A-646): research sections renumbered (mine is §22), amendment-log
  ends main-first, GAPS sentence placed before "Owed, in order"; every resolver asserted both sides and was chained.
- Commits: ecfade106 (session), 23ebce614 (property), 1d0562eb1 (docs), merges f58a42526, 1be24563d, f820c1b3a.
- Cleaned: Studio ~/.cache/g079-a619c-*; red worktree g079-a619c-red + branch (local, remote).
- LEFT (main starts it): HTTP version 1 over slates-http (A-595) and the shared edge (A-625, g299's listener) -- NOT
  started, context spent on parts 2 and 3; the CLI process around the session (stdin/stdout through the runtime, sliced
  within the shard's budget); held deltas; obsolete's listkeys dump.
- ENDED.
