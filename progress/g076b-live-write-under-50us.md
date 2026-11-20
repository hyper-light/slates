# g076b -- progress (REWRITTEN 2026-11-19 ~00:55Z from my own context; the old file was lost in the 14:04 PST wipe)

Role: implementer subagent of main. THE RULE (Ada, final): one piece; `cargo test -p <crate> --lib <filter>` + `cargo fmt`
on the Mac Studio (d1323898, sync false, GitHub clone of the pushed branch, CARGO_TARGET_DIR under ~/.cache/g076b, deleted
after) is the only compiling I do; merge (fetch, merge origin/main, diff --stat my files alone, push HEAD:main as its own
command); END (this file, run-requests.md, a 3-line report). Never checkout/switch/stash/reset/force/commit-tree; never
~/Projects/slates; never build on the MacBook.

## Piece A-625 (UI goal 93, shared HTTP edge: git piece 8's edge), 2026-11-19 (g076b): PART 1 LANDED, part 2 owed
- PART 1 LANDED main b524f0440..01f0b6be5 (~05:25Z). Worktree ~/slates-work/g076b-a625 (branch agents/g076b-a625).
- Commits: 9bbf85a1b (red: new crate crates/http-edge with stubs + 31 tests; slates-http's two clippy findings fixed:
  tests/body.rs read_in -> typed Failed, tests/head.rs:47 one whole Head; Status 500 + 503), b656845e1 (engine + reply::bare),
  2c928f46b (docs); merges 8c6fa6a31 (main's 6246b75f3 budget rows/escaped tags), 01f0b6be5.
- What: slates-http-edge (pure, no socket): connection.rs (Connection::next state machine: Write/Read/Decide/Body/BodyEnd/
  Reply/CloseWrite/Drain/Close; Decision Serve/Refuse; Phase; Ended), tls.rs (ServingIdentity per-connection rustls config:
  TLS1.3+1.2, ALPN http/1.1, no client auth, no tickets/session cache; HttpsSession), credential.rs (Basic strict base64,
  TLS only; Bearer b64token; challenges; same_secret; Debug hides secrets). Staged close per A-183; keep-alive on bodiless
  refusals; 100-continue only when the client waits.
- Evidence (Studio, debug): red (29/31 failed on stubs); green b656845e1 (edge 31/31, http 52/52); merged 2c928f46b same,
  fmt --check clean, lock unchanged.
- PART 2 OWED (main starts it): crates/server/src/vcs_http.rs: the listener on the runtime's TCP (MCP's serve pattern:
  shard-kept state, admission bound -> 503 staged, spawn_child+detach per connection), the per-connection driver of
  slates_http_edge::Connection, each Phase's wait learned with crate::learned_wait::LearnedWait (handshake, head arrival,
  idle keep-alive, body stall, drain), connections and request room derived from the shard budget, refusal counters;
  git's site: slates_git::http::route + an authenticator seam (credential -> slates_db Principal; a token minted at start
  for the live run, like MCP's bearer) + the policy decision (vcs_policy, A-597/A-622) + upload-pack/receive-pack services
  (A-620/A-624) behind a streaming service seam; slates-git needs `refusal_parts`/`reply_fields` helpers (fields as data)
  so the site builds Decisions; then the MCP edge onto slates-http.
- Scratch ~/.cache/g076b-a625 (635 MB) deleted.

## Piece A-595 (UI goal 92 51cef9f7, git piece 8 part 1: smart HTTP's request/response codec + git's routes), 2026-11-19 (relaunched g076b): LANDED
- LANDED main bceebaea8..4c79dabb7 (~04:50Z). Worktree ~/slates-work/g076b-a595 (branch agents/g076b-a595).
- Commits: 56146c543 (red: slates-http stubs + all tests; git routes stubs + tests), 70948a3f4 (red compiles; lock as
  cargo resolves A-607), 7ece429be (implementation), c5b2ad86e (fmt), 6c6dd7132 (golden vectors + design/GAPS/amendment),
  ffdd2a2da (fmt), 2dcc07210 (golden: 1401 wants of 1400 commits, HEAD's tip twice), docs results, 861d8aa2f (drop the
  dev-dep flate2: a normal dep since A-608); merges cc2227298, 06231d765, 7903354c6, merge4, 4c79dabb7.
