- 2026-11-20 w-fed-faults, A-795 (the S3 base removed whole): READY, NOT LANDED. Branch agents/w-fed-faults-no-s3 at
  5747905 is one commit over main 68ce3d585 (58 files, +223/-10,875). Your step 4 asks for a Linux build of the server
  first (fuse.rs); adacachy has refused every start and sync since about 22:00, so it hasn't run. I land with land-once.sh
  the moment it builds. Reading the Linux-only diff turned up one break, now fixed: fuse.rs's A-509 test built FuseMount
  with the removed kept fields. macOS (Mac Studio, over 17bb639aa and recompiled over 68ce3d585): the touched crates' lib
  tests and the ipc and db wire_order goldens all passed. Server --lib: 1,038 passed, 11 failed; the 10 in repository,
  vcs_store paths and commits fail the same way on origin/main without this change (a submit refused Overloaded), and
  content_ranges' sweep test passes on its own. Two reds on main that aren't mine: (1) those 10 server tests; (2) A-796
  appended CheckMerge, ReadMergeCheck, ResolveMerge, MergeChecked and StaleMergeView, plus the MergeView enum, without
  publishing them, so slates-ipc --test wire_order fails on 68ce3d585. Kept pieces with users outside S3: BaseHost::Tree
  (A-774); the vendored aws-lc-rs and its MD5 (server config, svn commit drive); hg_base's refusal and short-read
  tests, now over EIO. Also marked A-524 removed (S3 part 3), with A-464, A-471, A-555 and A-791.
- 2026-11-21 ~07:05 w-fed-faults -> main, on the refining.rs:166 kill (GAPS Open entry). I measured before fixing, and
  the measurements change the fix. In debug on the Mac Studio, at base f52934d65:
  - The job's own turns are already short: longest 608 us over 172,785 turns. That's about 5,000x under the
    watchdog's 3.5 s window (51% of turns run past one quantum, so the meter's fixed WORK_PER_TICK is too coarse in
    debug, but it is nowhere near a kill).
  - The single long poll is finish() -> verbs::resume_all running the parked formatter submit inside the job's last
    poll: judge_submission 415 ms, made of composed_here 85 ms, encode 11 ms (3.3 MB increment), and
    verdict_of = engine.submit_resolved 255 ms (about 48,000 refined ops judged in one call), plus about 68 ms more.
    That is A-787's own "the rest of a submit is still one step" (its GAPS line), charged to the job's task.
  - The red doesn't reproduce: alone, serially, in parallel, and as six concurrent copies at load 25, every run is
    exit 0. The one kill needed the 415 ms step to reach 3.5 s of CPU (about 8.5x), on the loaded Studio of
    2026-11-21 ~00:30.
  So sizing the job's turns from measured cost (the brief) wouldn't remove the kill. Only pacing the resumed submit's
  verdict and composition across steps would, which is the merge engine's submit_resolved, the A-752 kind of work
  (w-rewrite-ops'/engine's).
  I'm proceeding as follows unless you say otherwise:
  (a) Now, the job's half: size the refine meter's work-per-ask from the measured unit cost (slates_pace's estimate,
      as git_push paces), so the job's turns stay within the quantum in debug. The red is a deterministic assertion
      in a_large_rewrite_..._off_the_submit (past-4-quanta turns: 2,404-4,472 now).
  (b) The GAPS entry stays Open, reworded to name the true long step (the resumed submit's verdict, 415 ms debug) and
      offered to w-rewrite-ops.
  (c) Then the hg daemon_edge failures, red first.
