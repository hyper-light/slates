# FIXED WAITS LEFT (goal 85), counted 2026-11-18 ~23:05Z after A-470: about 24 (g251 owns 12, g255 owns 12; the
#   reap cadence left the count: it moves to main's event-driven reaper series).
#   Class A: CLOSED (g251's A-432 and A-440, 6859ceb2f).
#   Class B: CLOSED (A-437, A-439, A-442, A-444, A-446, A-449).
#   Class C 12 (g251's): C-a 1 (the claim wait done by A-443, A-447, A-452; CREDIT_WAIT done in fleet.rs by A-454,
#     owed in ack_release.rs, attach_forms.rs, shards.rs), C-b 3, C-c 8.
#   Class D 6 (g255): D-a DONE (A-455); D-b DONE (A-461, A-467); D-c: log_late_beat DONE (A-470), the reap cadence
#     to the event-driven reaper (main's series, piece 1 next with main); left: starved_share_ns (test support), stop_parts
#     (the log writer's drain wait), start_with_fleet (the learning gate), anchor_hold_windows x2, held_copy.
#   On the 10 s placeholder 6 (g255): telemetry horizon, FUSE helper x2, restart cap, status capture, a move's overdue
#     bound; then L-h removes FAILOVER_SLO_NS. Method: one per named bound or call site in this file's lists.
#   Done since the count began (72 at ~20:45Z): A-426, A-431, A-434, A-437, A-439, A-442, A-444, A-446, A-449, A-455,
#     A-461, A-467, A-470 (g255); A-432, A-440, A-443, A-447, A-452, A-454 (g251).
# REWRITTEN 2026-11-18 ~22:20Z (14:2x PST) after the outside cleanup of ~/.cache at 14:04 PST, from this agent's own
# copy: the lists and the plans verbatim as last read; the landed pieces' notes condensed to their facts.

# g255 -- EVERY ITEM THE GOAL STILL NEEDS ([x] on main, [ ] owed). Goal 5875dc0c: every cross-shard and remote wait
# derived, not a fixed second (R3 classes A-D of LIVENESS_BUDGET_NS), leases included.
## Done on main
[x] A-315/A-352 forward lane judged in its own time, never refused below its path floor; A-317 location budget
[x] A-359 W = T by rent or buy; A-363 failover delay F a live output (D,E,R estimators, status); A-405 laptop D = W
[x] A-375 failover-shaped waits read live F (forward bound, location cap, held-reply total, takeover attempts, green_adopt)
[x] A-384 CLI attachment term = min(F, renewal-gap tail + W); Renew verb; fresh term at restart; Hold; `attach --hold`
[x] A-403 class A recovery passes + image alias steps in shard time (call_in_shard_time)
## Class A (cross-shard): CLOSED
[x] A-a (A-408) green_adopt x4, nfs routed, remote_attach end, clone_named_elsewhere x2 -> call_in_shard_time
[x] A-b PART_ROOM_BUDGET_NS (A-426, 4553e8650) and TOLD_WAIT_NS (A-431, 0a4d69e25: learned, RFC 6298 + Karn + back-off
        to the failover delay); work_home's park bound (A-434); g251's 25 sites (A-432, A-435, A-440)
## Class B (remote host): CLOSED
[x] B-a (A-437, cd9a70c55) owner_location locate_round + wait_for_returning_sessions (W): record_exchange_budget + slowest
        asked peer tail, after checking every session borrower's hold bound (forward lane turns, discovery pages)
