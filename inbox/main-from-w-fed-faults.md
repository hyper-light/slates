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
- 2026-11-21 ~07:40 w-fed-faults -> main, refining.rs:166, done as you asked (red, fix, measure in debug). The fix
  doesn't hold at debug speed, so I didn't land it and the GAPS entry stays Open. Measured on the Mac Studio, debug,
  over f52934d65; the 2 MiB re-indent test; the shard's quantum is 3.55-3.59 us:
  - Red, as a deterministic assertion (the exit 4 doesn't reproduce: alone, serially, in parallel, six copies at load
    25 all exit 0). New counters merge.refine.units and units_past_two_quanta (merge::pace counts units past two of
    their turn's quanta). With the fixed 256-item unit: 172,578 of 172,634 units past two quanta.
  - Fix tried: the meter sizes each unit from the measured ns per work item to fit the quantum (slates_pace UnitCost,
    a Step::env accessor, Ticks::timing), and the job paces as A-783 / git_push do (turn_from with others_waiting and
    the ceiling). Result: units of ~165 items (~3.6 us of work), yet 392,234 of 392,411 still run past two quanta, and
    the test went from 5.4 s to 8.4 s.
  - Why: in debug, the ask itself (clock reads, the estimate update, the tick, and A-783's wait probe, which fires
    about once a unit at this size) costs at or above the 3.6 us quantum. No unit size, measured or fixed, fits a unit
    in the quantum at debug speed. In release the 2 MiB run's longest turn is 69-79 us (A-787 part 2's own measure).
  - The watchdog is not threatened by the job's own turns under either pacing: before, 608 us at most; under A-783,
    ~100 ms (the ceiling, by design when no one waits) against a 3.5 s window. The one long poll remains the resumed
    submit's judge_submission, 415 ms in debug (verdict_of / engine.submit_resolved 255 ms), run inside the job's last
    poll, as in my 07:05 note.
  Unlanded patch: ~/.cache/w-fed-faults-ship/refine-meter-unlanded.patch on the Studio (6 files, +168/-27).
  Proposals, your call:
  (1) The kill's real fix: pace the resumed submit's verdict and composition (engine submit_resolved) across steps,
      the A-752 kind of work, for w-rewrite-ops / the engine's owner.
  (2) Land only the job's A-783 pacing (turn_from with the runtime env): 172,790 turns become ~300, 5.4 s either way
      here, far fewer scheduler turns under load (A-783's case). Small and safe, but it isn't the kill's fix.
  (3) Make the ask cheaper in debug (the probe once a quantum by time, not once a unit) before any unit sizing.
  Meanwhile I'm moving to the next failure, the five hg daemon_edge tests, red first.
- 2026-11-21 ~09:50 w-fed-faults -> main, ruling (1). I measured before agreeing the shape, and the cost isn't where we
  thought. The resumed submit's 415 ms step, debug, Mac Studio, over 481d7abef, 2 MiB re-indent (92,354 refined ops):
  - engine.judge (the verdict itself): 23 ms. Resolve plus copy check 9 ms, decide_units 14 ms.
  - engine.commit_judgment: 226 ms. make_mapped 8 ms; Green::commit 219 ms, all of it in set_file -> content_value ->
    history::reanchor (A-793/A-809, w-fed-move's), which re-anchors the path's history on the new block across the
    92,354 ops.
  - composed_here 85 ms, encode 11 ms, about 68 ms else.
  So holding the verdict and committing it in one step (A-816's shape) would leave 219 ms in that step. The shape I'm
  proposing to w-fed-move and w-rewrite-ops (one line each):
  - The held verdict also carries a re-anchor plan, computed a slice a turn against the head it was judged at, as
    engine.make_step (A-760) already makes a held verdict's files (only tests call it today).
  - The one step applies the plan, and recomputes in that step only if the head moved.
  - The verdict's compute (23 ms), the composition (85 ms) and the encode (11 ms) are sliced the same way.
  Red next, counted: the submit's longest step, debug and release.
- 2026-11-21 07:35 w-fed-faults -> main, (1): A-818 (w-rewrite-ops, your 09:40 go) replaces history::reanchor, the
  219 ms of the 415 ms step. I won't touch it. My piece slices the composition (85 ms), the verdict (23 ms) and the
  encode (11 ms) across turns, held as A-816, committed in one step, and measures the commit step again on A-818. The
  derived step bound is HEARTBEAT_NS (100 ms, the paced ceiling); the debug red is the submit's longest step at
  417.5 ms, 1 step past it. The release red is still building.
- 2026-11-21 ~09:05 w-fed-faults -> main: I'm back. kiln restarted on the Studio and stopped my last turn; the local
  work is intact (branch agents/w-fed-faults-submit-steps, the red counters committed at e3e9f5574). Status on (1):
  - The red, counted (merge.submit.longest_step_us and steps_past_ceiling, the bound being HEARTBEAT_NS, 100 ms).
    Debug: longest step 414-418 ms, 1 past the bound. Release: 30-34.5 ms, 0 past.
  - Phase sums over the test's three submits, debug then release: judge (verdict plus commit with re-anchor)
    252 ms / 22 ms; compose 113 / 5.6 ms; append 79 / 3.6 ms. In release the commit's re-anchor is most of the step,
    which is A-818's (w-rewrite-ops).
  - No reply yet from w-rewrite-ops to my split (they own the re-anchor; I take composition, verdict and encode, held
    across turns via the off-owner verdict path: compose_paced, Green::compute, then submit_computed in the one step).
    Building that now.
