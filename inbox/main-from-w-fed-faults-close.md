
## C2 closed (goal 85c): live_chaos kills a writing client mid-write, and the next owner keeps or drops the write whole
- Commit on slates origin/main: c733506 (GAPS records the run; the flow itself was already on main, never run).
- Test: `SLATES_TEST_CLI=1 cargo test -p slates-cli --test live_chaos -- --exact
  a_client_killed_mid_write_leaves_its_write_whole_or_absent_through_a_takeover`, macOS arm64, Mac Studio, over slates
  main d4a5f9f43: 1 passed in 9.3 s. The client's write reached the owner and parked there (`fleet.live.awaited` moved)
  before SIGKILL; the owner's node was then lost; the successor served 2.6 s later with every acknowledged write whole
  and the killed client's absent or whole (NoLoss), its first head 21 past the last durable 19 under epoch 2
  (Continuity), every survivor reading its own bytes (ReadSafety). No code change needed.

## C1 closed (goal 91): the multi-runner test with injected faults
- Commits on kiln origin/main: 30c5929 (A-580 harness), e33c470 (A-708 relay, bind), 0edd8dc + e3e5d7b (A-734: one
  fleet, kiln founds its groups once), ed8991d (A-750: workspaces live-shipped and shared with every member), 5bd0de7
  (flows on macOS: runners and mounts let go before a signal), 648c571 + 4f19751 (headroom off; starts asked again),
  73712cc (the move-cut flow over the relay). Slates origin/main: 51a061d + 8d0e137 (A-764: credentials are fleet state,
  with tombstones), a5140de (fleet.rs:4819).
- Test: `SLATES_FLEET_FAULTS=1 cargo test -p kiln --test slates_fleet_faults -- --exact <flow> --nocapture
  --test-threads 1`, three kiln runners of a test's own server forming kiln's Slates fleet at f = 1, the agent's
  workspace live-shipped and shared by kiln alone. Every flow PASSED on macOS arm64 (Mac Studio, local):
  killed 43.5 s, partitioned 46.2 s, restarted 32.6 s (base 2f589baa6); runner cut off 35.0 s (base ed8991d); slow
  lossy link 43.3 s, its move refused by kind unplaced (base e3e5d7b); move whose target is cut off midway 84.2 s, refused
  by kind not_a_member (base eb7194f00). Each: every acknowledged file served byte for byte through the survivors during
  the fault and by every node after, the move moved or refused by a W7 kind, one fleet re-formed.
- Linux x86_64 (adacachy): killed PASSED 332 s, restarted PASSED 132 s (base 715063153), partitioned PASSED 128.5 s (base
  c0f576eb3). The link flows and the relayed move-cut on Linux are main's lane (run-requests.md); adacachy swapped
  104 GB of 122 GB today, so a kiln build there took over an hour.
# w-fed-faults close note (2026-11-21)

## A-795: the S3 base removed from slates whole: LANDED
- Commit: slates 6f29c8d on origin/main (applied over 72576cb; `git merge-base --is-ancestor` confirmed).
  58 files, +223/-10,875.
- Removed:
  - crates/base: the s3 module and BaseHost::S3, plus the dependencies only S3 used (aws-lc-rs, rustls, slates-rt,
    slates-transport; dev proptest, slates-test-seeds, rcgen).
  - crates/vfs: HostError::Pending, FetchId, identify_dir and BasePending.
  - crates/server: CreateOverS3's path (open_s3_base, s3_client_of, the driver's start) and the pending-base parking
    (base_wait.rs, the NFS JUKEBOX wait, the FUSE kept requests); bridge-fuse's dispatch_kept, allocation_running and
    Dispatched::request.
  - crates/client: create_over_s3. crates/api: the S3 names (CONTRACT 58).
  - crates/server/tests/s3_base.rs and every S3 unit test.
- A local base never pends: only S3Host ever answered Pending.
- Wire: RequestBody 97 is RemovedCreateOverS3, refused Unsupported by kind. ImportFault 7 is RemovedPending, which no
  daemon sends. BaseRecord 3 is RemovedS3, refused at a clone and holding a recovery BaseUnavailable. The ipc and db
  wire_order goldens say so.
- Kept, because something other than S3 uses them:
  - BaseHost::Tree (A-774).
  - The vendored aws-lc-rs and its MD5 (the server's config digests, the svn commit drive).
  - hg_base's refusal and short-read tests, now over EIO.
- Docs:
  - SLATES_DESIGN: A-795, and §4.15's "An S3 base" replaced by its removal.
  - A-464, A-471, A-555 and A-791 marked removed, plus A-524 (S3's part 3).
  - GAPS: the S3 entry removed, and a line that Mantle's CP-T10.5 ask is declined.
- Tests, macOS arm64 (Mac Studio):
  - The lib tests of base, vfs, ipc, db, api, client, bridge-nfs, bridge-fuse, land, hg, mcp and http-edge, plus the ipc
    and db wire_order goldens: 14 runs, all passed (over 17bb639aa; recompiled over 68ce3d585 and d2ad10da4).
  - slates-server --lib: every failure fails the same way on main (see below).
- Linux: the Linux-only fuse.rs and channel.rs changes were read, not compiled.
  - Both Linux Rust targets are installed, but there's no Linux C cross-compiler or sysroot: zstd-sys, blake3 and
    aws-lc-sys stop. adacachy refused every start.
  - Reading the diff found one Linux-only break, fixed before landing: fuse.rs's A-509 test built FuseMount with the
    removed kept fields.
  - The Linux clippy and FUSE tests are filed in run-requests for w-lint.

## Bisect: the 10 server tests failing "a submit refused Overloaded": FIXED
- Good base: f98d99685 passes all 30. f9c113ae9 fails 10, not 4. Parent 81d8baea5 passes 30 of 30.
- First bad commit: 2a498b90f, "server, merge: a whole-file rewrite is refined off the verb, a slice a turn (close R3,
  A-787 part 2)", from w-rewrite-ops. Its list was closed at 19:05 and it has been quiet since, so I fixed it.
- Cause: the commit parks a local submit or rebase whose journal replaces a file whole behind a refinement job, and
  park_on_refinement refuses Overloaded outside a recorded request (no route to answer on). So every whole-file
  rewrite from a bare dispatch was refused, whatever its size.
- Fix (slates 6459374): parkable_refinement. A verb outside a recorded request is no longer parked: it composes
  unrefined, as a remote edit-and-submit does, and is answered now. Verbs in a recorded request park as before.
  t_a664_live's submit now appends to README instead of rewriting it, so its resumed run isn't parked again behind a
  refinement a synchronous audit can't drive.
- Red: the 10 on origin/main 8cb10f986. Green: repository, vcs_store paths and commits, and refining: 31 of 31 (again
  over 89e37868f). slates-server --lib: 1,049 passed, 5 failed (below).

## Found and left open (written to GAPS; every one fails the same way on 8cb10f986)
1. green_post_state ends exit 4: the watchdog kills a shard spinning 3.5 s of CPU in A-787's refining job (task
   spawned at refining.rs:166), in a debug build on the Mac Studio.
2. Five Mercurial vcs_http::daemon_edge lib tests fail on macOS arm64. Not looked into.
3. A-796's CheckMerge, ReadMergeCheck, ResolveMerge, MergeChecked, StaleMergeView and the MergeView enum are appended
   but not published, so slates-ipc --test wire_order is red on main.

My list is done.