[x] B-b (A-439, 899aac162) verbs promote_region_here_or_forward (W): the leader forward's bound from the root voters' path tail
[x] B-c (A-442, e9c3bf2d6) nfs probe_back_channel (NFSv4 callback probe) + send_note (W): the client's path tail (its callback RTT)
[x] B-d (A-444, c3dfab016) fleet return_sessions (silence floor) and live_budget (A-200 exchange hard W): the holder path's PTO-based floor
[x] B-e (A-446, 1ae40c678) live_ship DURABLE_WAIT_NS (the durable bound's window = W): state it (judged by silence after a drained receiver)
[x] B-f (A-449, 1d8f265e3) remote_attach sweep_departed (W); daemon SESSION_WAIT_NS (test support = W)
## Class C (client-facing): g251's since ~22:00Z
[~] C-a ipc rendezvous CLAIM_WAIT_NS: done (A-443 Linux/macOS, A-447 Windows, A-452 the macOS region fetch); the fleet
        tests' CREDIT_WAIT in daemon time done in fleet.rs (A-454), owed in ack_release.rs, attach_forms.rs, shards.rs
[ ] C-b slates-api Deadlines::derive (reply = W, reconnect = T + W) and mount.rs MOUNT_CALL_DEADLINE
[ ] C-c shards_route OFFER_LIFE_NS / EXCHANGE_NS; nfs UNMOUNT_CONFIRM_NS + routed; fuse/winfsp open_journal_reader;
        nfs_writeback tracking; verbs await_materialization (each W)
## Class D (supervision): g255
[x] D-a (A-455) watchdog window / spin window (W, judged in shard CPU time since A-228/A-318): each shard's window read at
        every look from its measured overrun (Pulse::scheduler_overrun_ns), within the observation budget, published as
        the health signal shard.watch_window_ns
[x] D-b api role/anchor: start/stop half DONE (A-461, bc6c0866b: a start judged by its silence until its watchdog
        looks, each beat a step, against anchor_window_ns = watch_window_ns(the anchor's lateness p99); on_lapse's
        process judgment on the same window; a stop judged in the daemon's own beating, StopBounds + stop_overdue).
        Second half DONE (A-467, ea26f5d1a): lapse_bound_ns = (W + p99).min(ceiling); observation_gap_ns = bound -
        p99 - beat, at least a beat (OBSERVATION_GAP_PARTS gone). D-b CLOSED.
[ ] D-c daemon reap_loop cadence, log_late_beat, starved_share_ns, stop_parts, start_with_fleet;
        anchor_hold_windows HOLD_BUDGET_NS / RELEASE_ATTEMPT_NS; held_copy reconnect_budget_ns
[ ] D-x (found by A-455) a laptop's failover detection term (A-405: Failover::supervised(LIVENESS_BUDGET_NS)) should
        follow the windows the watchdog now judges by; and OBSERVE_BUDGET_NS's own "ten windows" (A-455's ceiling)
        has no derivation beyond "ample headroom" yet
## Leases and the F placeholder (config.failover_slo_ns = 10 s)
[x] L-a (A-414, a90ba8491) D-16 write lease: renewed with its holding attachment; another principal refused LeaseHeld
[x] L-b (A-414) landing target lease (term = F; keepalive unchanged, half term)
[x] L-c (A-425, d3a96c048) NFSv4 lease + delegation (nfs.rs lease_ns, nfs_state.rs, delegation.rs): an idle Linux client's
        renewal floor (5 s, nfs4renewd.c) + W in whole seconds (6 s), one accessor nfs::v4_lease_ns
[x] L-d (A-414) staging buffers' expiry (verbs StageBegin/StagePut: now + F)
[ ] L-e telemetry LeaseExpiring horizon (follows the lease terms)
[ ] L-f fuse.rs mount helper deadline and unmount reap bound (F placeholder): the helper process's own bound
[ ] L-g anchor RestartPolicy backoff cap (F placeholder; supervise.rs, api role/anchor.rs)
[ ] L-h remove FAILOVER_SLO_NS / with_failover_slo / DaemonConfig::failover_slo_ns once L-a..L-g read live terms
[ ] also on the placeholder: the status capture's deadline (count from the capture, not the request), a move's overdue
        bound
## Regression watch
[x] R-a NOT MINE (A-376/A-428, g232): the A-344 warm-restart break; main ran the A-437/A-435 fleet flows (r242, r277)

# g255-cross-shard-remote -- progress

- Goal id: 5875dc0c4b72886bbcba316abe087fae (kiln goal stack pos 255; UI goal 85)
- Goal text: "Every cross-shard and remote wait is derived, not a fixed second (R3 classes A-D of LIVENESS_BUDGET_NS).
  clients' location class B is ready (agents/clients-timing-final, CI lanes finishing). The forward lane's wall-clock
  stall budget is loadtest-scale's, judged in shard time and capped by the failover SLO."
- Role: implementer subagent (Claude) of the slates supervisor ("main"). THE RULE (Ada, final): one piece, failing
  unit test first (red-checked once), design/GAPS/amendment lines, `cargo test --locked -p <crate> --lib -- <filter>`
  on the Mac Studio (d1323898b255a3ed435eb83fa0f7b0cf, kiln, sync false, a GitHub clone of the pushed branch,
  CARGO_TARGET_DIR under ~/.cache, deleted after) plus `cargo fmt`; merge (fetch, merge origin/main, diff --stat shows
  my files alone, `git push origin HEAD:main` as its own command); END (this file, run-requests.md, a few-line report).
  Never: clippy, suites, e2e, A/B, benchmarks, repeated runs, git checkout/stash/reset, commit-tree, building on the
  MacBook, working in ~/Projects/slates.
- Clone: ~/slates-work/g255-cross-shard-remote. Scratch: ~/.cache/slates-scratch/g255/ (wiped 14:04 PST).

## Landed pieces (condensed; each with its amendment in the design and its GAPS line)
- A-315 (8046bdd0e), A-317 (1a55606cb), A-352 (9cfd75926), A-359 (30cd26392), A-363 (227bc6af1), A-375 (ddf9ec783),
  A-384 (0092e750d; holders bf586915b), A-392 (warm-restart held reply counts from serving_since + F), A-403, A-405,
  A-408, A-414 (a90ba8491), A-425 (d3a96c048), A-426 (4553e8650), A-431 (0a4d69e25), A-434, A-437 (cd9a70c55),
  A-439 (899aac162), A-442 (e9c3bf2d6), A-444 (c3dfab016), A-446 (1ae40c678), A-449 (1d8f265e3), A-455 (this piece).
- A/B baseline branch for main: agents/g255-ab-boot-baseline (5b927ba28, from main ddf9ec783): refresh_failover sets
  failover_ns to the boot sum (lease::horizon_ns + 11 x HEARTBEAT + RECOVERY_BUDGET_NS, ~3.0 s) instead of the live
  estimate; NEVER merge it. Build A = that branch, build B = main; profiles starved + chaos; metrics: false suspicions,
  failover time (kill to served), forwards refused OwnerUnreachable, live sessions dropped.

## A-455 (D-a), 2026-11-18
- Branch agents/g255-watch-window: a464fe9b8 (the rule, the signal, the tests), b84d27cde (liveness.rs bounds),
  b5d62bee0 (test call fixes), the evidence commit, merge 2c870451a (main 2e2770a38..b97d33e90).
- Rule: watchdog::watch_window_ns(overrun) = overrun.min(WINDOW_CEILING_NS).max(LIVENESS_BUDGET_NS), overrun = the
  shard's registry pulse mirror of ShardContext::scheduler_overrun_ns, read at every look; spin_window_ns(cost, window);
  WINDOW_CEILING_NS = OBSERVE_BUDGET_NS (the daemon's own observation gives a shard up there; a wedged shard never waits
  again, so a spike just before its wedge would otherwise blind the watchdog for good). Verdict lateness, log lines and
  the end line name the shard's window. HealthSignal::WatchWindowNs ("shard.watch_window_ns") in the status.
- Evidence (Studio r331, r334, load 3.8-7.3, debug): server lib `watchdog` 27/27; ipc lib `health_signal` 5/5; api lib
  `a_lapse_kills` 1/1; lone_daemon and liveness build (--no-run). Red once: window held at W -> a 2.5 s overrun shard
  judged wedged at 1.0 s and spinning at 1.0 s of CPU. Post-merge check r336 (same results).
- LANDED on main 2026-11-18 ~22:12Z: b97d33e90..2c870451a. amendments.tsv says landed. Run requests filed (A-455's
  suites and the starved lane; and the re-filed A-414..A-449 flows lost in the wipe). Studio scratch and target deleted;
  branch agents/g255-watch-window deleted. ENDED here; main starts the next piece (D-b by this file's order).
- Integration tests changed (main runs them): crates/client/tests/lone_daemon.rs (reads the window the end line names;
  the start line's new words " ms of CPU at least"), crates/server/tests/liveness.rs (a wedged shard's stop held to the
  window read from its pulse while held).

## Plans for what is left (g255)
- D-b, the anchor (crates/api/src/role/anchor.rs): its start judgment (judge_start_by: watch.judge(look,
  LIVENESS_BUDGET_NS, spin_window_ns)), on_lapse/lapse_bound_ns (W + the anchor's own lateness p99), stop_gracefully,
  start_stuck, observe. The anchor runs outside the daemon's runtime: the daemon's live window reaches it only through
  the segment (a supervision word the watchdog writes, e.g. the largest window it judged by at its last look, beside
  SUP_LOOKED at 216; offsets 232-255 are free) or by its own measure of the start's process (its Lateness already is
  one). Decide by A-232's terms; failing test first per site.
- D-c: daemon.rs reap_loop cadence, log_late_beat, starved_share_ns, stop_parts, start_with_fleet;
  anchor_hold_windows HOLD_BUDGET_NS / RELEASE_ATTEMPT_NS; held_copy reconnect_budget_ns. Each: what it waits for, then
  the rule (a shard's own time where it waits on a shard; the window the watchdog judges by where it is supervision).
- D-x: Failover::supervised(LIVENESS_BUDGET_NS) (daemon.rs, A-405) to the live window; OBSERVE_BUDGET_NS's derivation.
- Placeholders: telemetry LeaseExpiring horizon (follows the lease terms: the shortest live term the shard holds?), FUSE
  helper deadline and unmount reap bound (the helper process's own bound), the anchor's restart cap (RestartPolicy,
  supervise.rs and role/anchor.rs: the recovery estimate R?), the status capture's deadline (count from the capture),
  a move's overdue bound; then L-h removes FAILOVER_SLO_NS, with_failover_slo, DaemonConfig::failover_slo_ns, and
  fleet.rs ~5611's test backstop and cli/tests/fs_server.rs's outage sizing that still read it.

## Decisions and orders on record (verbatim in substance)
- Supervisor 2026-11-07: RFC 9002 initial state when no sample; location = membership horizon; owner_location round =
  slowest asked peer tail + record_exchange_budget; SESSION_WAIT = record_exchange_budget.
- Ada 2026-11-18: no boot snapshot; online estimators; the failover delay is an OUTPUT (sum of the estimates) published
  live; every lease and W's floor from the live estimators; the boot derivation kept only as the A/B baseline (a build
  of its own, never a mode switch). F is derived, not Ada's question.
- Coordinator 2026-11-18: W-on-T stands (A-359); each lease consumer gets its own term from what it serves.
- Ada, final (relayed): agents implement, run unit tests, merge, and END.

## A-461 (D-b's start/stop half), 2026-11-18
- Branch agents/g255-anchor-start (deleted): 2508b7555 (the change), the evidence commit, merge bc6c0866b (main
  0049aa171..5e7d7250d). LANDED on main ~22:33Z: 5e7d7250d..bc6c0866b. amendments.tsv says landed.
- Rule: role/anchor.rs anchor_window_ns(lateness) = watchdog::watch_window_ns(lateness.p99_ns()); judge_start_by takes
  (now, memory, beat_ns) and (window, spin); the start phase lasts until SUP_LOOKED (watchdog's first look), each beat a
  step; start_stuck/judge_lapse name the window; stop_gracefully -> StopBounds { lapse_ns: lapse_bound_ns(), window_ns:
  anchor window }; slates-anchor stop_overdue(requested, stop_by, beat, window) = beat > (stop_by or requested) + window.
- Evidence (Studio r365, r372 post-merge, load 3.8-13, debug): api role::anchor 11/11 (1 ignored), anchor stop+backoff
  4/4, anchor and cli suites built --no-run. Red once: beats no step + bare window -> beating start stuck at 1.0 s,
  2.5 s-late anchor's window 1.0 s.
- Test edits main runs: crates/cli/tests/cli.rs (frozen daemon + stuck start read the named window), crates/anchor/tests/
  anchor.rs (StopBounds). Not compiled here: the api's Linux/Windows-only a_start_held_off_its_cpus test (call updated).
- Run requests filed (role liveness_tests, anchor suite, CLI anchor flows + lone_daemon, Linux/Windows compile).
- Slip to report: created the branch with `git checkout -b` (non-destructive, clean tree); use `git switch -c` next.
- ENDED here; main starts the next piece (D-b's rest: the lapse bound and the observation cadence, or D-c).

## A-467 (D-b's second half, closing D-b), 2026-11-18
- Worktree ~/slates-work/g255-lapse (branch agents/g255-lapse-bound, made with `git worktree add -b`, remote deleted):
  0babd7b84 (the change), the evidence commit, merge ea26f5d1a (main 552fe1f0c). LANDED ~22:45Z: 552fe1f0c..ea26f5d1a.
- Rule: Lateness::lapse_bound_ns = (W + p99).min(watchdog::WINDOW_CEILING_NS); Lateness::observation_gap_ns =
  (bound - p99 - HEARTBEAT).max(HEARTBEAT); lapse_wait(watch, beat, now, (budget, gap)). Bug fixed on the way: with the
  anchor's lateness past a window, half the bound + lateness passed the bound at every look, the resume rule took each
  look for a sleep, and a wedged daemon was never found lapsed (replay test).
- Evidence (Studio r386, r392 post-merge, load 5.5-11, debug): api role::anchor 12/12. Red once (no ceiling + half the
  bound): bound 3,601 s for an hour-late reading; the 2W-late replay never found the wedge.
- Run requests filed (role liveness_tests incl. the starved case, the CLI frozen-daemon flow, the anchor suite, clippy).
- ENDED here; main starts the next piece (D-c by this file's order).

## A-470 (D-c part 1), 2026-11-18
- Plan changed mid-piece (main, Ada's question): the reap cadence goes to an event-driven reaper, a series of its own.
  The tick-based draft (a full tick per shard window, early wakes for owed terms floored at an idle turn) is parked,
  unbuilt, on agents/g255-reap-cadence (ba45a5e94), never on main.
- Landed log_late_beat alone: worktree ~/slates-work/g255-beat (branch agents/g255-late-beat, deleted), 99109ac36 +
  evidence 9bc4011f2, LANDED ~23:00Z: 21e017deb..9bc4011f2. daemon::beat_is_late(gap, overrun) = gap >=
  watch_window_ns(overrun) / LATE_BEAT_SHARE; the line names the window. Also docs/bugs/2026-11-18-the-anchor-never-
  found-a-wedged-daemon-once-its-own-waits-ran-a-window-late.md (A-467's bug; sibling: the watchdog's A-308 gap rule).
- Evidence (Studio r411): server lib late_beat + watchdog 28/28; red once (half the bare window). New rule from main:
  clippy -p <crates touched> --all-targets + cargo xtask check before merging: r416/r419 found nothing in A-470's lines;
  the only finding from g255's pieces is A-455's watchdog.rs:337 manual_clamp, which main is fixing.
- ENDED here; main starts piece 1 of the event-driven reaper with g255.

## A-474 (the watchdog's A-308 sibling), 2026-11-18
- Worktree ~/slates-work/g255-gap (branch agents/g255-watch-gap, deleted): 282deb94e (the change), evidence commit,
  merges 9e67b1321 and the A-476 one. LANDED ~23:13Z: 0ef784f19..5ceda4847.
- Rule: watchdog::slept_past_ns(window, watcher_late) = (window + late).min(max(ceiling, window)); A-308's reset only
  past it. Look::watcher_late_ns; watchdog::LatenessTail (last 100, nearest-rank p99, moved from the anchor) kept of
  the watchdog's looks; the anchor's Lateness wraps it and passes its p99 into its start and silent-watchdog looks.
- Evidence (Studio r435, r441): server watchdog 28/28, api role::anchor 12/12; red once (reset at any window gap: never
  judged). clippy (server + api, all targets): nothing in A-474's lines; xtask: none in its files.
- Bug record docs/bugs/2026-11-18-the-watchdog-never-judged-a-wedged-shard-once-its-own-looks-ran-a-window-late.md;
  A-467's record points at it. Runs filed. ENDED; next with main: the reaper's piece 1 (A-475, dead clients by hang-up).

## A-475 (the event-driven reaper's piece 1: a dead client reclaimed on hang-up / process exit), LANDED 2026-11-19 ~00:20Z
- Worktree ~/slates-work/g255-hangup (branch agents/g255-reap-hangup): 076746072 (change), b0e015fc0 (evidence), merges
  9ede900e6 + the A-503 one, a3c9dac08 (test seat's maps_sent). LANDED e2d1bc42b..920b8fc9c, then 2c4c9bd21 (unlinked the
  Linux-only `Ended::Broke` in hangup's module doc: the macOS/Windows doc gates document private items).
- What: crates/server/src/hangup.rs (new): per-platform wait, register-then-test-once; Linux = dup of the control
  socket + EPOLLIN + PEEK (byte = `Ended::Broke`, counted client.control_broken); macOS = ExitWatch kqueue awaited by the
  shard's kqueue (nested kqueue works: measured); Windows = slates_rt::handle_wait (new; RegisterWaitForSingleObject
  ONLYONCE|INWAITTHREAD, callback posts the task word to the shard's IOCP via iocp::post_word; drop =
  UnregisterWaitEx(INVALID_HANDLE_VALUE)). reap.rs: start_watch/watch/hung_up/reaper/retire/owner_window_ns; seat holds
  `watch: Option<TaskId>`, cancelled by end_unproven; ShardState.dead_clients + reaper_task; begin/sweep/peer.rs/
  last_seen_ns gone; staging + status-page expiry moved onto reap_loop's tick; TASKS_PER_CLIENT 2->3,
  LOOP_TASKS_PER_SHARD 5->6; unsafe budget rt 102->106, server 8->5. Counters: client.reaped_by_hangup (non-vacuity),
  client.unwatchable, client.watch_refused, client.watch_cancel_refused, client.control_broken.
- Evidence (Studio, macOS debug): server lib hangup::tests + reap::tests 5/5; merged tree 6/6 with A-492's test (its
  remove_on_owners call now passes `|_| W`). Seen 0.37 ms after a SIGKILL at the watch's 2nd poll; seat back 9.0-9.4 ms.
  Red once: exactly the two guarded tests failed.
- Filed (run-requests.md 00:20Z): Linux and Windows lib runs of the new tests, rt handle_wait tests on Windows, clippy/
  xtask/doc gates on three platforms, the client reap suites (reap, reap_map, the CLI clients_reaped case).
- Left for the reaper series (main starts them): the tick's remaining jobs (staging/status pages, leases, CLI
  attachments, destroys, grants, audit, departed hosts, held copies, pressure, NFS purge, hints+delegation drain, idle
  seal, huge-page re-arm); owed: claim pid not bound to the process's creation (macOS/Windows), SDK heartbeat now unread.
- ENDED here.

## A-522 (reaper piece 2: the claim's pid bound to its process; staging + status-page expiry by their own event), LANDED 2026-11-19 ~00:40Z
- Worktree ~/slates-work/g255-a522 (branch agents/g255-a522, deleted): a15b1a37c (change), evidence commit, merge.
  LANDED ff94f8848..df30ee7f5.
- Part 1: claim slot AT_STARTED (u64 at 80; bootstrap MAGIC SLB4 -> SLB5); client writes ExitWatch::own_start()
  (macOS proc_pidinfo PROC_PIDTBSDINFO start in us; Windows GetProcessTimes creation); Accepted.started; daemon's
  Hangup::of_client(control, pid, started) -> ExitWatch::on_started (macOS: NOTE_EXIT first, then start check, mismatch
  = exited; Windows: on_created). exit_watch::bsd_info shared with mach_claim::parent_of (no new unsafe); Windows
  `open(pid, rights, call)` shared by on/on_created/start_of (ipc unsafe -2). Socket peer tokens rejected: no socket or
  pipe at either claim route (reasons in A-522).
- Part 2: crates/server/src/deadlines.rs: keeper task per shard (LOOP_TASKS_PER_SHARD 6->7) sleeping on the wheel to
  min(staging next_expiry, capture_deadlines first); `noted` wakes it only for an earlier deadline; StatusPages.indexed
  keeps one index entry per seat; forget_seat at end_unproven/finish; tick no longer expires staging/status.
- Evidence (Studio): ipc 6/6, server 9/9 (staging due 100 ms released 105.6 ms, status 101.2 ms); red once: exactly 4
  guarded tests failed. Windows/Linux arms + gates + integration filed (run-requests.md 00:40Z).
- Left on the tick (next pieces): leases, lapsed CLI attachments, destroys, grants, owed audit, departed hosts, held
  copies, pressure hold, NFS purges, base hints + delegation drain, idle seal, huge-page re-arm.
- ENDED here.

## A-536 (reaper piece 3: leases, CLI attachments, destroys off the tick), LANDED 2026-11-19 ~01:05Z
- Worktree ~/slates-work/g255-a536 (branch agents/g255-a536, deleted). LANDED c8731b725..ec57251b0. Commits: 5be4df227
  (leases: keeper sleeps to the lease wheel's next deadline; Partition::take_noted_lease + state::try_with_state closing
  with deadlines::after_borrow), c78f071a1 (CLI terms: Partition::termed_by_expiry index, lapsed_terms, next_term_lapse,
  take_noted_term; expire_lapsed_attachments reads the index), 10b0801c0 (destroys: relinquish_superseded wakes the
  serve loop; refused VolumeDestroyed retried at destroy_retry_at = now + shard window), 29125e66f + 6416876fe (no-spin:
  deadlines::armed_for, `deadline.stuck`; found by job 3's red run, which crashed the test daemon on a keeper spin), docs.
- Evidence: per-job green+red on the Studio; merged tree 20/20 server, 112/112 db.
- Note: main's A-525 (another agent) moved the status capture's deadline to the capture (send_reply calls capture_set);
  merged cleanly with A-536.
- Left for piece 4: grants past their term (landing::end_expired_grants), the owed audit's resend
  (landing::resend_stuck_audit); then departed hosts, held copies, pressure hold, NFS purges, hints + delegation drain,
  idle seal, huge-page re-arm; then delete reap_loop.
- ENDED here.

## A-554 (reaper piece 4: grants, owed audit, departed hosts, held copies off the tick), LANDED 2026-11-19 ~02:00Z
- Restarted g255 (predecessor stopped by a usage limit after making worktree ~/slates-work/g255-a554, no commits).
- LANDED main ac2434f89..8cb45438f. Commits: 3a83ace10 job 1 grants (slates_land::grant::Grants by_term index,
  next_due_ns = expires_ns+1, take_noted via deadlines::after_borrow; deadlines::retry_window_ns names the stuck retry
  window = A-455's shard window, also the destroy retry's); c71850901 job 2 owed audit (landing::arm_owed_audit_look in
  write_audit + shard start, look_at_owed_audit on the keeper, LandingState.audit_look_at; resend_stuck_audit gone);
  80c9aa2b6 job 3 departed hosts (FleetNode::take_members_changed read in after_borrow; look_for_departures at the two
  learn paths + plane peer start; the reaper runs sweep_departed; unsettled -> departures_retry_at on the keeper; no new
  task); 570664b9f job 4 held copies (Linux only: held_copy::next_claim_deadline/owe_reclaim_if_due/reclaim_owed,
  deadlines::claim_due paired cfg, ShardState.held_copies_owed); 15708e5eb evidence; merges c2abf6e93, 8cb45438f.
- Evidence (Mac Studio, debug): red first for jobs 1-3 (r40 on main cf17cae, r68 on c718509); green per job (r45,
  r54, r74, r85) and merged tree c2abf6e (r93): fmt clean, land grant 5/5, cluster fleet 10/10, server 30/30.
  Job 4 never compiled here (Linux-only): filed (run-requests.md ~02:00Z, Linux first).
- Check asked by the brief: deadline.stuck's "a window later" retry WAS derived (A-455 watch window) but unnamed in the
  design; now deadlines::retry_window_ns, named in §4.7's A-554 status and the amendment.
- LEFT for piece 5, in order: 5 pressure hold (event = watermark crossing; hosts' own signals already read polled in
  slates_machine::memory_stall: Linux PSI some (trigger POLLPRI via readiness), macOS kern.memorystatus_vm_pressure_level,
  Windows low-memory notification via slates_rt::handle_wait; first decision: the hold follows the shortfall below the
  boot baseline today, not the host's level), 6 NFS purge, 7 hints + delegation drain, 8 idle seal, 9 huge-page re-arm;
  then delete reap_loop and say so in §4.7.
- Studio scratch ~/.cache/g255-a554 deleted; branch agents/g255-a554 deleted after landing.
- ENDED here.

## A-570 (reaper piece 5, jobs 1-2: pressure at the kernel's reports; NFS purges at their own deadline), LANDED 2026-11-19 ~02:40Z
- LANDED main 87bcb392e..1f712cc28 from worktree ~/slates-work/g255-a570 (branch agents/g255-a570, deleted after).
  Commits: b9a8da83f job 1 (slates_machine::memory_pressure: macOS dispatch DISPATCH_SOURCE_TYPE_MEMORYPRESSURE -> pipe;
  Linux PSI trigger "some 1 2000000" in its own epoll set; Windows low/high notifications; server crate::pressure
  sampler task samples at start + reports; injections ask, the first voids the host baseline; LOOP_TASKS 7->8; machine
  unsafe 86->97; xtask HOST_PATH reason), 6da6056fa job 1 revision at main's review (heartbeat stall look and
  re-sample-while-held WITHDRAWN as a second path; kernel without the event: watch refused by kind, logged, counted
  pressure.watch_refused / pressure.watch_unsupported; Linux stall end = trigger silent past window + window/10,
  UPDATES_PER_WINDOW psi.c), 878895a37 job 2 (nfs::note_purges in record_clients arms nfs_purge_at a retry window on
  while owed; arm_purges_at_start; keeper -> owe_purge_round -> reaper purge_owed_here), dc7697ce4 evidence, merges.
- Evidence (Studio): red on main 8cb6bef (fall not held: Ok(Ok(0)); purge resent 517 ms after its round); green
  878895a (39/39), merged trees 2d7a0ae and 44ba72b: fmt clean, machine 1/1, server 67/67.
- Kiln was down for every session ~02:05-02:25Z (main confirmed); work committed on the branch meanwhile.
- Filed: Linux/Windows arms never compiled (run-requests.md ~02:40Z, Linux and Windows first), gates, integration.
- LEFT for the next piece, in order: base hints + delegation drain (A-470 plan: hints by descriptor readiness; the
  2026-10-08 rejection was a destroy closing the watcher's descriptor under a waiting task: A-149's named readiness
  waits may answer that), the idle seal (aging buckets), the huge-page re-arm (a timer armed only while a placement is
  declined); then delete reap_loop and its tick, and say so in §4.7.
- ENDED here.

## A-570 rest (job: the huge-page re-arm), LANDED 2026-11-19 ~02:55Z; the rest OWED under Ada's new order
- LANDED main 1a42140de..f17565e05 from ~/slates-work/g255-a570b (branch agents/g255-a570b, deleted after): a36c98b16
  (ShardsBackings::remember_decline/take_declined; keeper re-arms at ShardState::placement_rearm_at a retry window after
  a lasting decline; tick's rearm_placement removed; design L1 passages, GAPS, CLAUDE.md example), evidence + GAPS
  docs, merges.
- Evidence (Studio, load 22): red by mutation (keeper re-arm removed, tick's restored): re-armed 484 ms after the
  decline; green a36c98b: fmt clean, server 14/14, re-armed 1.006 s after. Merged trees: the re-arm 1.0004-1.009 s.
- MAIN REGRESSION FOUND (not mine): deadlines::tests::a_status_capture_is_held_the_failover_delay_from_its_capture_
  not_its_request fails on main f24745e (deadlines.rs:773, the capture not let go within half a window) and on my merged
  trees; it passed on a36c98b (based on b7081c388). Entered main between b7081c388 and f24745e. Reported to main.
- OWED (Ada 2026-11-19: goal 81, then 89-92, then 84-86, then the rest; the tick is in "the rest"): base hints with
  their delegation drain (shard-wide watcher set keyed by generational handle), the idle seal + purge (aging buckets on
  the keeper), then delete reap_loop and its tick (no interval, no sweep) and say so in §4.7. Recorded in GAPS.
- ENDED here. Next piece: goal 81.

## Goal 89 part 9ec5603d (kiln SlatesVolumes keeps slates' resync end), 2026-11-19 ~03:05Z: NOT BUILDABLE IN KILN ALONE
- Clone ~/slates-work/g255-kiln-resync (kiln d7f76c1dc). Nothing committed.
- Found: kiln's `sandbox::served::SlatesVolumes::stream` returns `served.stream` and drops `served.resync` (slates
  `ShardsStream::resync`, the DEVICE's end, A-316/A-339, Windows A-427), because Hex's `serve::Servers::stream` returns
  one `Stream` (an OwnedFd/OwnedHandle) and the node hands Shards' run host one `ServedStream`.
- The device that would read the resync end is Shards' own virtio-fs device (Shards links no slates crate). Shards takes
  one stream (`--slates-fs-fd` / `--slates-fs-handle`) and has NO resync support: none in kiln's vendored Shards
  (0.8.0 + D286) nor in Shards origin/main aa819b0585 (git grep: no slates resync anywhere).
- So kiln keeping the end changes nothing for the VM: at a restart the successor would send the fresh stream into an
  end no device reads, and the share still ends. Kiln also sees no run-end signal from Hex's `Servers` to bound holding.
- The chain that fixes it: (1) Shards: `shards run vm` takes the resync end (Unix flag / Windows granted handle) and its
  device implements A-316's resync on Shards' accepted conditions (4a token default-deny, 4b resend the outstanding
  request whole and re-ask DAX descriptors, 4c Windows' handle path) -- Shards' binding policy (AGENTS.md, R01-R16)
  governs it; (2) Hex: `serve::Stream` carries the resync end beside the stream, handed with the run's request
  (`ImageSpec::served`); (3) kiln: `SlatesVolumes` passes `served.resync` through, after re-vendoring Hex and Shards.
- Reported to main; awaiting re-scope.

## Goal 89 kiln core.fileMode piece (relaunch 2026-11-18 ~03:35Z local record): PARKED, committed and pushed
- kiln clone ~/slates-work/g255-kiln-filemode, branch agents/g255-filemode, pushed: remote head 179b845b2 = local head,
  two commits over kiln main d7f76c1dc (4bdb84fc5 the change: git::file_modes reads core.fileMode, a review in a
  core.fileMode=false repository commits no execute bit; 179b845b2 the test's E0521 compile fix). Nothing uncommitted.
  Whether its lib tests passed after the E0521 fix was not recorded by my predecessor: unknown, owed to whoever resumes.
- ~/slates-work/g255-kiln-resync: kiln main d7f76c1dc, clean, nothing committed (the finding is recorded above).

## A-597 (UI goal 92, §4.17.1 shared piece 7: policy and hooks), started 2026-11-18 relaunch
- Worktree ~/slates-work/g255-a597, branch agents/g255-a597 from origin/main e85c17f9f.
- Plan: crates/server/src/wildmatch.rs (git's WM_PATHNAME wildmatch as a bounded DP; t3070 golden vectors; a faithful
  dowild port as its oracle in tests), crates/server/src/vcs_policy.rs (git ref patterns: for-each-ref's literal-or-glob,
  last rule wins as transfer.hideRefs; Mercurial names exact or `*` per branch/bookmark/topic, acl's deny-then-allow;
  Subversion paths literal or :glob:, authz's deepest depth then latest rule; indexed matcher + every-rule oracle),
  crates/server/src/vcs_hooks.rs (points, phases, inputs per tool, HookTimes over LearnedWait with no bound before the
  first finished run, the sans-io runner with typed outcomes). Docs: §4.17.1 status, amendment A-597, GAPS §4.17.
- Evidence read (scratch ~/.local/state/slates-fanout/scratch-g255/): git e1ec09b23 docs+refs.c+ref-filter.c+wildmatch.c+
  ctype.c+t3070+receive-pack.c; Mercurial 7.2.4 acl.py+config.txt (tarball sha256 85839e0f...); Subversion
  fd4e82b00 authz.h/authz.c/authz_info.c/hooks.c; svn 1.10 release notes.
- LANDED 2026-11-19 ~04:45Z: main d9ef6bf9f..fa3605e50. Commits: 1c22ca7e5 (code + tests), 108a220ba (E0499 fix: a
  run's words taken before finish), 14dab8ff0 (rustfmt layout from the Studio's patch, sha256 1ae8cabf...; tests reach
  every RevpropAction and StartRefused, which the test build had warned dead), d75d829e6 (docs), merges 416619570,
  478d7b016, fa3605e50 (each conflict at a document's end, main first, A-597 last). Branch agents/g255-a597 deleted.
- What: crates/server/src/wildmatch.rs (git's WM_PATHNAME glob as tokens + reachable-position sets, no recursion;
  t3070's 190 vectors; `reference` = dowild transcribed, the oracle, also used by vcs_policy's tests),
  vcs_policy.rs (Right/Effect/Who/Rule/Verdict; GitPolicy last-covering-rule-wins with for-each-ref coverage;
  HgPolicy acl order with NotListed; SvnPolicy deepest depth then latest rule; indexes + every-rule oracles),
  vcs_hooks.rs (HookPoint x8 with phase/name/words_from/unconfigured; HookInput builders per tool; HookTimes over
  LearnedWait with samples/cuts counters, no bound before the first finished run, no ceiling of its own; HookRun
  sans-io: Event -> Step {Wait{wake_at_ns}, Kill, Done{outcome, end_leftovers}}; Outcome Accepted/Refused/TimedOut/
  Abandoned/Unstartable; Words capped by the caller). lib.rs mod lines with cfg_attr(not(test), allow(dead_code)).
  Docs: §4.17.1 status + piece 7 note; §4.17.4 item 12 hook row now names the shared estimator (size regression only
  once measured); amendment A-597; GAPS §4.17 paragraph; research/vcs-servers.md §6 (sources with sha256 and lines).
- Evidence (Mac Studio, kiln, debug, load 7-11): slates-server lib wildmatch::/vcs_policy::/vcs_hooks:: 26 of 26 at
  14dab8ff0 and on the merged tree 416619570; fmt --check clean. Red six ways on mutated copies, each caught by its
  named tests (Dirs as Run; first git rule; NotListed as Unruled; svn depth ignored; sample on client-left; no
  back-off). Studio scratch ~/.cache/g255-a597 (5.4 GB) deleted.
- Filed (run-requests.md, 2026-11-19 g255 main fa3605e50): clippy on three platforms, xtask check, doc gates.
- LEFT (owed, design and GAPS say so): the policy in the repository record + the verb that sets it (with A-603's
  register); the branch verbs and push paths asking it; each tool's read-only hook view (git piece 10, hg piece 9, svn
  piece 11's svnlook) and the driver that starts a hook in its own process group over it (Linux: slates exec, §4.6);
  each tool's words for the outcomes (strings verified, in A-597's amendment); review events' hooks; a repository-owned
  queue for observing hooks that must outlive their push (§4.17.3). Not read: Mercurial's config parser on an empty
  allow section; hook.py's order within one priority; apr_fnmatch's POSIX classes (segment globs use git's wildmatch).
- Local evidence copies kept in ~/.local/state/slates-fanout/scratch-g255/ (git/svn/hg sources, a few MB).
- ENDED here; main starts the next piece.

## A-622 (UI goal 93, shared piece 7's rest, in parts), started 2026-11-19 after A-597
- Main's note: the /tmp write was an R1 slip; scratch only under ~/.cache, ~/.local/state or the worktree.
- Worktree ~/slates-work/g255-a622, branch agents/g255-a622.
- PART 1 LANDED ~05:20Z: main f68d32006..6ce9e687d. RepositoryRecord.policy (RepositoryPolicy: version, rules, hooks;
  PolicyNames/Right/Effect/Entry/Who, PolicyRule, HookPointName, HookConfig in db catalog and mirrored in ipc);
  Op::RepositoryPolicySet (tag 97); RequestBody::SetRepositoryPolicy (86); DatabaseRefusal 8/9; DbError
  PolicyChanged/PolicyRuleMalformed/PolicyHookMalformed; guard in crates/db/src/repository/policy.rs; bytes charged to
  the merge budget (state_bytes includes policies); server set_policy (owner only, read+1). Wire orders published by
  hand, checked by both --test wire_order runs. Evidence: db repository:: 15/15, server repository:: 5/5, ipc lib, both
  wire_order 2/2; red first (4 guarding tests failed). Commits 21062a44a, cfb331bdc, docs, merges.
- Slip: once ran `git diff --stat` and `git push` in one command; keep the push its own command.
- Main's lint pass adds `// Format:`/`/// Shape:` notes to test literals too: write them in from the start.
- PART 2 LANDED: main ed604ed99..0b09e3ce9. vcs_policy::admit(record, principal, asked) with Asked/Name/PushRefusal/
  Compiled; unruled = owner's alone; red first (unruled for anyone: the two check-point tests failed); vcs_policy:: 14/14.
- PART 3 LANDED: main a6223d317..7b540d04e. crates/server/src/hook_driver.rs (cfg(unix)): Command with process_group(0),
  std::io::pipe words (both streams or stderr alone), non-blocking stdin/words, waits raced in one poll_fn (words, input,
  ExitWatch, deadline sleep, client future, never re-polled after answering), exit via waitid WNOWAIT so the zombie leader
  keeps the pgid reserved until end_leftovers' kill, reap at the end, Drop kills the group and hands the child to a
  reaper task (hook.reap_handed / hook.reap_waited). vcs_hooks: Learns trait, Lesson, HookRun::with_bound. Tests
  re-invoke the lib test binary as the hook (SLATES_HOOK_ROLE); 42/42 server tests; red three ways. Bug found on the
  way: polling a finished client future panicked (fixed 1f8462e79).
- Studio scratch ~/.cache/g255-a622 (7.3 GB) deleted; branch agents/g255-a622 deleted. Run requests filed (Linux driver
  run first, clippy, xtask, docs).
- LEFT (in design + GAPS): Windows driver (job object, win_spawn); Linux launcher namespace (§4.6); the push pieces'
  calls of admit and run with each tool's view and words; the hook times' table on the owner shard; client/CLI forms of
  SetRepositoryPolicy.
- ENDED here; main starts the next piece.

## A-641 (UI goal 93, §4.17.3 Mercurial piece 5: pushes on the shared pieces) -- NOT STARTED, handed back 2026-11-19
- Main assigned it after A-622 (row A-641 already written by main; next free A-642). This session had carried A-597 and
  A-622's three parts, so its context was too long for a piece this size (g291's progress, §4.17.3 decisions 1, 2 and 6,
  A-618's slates_merge::push, A-603's entry, A-635's multi-work verdict trait, A-514/A-545/A-598's bundle2 parts, and
  golden vectors captured against stock hg 7.2 `hg serve`). Reported to main; main starts it fresh.
- Nothing written for it: no worktree, no branch, no commits.
- Pointers for the fresh start: the push's check point is `vcs_policy::admit(record, principal, &[Asked { name:
  Name::Hg(namespace, name), right }])` (A-622 part 2); the hook driver is `hook_driver::run(Launch { program, input,
  view_root, words_cap }, bound_ns, client_gone) -> (Outcome, Lesson)` with inputs from `vcs_hooks::hg_pretxnchangegroup`
  / `hg_changegroup` (A-597/A-622 part 3; Unix only); the policy and hook configs live in
  `RepositoryRecord::policy` (A-622 part 1); Mercurial's refusal words are recorded in A-597's amendment entry.
- ENDED here.