- What: crates/http (slates-http, NEW crate, shared with hg/svn): head (resumable line scanner, MAX_LINE 8000 per RFC 9112
  §3, head bytes + field index charged; origin/absolute/asterisk; strict framing per RFC 9112 §6.3; gzip-only 415; Expect
  417; fixed refusal order), body (streaming Content-Length/chunked state machine, data as input slices, trailers charged),
  gzip (flate2 write::MultiGzDecoder over A-607 zlib-rs; every inflated byte charged; never written after finish),
  percent, reply. slates-git::http: http-backend's routes/statuses/no-cache trio/order of checks; Git-Protocol greatest
  version (protocol.c; the old codec took the last: a bug fixed test-first); `# service=` prelude v0/v1 only.
  Golden: crates/git/fixtures/http (make-http-fixtures.sh + cgi-host.py; recorded on the MacBook with git 2.50.1 Apple
  Git-155, as A-600's were; heads + inflated bodies only per main's A-607 rule; SHA256SUMS checked by the test).
- Evidence (Studio, debug): red r513 (slates-http 49/51 failed on stubs, git http 11/11 failed); green 7ece429be
  (http 51/51, git 87/87); golden 2dcc07210 (git 92/92); final 861d8aa2f merged with main (http 51/51, git 178/178,
  fmt --check clean, lock unchanged). Formatting done with the pinned 1.98.1 rustfmt locally, each file's SHA-256
  identical to cargo fmt's on the Studio (no compile on the MacBook).
- Studio scratch ~/.cache/g076b-a595 (848 MB) and the stale ~/.cache/g076b-a580 (17 GB) deleted 2026-11-19.
- Run requests filed (clippy/xtask/lint matrix over slates-http + slates-git; cross-target incl. Windows).
- LEFT (main starts each): piece 8's next part, the edge (listeners on a shard, TLS, auth 401/403, 100 Continue, service
  bodies streamed to upload-pack/receive-pack, chunked replies, RFC 9112 §9.6 staged close); the MCP edge onto
  slates-http; hg's HTTP v1 (A-569) and svn's DAV reading requests through slates-http.
- A-580 (goal 90) re-checked 2026-11-19: fd6f5fce8 is on kiln origin/main, clone clean, nothing to commit; PARKED.

## Goal 81's live-write part: DROPPED 2026-11-19 ~03:00Z (Ada via main: load and performance work wait until everything
is implemented and merged). Nothing was started for it (docs read only), so nothing is parked on a branch.

## Piece A-580 (UI goal 90 part 60178ceb, the multi-runner test with injected faults; kiln), 2026-11-19: LANDED, PARKED
- LANDED in kiln main d698ebdeb..fd6f5fce8 (~03:12Z). Clone ~/slates-work/g076b-a580-kiln (branch agents/g076b-a580; made
  with `git switch -c` in a fresh clone, nothing uncommitted: a procedural slip against the preamble's rule, noted).
- g225's start: none pushed (no agents/g225*a580 in slates or kiln); its local clone ~/slates-work/g225-a580-kiln was being
  deleted by someone else while I looked; left alone.
- What: `kiln::workspace::slates_faults` (judge_move/refusal_kind, lost, not_reformed) + `slates_move::KINDS` with a
  drift test; docs/agentic.md W7 "The multi-runner test with injected faults"; tests/slates_fleet_faults.rs (four flows:
  killed, partitioned, restarted, a move's target cut off midway; Linux only, since a macOS workspace is a kernel NFS mount
  and a stopped/killed daemon under it has panicked the Mac; gated SLATES_FLEET_FAULTS=1; waits read the fleet's own
  failover delay from daemon_status). Not A-583's tests (g230).
- Evidence (Studio, debug): red r375 (3 verdict tests failed on stubs); green r398 (7/7 over slates_faults, slates_move).
- Run requests filed (the integration test was never compiled; Linux).
- LEFT (parked): main's compile/run of the integration test and the fixes it sends back; macOS flows once a runner lets its
  workspace mounts go before a fault; one-link cut (needs kiln's manifest to name Slates' `bind`, A-277); cross-machine
  runners via `kiln run --on`.

## Piece A-574 (UI goal 86: a laptop's failover detection term derived; R8), 2026-11-19 (g076b): LANDED
- LANDED main 8b0a9a57e..846b1271e (~02:50Z). Worktree ~/slates-work/g076b-a574 (branch agents/g076b-a574; the red
  commit also on agents/g076b-a574-red).
- Rule: where no peer detector judges a node, D = the watchdog's widest shard window (A-455) + the anchor's lapse bound
  (A-467), which the anchor publishes in SUP_LAPSE_BOUND (offset 232, no layout bump; api role/anchor.rs
  publish_lapse_bound); read at every beat on every node (daemon heartbeat_loop -> fleet::refresh_supervision ->
  fleet::publish_failover, the coordinator's refresh_failover publishing through the same fn) and at boot (init_shard);
  the beat fans the delay (fleet::fan_failover, counters fleet.failover_fan.sent/.refused); the configuration Fan no
  longer carries it; Failover::supervised deleted; watchdog::widest_window_ns shared with A-552's stop. R8 test in
  cluster (one sum at f=0 and f=1). A laptop's F at rest ~3 s (was 2 s); docs/cli.md updated; CLI formation allow-list
  names fleet.failover_fan.sent.
- Commits: da2c2d30e (red tests), 4add1161d (change + docs), ee1f9f1d5 (evidence), merges 714f2ce20, 832bd575f,
  7ef9bf73e, b37a8b663, 418f17155, 846b1271e.
- Evidence (Studio, debug): red r256 (supervision `Term { ns: 1000000000, samples: 0 }` vs 2.2 s and a reading; the
  laptop daemon's delay never held the lapse in 10 s); green r256 (714f2ce20) and r308 (b37a8b663, merged with main):
  cluster failover 12/12, anchor 40/40, server 34/34, api role::anchor 13/13 (1 ignored), fmt clean.
- kiln was unreachable for this session ~02:12-02:26Z ("not connected to the kiln server"); a background `kiln run --
  true` loop waited it out. r237 was lost by kiln (never ran); r256 reran it.
- Run requests filed 2026-11-19T02:50Z (anchor/CLI flows with a real anchor, fleet formation, laptop-F-timed suites,
  lint matrix).
- Studio scratch ~/.cache/g076b-a574 (11 GB) and ~/.cache/g076b-a552 (7.5 GB) deleted ~02:52Z.
- LEFT: nothing of A-574. Class D on W: only the reap loop's tick (g255's reaper series).

## Piece A-552 (UI goal 86, part "D-c, the 10 s placeholder readers, and L-h": D-c), 2026-11-19 (restarted g076b)
- Clone ~/slates-work/g076b-a552 (branch agents/g076b-a552; the predecessor's uncommitted drafts of the log and gate
  red tests were finished, not discarded). Merged origin/main a613562eb first (fast-forward).
- Commits: faf5b5f93 (five red tests), cd43525f3 (1/5 starved_share_ns: half the shard's own window,
  watchdog::shard_window_ns/own_window_ns), a3f39f4e7 (2/5 LogWriter::stop waits while lines are written, gives up a
  window after the last; stop_parts uses the widest shard window), 077434a3a (3/5 learning gate: woken at each
  learning's end, gives up after the waiting shard's own window with none ending, parks a beat between looks;
  Gate::new(threads, window fn, look)), 3ffbdd103 (4/5 Windows hold channel: crate::anchor_answers over learned_wait,
  backed off to F; HOLD_BUDGET_NS/RELEASE_ATTEMPT_NS/RELEASE_BOUND_NS deleted; release_bound_ns()), bde798e3b (5/5
  held_copy claim owed for the failover delay the shard holds: direct_map::claim_overdue; reconnect_budget_ns deleted),
  5d81de0d1 (three failover_slo test names renamed to failover_delay; durable_bound wording).
- Red (Studio kiln r53, 2026-11-19 01:41-01:43Z, load 13.5, debug): all five failed for their reason (log gave up
  after 601 ms; gate EAGAIN after 601 ms; starved 500 ms vs 1.25 s; anchor_answers 2 s vs 1 s; claim overdue at 2.5 W).
- Green (Studio, debug): r87 (01:52Z, load 21: machine log 3/3, base own_mounts 4/4, server 40/40, bridge-core
  durable_bound 8/8, fmt clean); after each merge of main r100 (01:55Z), r115 (02:00Z, server 49/49 with deadlines) and a
  last one at 02:03Z (server 51/51 with reader_patience), all green, fmt clean.
- Merges: ce152b2f6, 95a5d1544 (A-554 had moved the held-copy reclaim to the deadline keeper: kept, with
  `next_claim_deadline` = earliest taken back + the delay held at each pass via `direct_map::claim_due_ns`, and A-554's
  Linux test entering its map taken back a delay before due), b042a5ded + 5127b7ab0 (learned_wait's users list),
  ed648369b, b1549a81e. LANDED main 87c1be22e..b1549a81e (2026-11-19 ~02:10Z). D-c closed.
- Run requests filed 2026-11-19T02:10Z (Windows xwin check + anchor_hold pipe test + WinFsp/Shards suites; Linux check +
  held_copy lib test + pmem flows; gate restart flows; stop/starve suites; lint matrix; renamed fleet tests).
- Uncompiled by me (filed for main): anchor_hold_windows.rs + winfsp.rs (Windows), held_copy.rs + shards/dax_hold.rs
  (Linux), crates/server/tests/fleet.rs renames, crates/cli/tests/pmem.rs doc.
- LEFT (main starts each): nothing of D-c. Class D still on W: the reap loop's tick (the reaper series, g255) and a
  laptop's failover detection term (A-405's supervision window).
- Studio scratch ~/.cache/g076b-a552 deleted ~02:52Z (kiln had refused the first delete at ~02:12Z).

## Piece A-525 (UI goal 86, part 01a1f1208ef1724686759630ca531a9b "D-c, the 10 s placeholder readers, and L-h"), 2026-11-18/19
- Worktree ~/slates-work/g076b-a525 (branch agents/g076b-a525, `git worktree add -b` from origin/main d63e602a7).
- Commits: 6ffc4fcc1 (three red tests), 49806b403 (the telemetry red test's imports), 21155acdb (the change + docs),
  merges 0a2e2d19c, c7515aea8, 32e55c5bd. LANDED on main bf21a05b4..32e55c5bd (2026-11-19 ~00:50Z).
- What: `moves::outgoing` overdue bound = `ShardState::failover_ns` (live F = D + E + R, A-363); `telemetry::drain_verb`
  freshness horizon = `ShardState::failover_ns`; `role::anchor::restart_policy(config)` = `RestartPolicy::derive(
  config.nfs_v4.lease_ns)` (the NFSv4 lease the daemon grants, A-425), supervise.rs's derive/docs/tests renamed to the
  lease; cli/tests/cli.rs and server/tests/common/supervised.rs mirror the lease. Design: §4.4 A-363 status, A-525
  amendment; GAPS failover entry.
- Evidence (Studio, debug): red once (api cap 10 s vs 6 s lease; move overdue Some(false) at 3 windows with F = 2 windows;
  chokepoint fresh at 3 windows); green: server moves:: + telemetry:: 11/11, anchor backoff_tests 3/3, api role::anchor
  13/13 (1 ignored), fmt clean; same after merging main (0a2e2d19c).
- Run requests filed 2026-11-19T00:55Z (clippy/xtask on the three crates; anchor + CLI anchor flows; the supervised server
  tests; fleet move tests).
- (Part 1's "left" list is done by part 2 below, except D-c.) Was: the FUSE helper bounds; the status capture; then L-h (remove FAILOVER_SLO_NS, with_failover_slo, DaemonConfig::failover_slo_ns, the nfs.rs unit test
  that sets it, and the integration tests sizing outages by it: fleet.rs ~6580/~13471/~28345/~28423, fs_server.rs ~1730,
  live_chaos.rs ~5366). D-c (daemon.rs starved_share_ns, stop_parts's log-writer drain wait (needs a sink seam in
  slates-machine's LogWriter to unit-test; rule: wait while the writer drains, give up after a window without progress),
  start_with_fleet's own-mounts gate (LIVENESS_BUDGET_NS), anchor_hold_windows HOLD_BUDGET_NS/RELEASE_ATTEMPT_NS (Windows),
  held_copy reconnect_budget_ns (Linux)).

## Piece A-525 part 2 (same UI part), 2026-11-19 ~02:10Z: LANDED main 3cc9ce3f0..3c9a37dd2
- Commits (worktree ~/slates-work/g076b-a525, branch agents/g076b-a525): c6258be35 (two red tests: helper_runs with the
  placeholder; the status capture held from its capture), 5d9b98c38 (the change + L-h + docs), 5fb752585 (the helper
  test's delay shape: 3 windows), merges e8f8deb04, 50e7e2bd1, 3c9a37dd2.
- What: crate::helper_runs (cfg-free HelperRuns over learned_wait::LearnedWait; bound_ns(F) = tail + W, Karn, back-off
  capped at F) read by fuse.rs's defer_attach/mount_without_blocking/reap (Linux; ShardState::fuse_helpers);
  StatusPages::begin sets no deadline, capture takes (capacity, deadline), deadline() for the index; send_reply sets
  now + F at the capture and enters it in A-522's index; status_on_channel/send_reply pub(crate) for tests. L-h:
  config::FAILOVER_SLO_NS, DaemonConfig::failover_slo_ns, with_failover_slo deleted; nfs lease test holds F short;
  fs_server link cut and live_chaos partitions read `fleet_failover: total_ns=` from status (live_chaos::live_failover,
  PartitionRun::failover, judge_stale_writes(held_bound)); fleet.rs burst backstop from Daemon::failover_ns; test
  locals renamed (failover_ns, SHORT_FAILOVER_NS); anchor test LEASE_NS.
- Evidence (Studio, debug): red once (10 s vs 1 s; capture not let go within half a window); green server 20/20, api
  role::anchor 13/13, fmt clean; same after merging main. Filed: Linux fuse:: + helper_runs:: lib tests (uncompiled by
  me), clippy/xtask, the edited integration tests (anchor, fs_server, live_chaos, fleet, CLI FUSE flows).
- LEFT (main starts each): D-c as its own piece (daemon.rs starved_share_ns, stop_parts's log-writer drain wait,
  start_with_fleet's own-mounts gate; anchor_hold_windows HOLD_BUDGET_NS/RELEASE_ATTEMPT_NS (Windows); held_copy
  reconnect_budget_ns (Linux)). Test names still saying failover_slo (durable_bound.rs, two fleet tests) are names only.

## Earlier: goal 80 (kiln 01a1af515aa6705aac9a1560bc1c22e8, "a live write adds under 50 us at the median, 1 ms p999")
- LANDED on main 213de0284 (2026-11-18 ~21:00Z): A-396 live holds (owner answer hold, control exchange + dispatch-stream
  holds, holder entry-stream hold; windows learned by RFC 9002 §5.3, stream holds by A-326's run rule; asked of the
  runtime and kept only under A-387's measured predicate; counters fleet.live.hold.{answer,exchange,dispatches,entries,
  declined}); also the hop counters fleet.live.hop.* (7aea4d736). Landed BEFORE its A/B under Ada's rule.
- Owed by main (re-filed 2026-11-19T00:55Z): the held-loop fleet test (Linux, macOS) and the A/B 7e3b4c9fb vs 213de0284
  (adacachy capped unit with nr_throttled, Studio), which decides whether the holds stay and gives the hop breakdown.
- Not built (main starts them after goals 84-90): arm B (answer holds only) if the A/B asks; the build off the critical
  path (install image after the write's own dispatch; body_keys unwraps tenant+lineage twice per entry, 5 AES-KW unwraps
  where 3 do; the bounded per-shard key cache A-92 piece 2b owes); the BENCHMARKS record with main's figures.
- Clones: ~/slates-work/g076b-prod (agents/g076b-live-write-under-50us = main 213de0284), g076b-hops (landed),
  g076b-live-write-under-50us (agents/g076b-diag-hops, DIAG only, never lands).
