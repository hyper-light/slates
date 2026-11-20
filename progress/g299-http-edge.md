# g299 -- progress (A-625 part 2, UI goal 93 51cef9f7: the HTTP edge on the daemon's runtime and git's site)

Role: implementer subagent of main, taking over A-625 part 2 from g076b. THE RULE (CLAUDE.md §8, read on origin/main
066135675): one piece; only `cargo test -p <crate> --lib <filter>` + `cargo fmt`, on the Mac Studio d1323898 (sync false,
GitHub clone of the pushed branch, CARGO_TARGET_DIR ~/.cache/g299-a625, deleted after); merge = fetch, merge origin/main,
read `git diff --stat origin/main HEAD`, `git push origin HEAD:main` alone; END with this file, run-requests.md, a 3-line
report. Never checkout/switch/stash/reset/force/commit-tree; never ~/Projects/slates; never /tmp; never build on the
MacBook (formatting with the pinned 1.98.1 rustfmt binary locally compiles nothing; `cargo fmt --check` ran on the Studio).

## Piece A-625 part 2 (2026-11-19, g299)
- Worktree ~/slates-work/g299-a625 (branch agents/g299-a625, `git worktree add -b` from origin/main 066135675 off the
  g076b-live-write-under-50us clone). Scratch outside the tree: ~/slates-work/g299-scratch (commit messages, the real
  files kept while the red stubs were committed).
- Commits: abf86570b (red: 22 tests over compiling stubs), ff4b5012d (the implementation), 3135d62e2 (design status,
  tunables row, amendment, GAPS), 2dfbb3e98 (merge main: lib.rs module list + amendment log's end), 634cde44c (owed list:
  A-645 cross-shard, A-629 expiry), fef7a1643 (merge main again: amendment log's end).
- What landed (see the A-625 part 2 amendment in SLATES_DESIGN.md for the full record):
  - crates/server/src/vcs_http.rs: `serve(listener, Kept<EdgeState>, make_site)`: accepts only while a seat is free (the
    listener parks, woken by a connection's Seat guard on drop; counted `Counts::full`), each connection a
    `spawn_child` + `detach` task (the runtime cancels and joins children when the listener ends); cleartext refused off
    loopback. `Bounds::of_shard(clients_per_shard, region_bytes)`: seats = clients_per_shard, room = one client region;
    backlog = seats. The per-connection loop drives slates_http_edge::Connection: write/read within the phase's learned
    deadline (`within`), decide via `Site`, body runs to the `Exchange`, reply runs back, close_write + drain for a staged
    close; yields after each body run, reply run and drained read. Refusal counted at the half-close (before the client
    can see the end); cut/failed counted before the socket closes.
  - crates/server/src/vcs_http/waits.rs: `PhaseWaits` (one LearnedWait per Phase, no bound before the phase's first
    completed wait, no ceiling of its own on the back-off) and `Tracker` (per connection: handshake/idle/head/drain are
    whole-phase waits with a deadline fixed at the phase's start; body/reply are per-silence; waits before the proof are
    kept as each phase's longest and folded in at the proof; a stranger's cut backs nothing off; a client's close
    teaches nothing).
  - crates/server/src/vcs_http/git.rs: `GitSite<A: Authenticator, R: Repositories, S: GitServices>`: http-backend's
    route (refusal_parts) whoever asks; credentials -> 401 with the connection's challenges (Basic over TLS only) / 400;
    repository by exact name (404, other tool 404); `vcs_policy::reaches` (403); service start (`Unserved` -> 503 for both
    services: no server-side git store per repository yet, so neither A-640's upload-pack nor A-620's session can be
    instantiated by the server); v0/v1 `# service=` prelude before the service's advertisement.
  - crates/server/src/vcs_policy.rs: `reaches(record, principal, rights)`: owner always; else any Allow rule naming the
    principal for one of the rights; Unreadable for a damaged record.
  - crates/git/src/http.rs: `Field`, `no_cache_fields`, `reply_fields`, `refusal_parts` (fields as data);
    `reply_head`/`refusal_reply` now written from them (byte-identical, golden tests unchanged and green).
  - Manifests: workspace `slates-git` entry; slates-server depends on slates-http, slates-http-edge, slates-git; lock
    hand-edited (three lines), verified by `--locked` on every Studio run.
- Evidence (Mac Studio, kiln, debug, --locked):
  - red abf86570b (2026-11-19 ~06:01Z, load 5.8): slates-git --lib http 16 passed / 2 failed (both new);
    slates-server --lib vcs_ 26 passed / 20 failed (all 20 new), each for its reason.
  - green ff4b5012d (~06:04Z, load 11.1): fmt --check clean; slates-git 18/18; slates-server 46/46 (1.01 s).
  - merged 634cde44c (~06:11Z, load 3.6): fmt clean; 18/18; 46/46.
  - merged fef7a1643, the landed tree's code (r950, ~06:15Z, load 11.1): fmt clean; slates-git 18/18; slates-server
    --lib vcs_ 62/62 (main's 16 new vcs_hooks tests match the filter). The last merge, 6154528e4, brought slates-svn
    and documents alone.
- LANDED main 87e21b01a..6154528e4 (2026-11-19 ~06:18Z); `git diff --stat origin/main HEAD` before the push showed my
  15 files alone. amendments.tsv's A-625 row updated (backup amendments.tsv.bak-g299). Run request filed 06:20Z
  (clippy, xtask, doc, the Linux/Windows lib tests, and the live clone with its prerequisites).
- Decisions taken (each recorded in the amendment; main may overrule):
  1. At the seat bound the listener does not accept (no 503): over HTTPS a 503 needs a handshake (a task and buffers past
     the bound), and a refusal closed with the client's bytes unread is a reset that can erase it (A-183). The kernel's
     backlog is the same bound. Banned item 8's "typed refusal at the bound" is then the kernel's; the full periods are
     counted. MCP's loopback edge answers 503 inline; that cannot work under TLS.
  2. "None before a first measurement" read as HookTimes reads it: a phase's waits have no deadline until one of its waits
     completed (the design's table had named "the round trip measured at connect" as the first value; the row now says
     none). Exposure: before any measurement a silent client holds one seat until it leaves; the seats bound how many.
  3. Only connections whose credentials proved a principal teach the bounds (A-523's rule for the MCP edge): without it
     any anonymous TCP client could ratchet RTTVAR up ~1.75x per trickled sample (computed from RFC 6298's gains).
  4. Whole-phase waits for handshake/idle/head/drain (slowloris is held to the phase's learned length), per-silence for
     body/reply (their length is the push's or clone's size). The design's row said "gaps"; the amendment explains.
- LEFT (main starts each):
  - The daemon starting an edge: listen address, the operator's certificate and key (no std::fs in slates-server: the
    CLI or API role must hand the DER in), an Authenticator over §4.13's principals and keys (a minted bearer for the
    live run, as MCP mints one), a Repositories lookup that reaches each repository's partition (async, cross-shard).
  - Serving across shards (A-645's scale): one shard's seats bound this edge; move each connection to its repository's
    owner shard once its head names the repository (as NFS moves a mount's connection), or listen on every shard.
  - GitServices' real implementation once the server holds a git store per repository: A-640's upload-pack (Graph over
    A-614's store) and A-620's session/commit (A-642's per-green logs). The Exchange trait is async for them.
  - The Authenticator handing a bearer credential's expiry with its principal (A-629 part 2's LFS actions), and LFS's
    routes through the edge (`LFS-Authenticate`).
  - Mercurial's and Subversion's sites; the MCP edge onto slates-http/slates-http-edge.
  - The live `git clone` over HTTPS: main's, once the daemon starts an edge and upload-pack serves (filed in
    run-requests.md with its prerequisites).
- Studio scratch ~/.cache/g299-a625 (8.1 GB) deleted 2026-11-19 ~06:21Z. The worktree ~/slates-work/g299-a625 is clean.

## A-625 part 2, continued (2026-11-19, main's two rulings): LANDED main 3fc080a36..cf9fe4c25 (~07:03Z)
- Rulings: (1) confirmed: at the bound no 503, the listener holds/pushes back. (2) overruled "none before a first
  measurement": before a phase has learned, RFC 6298's first RTO from the connection's own measurement R, floored by the
  clock's measured granularity; then refined with g294's SSH rule: a wait past its bound marks the connection overdue,
  the edge reaps overdue connections, most overdue first, only while every seat is taken; a reap teaches nothing and
  backs off; test both sides.
- Built: slates-rt `tcp::RoundTrip` + `TcpStream::round_trip` (Linux tcpi_rtt us @68 of tcp_info, macOS tcpi_srtt ms @44
  of tcp_connection_info, Windows RttUs via a shared `tcp_info` helper; cfg-free parsers with golden vectors). Edge: R =
  kernel's TCP handshake RTT at accept, + the edge's own TLS work during the handshake, then the handshake's duration; G =
  max(tick, scheduler overrun, kernel RTT resolution); bound = R + max(G, 2R), doubled per reap backed off. `awaited`
  races I/O vs deadline without dropping the I/O (write_all keeps its place); overdue mark in the seat; seats are slots
  (task, mark, reaping); the listener always accepts the next connection and `seat_for` reaps the most overdue
  (cancel the task; Seat guard gives the seat back from finish(); same-shard wakes go to the local queue, safe there);
  reaps back off once per bound (generation rule, RFC 6298 §5.5), only for proven connections.
- Commits: 2d2749b1a (red: first RTO + rt readers), 41698f336 (red: reaping, 8/8), 2559aa2fd (green), docs commit,
  merges 05ccf4e8c, 72948810c, d8ce96841, cf9fe4c25.
- Evidence (Studio, debug, --locked): red 2d2749b1a: rt 2/2 failing, server 4 failing (2 waits, the then-silent test,
  HTTPS under an unbounded stub); red 41698f336: 8/8 reap tests failing; green 2559aa2fd: rt 2/2, server vcs_ 69/69,
  fmt clean; merged 05ccf4e8c: same.
- Filed: run request 07:05Z (Linux/Windows readers, clippy/xtask, the timer hypothesis, main's two warnings).
- NEXT (main's instruction): A-625 part 3 -- the daemon starting an edge (address, identity, authenticator, the
  repository lookup across partitions), GitServices wired as A-640 (g296) and A-620 (g258) land.

## A-625 part 3, first slice (2026-11-19, main's instruction: wire A-640 part 2's upload-pack): LANDED main 6ebb50732..9200bb014 (~07:20Z)
- Built: `crates/server/src/vcs_http/git/upload.rs`: `StoreLender` (holds(id), lend(id, step) lending `Served` afresh per
  step), `UploadServices<L>` (GitServices: upload-pack started for a held store; receive-pack and an unheld store 503),
  `Upload` exchange (POST request held whole, charged to a slates_git::Admission of the request room, 413 past it;
  body_end -> `slates_git::upload::serve` with Asked::Request or Asked::Advertise; reply = one `Answer::write` step of
  `pktline::MAX_LEN` room). Tests through git's site over an empty repository's store (Repository::new + ContentStore,
  slates-vcs dev-dependency): v2 advertisement, ls-refs flush, a garbage body answered ERR, receive-pack/unheld 503.
- Commits: 1fb50b1be ("wip": the first draft, committed by mistake with a bare message before a merge; not rewritten,
  recorded in cb6deefe7's message), 547852e41 (merge main), cb6deefe7 (red), 4b816d17a (green), 9200bb014 (docs).
- Evidence (Studio, debug, --locked): red cb6deefe7 3/3 new failing; green 4b816d17a: server vcs_ 76/76, rt 2/2, fmt.
- Studio scratch deleted (8.0 GB) ~07:21Z.
- LEFT of part 3 (main starts each):
  1. The server holding each git repository's store (slates_git::store::Repository + slates_vcs::ContentStore) on its
     owner shard, made with the repository and recovered, and a `StoreLender` over it -- the lend runs on the owner
     shard, so either the connection moves there once its head names the repository (A-645) or each step is a
     cross-shard call (`Answer` would then have to cross shards). With A-653's PackSource over the store's stream.
  2. The daemon starting an edge: `DaemonConfig` field for the listen address (and an identity for TLS: the server may
     not read files; the CLI/API role would hand DER in, or an owed identity source), Bounds::of_shard(config.
     clients_per_shard, config.region.total_bytes(), config.runtime.timer_tick_ns), EdgeState kept on the control shard,
     `serve` spawned beside `start_nfs_transport`; an Authenticator: a bearer minted at start from
     `slates_transport::handshake::secure_random` (as MCP mints its own), logged once, proving
     `Principal::Uid { host: daemon::local_anchor(&config, &identity), uid: <the daemon's uid> }` (the principal the CLI's
     CreateRepository records as owner); a Repositories lookup: `verbs::owner_of_name` -> `verbs::shard_of_partition` ->
     `xshard::call_in_shard_time_given_room(.., |s| s.db.partition().repositories().repository_named(&name).cloned(), ..)`
     (Repositories::find becomes async; GitSite::answer awaits it).
  3. receive-pack's session behind `GitServices` when A-654 lands (slates_git::receive::Session + A-635's
     RepositoryLogs::append, which nothing calls yet).
  4. The live `git clone` (main's): over loopback cleartext with `git -c http.extraHeader="Authorization: Bearer <token>"`
     once 1-2 land, or HTTPS once an identity source exists.

## A-625 part 3's rest (2026-11-19, g299 relaunched; main's rulings 1-5, main's credential-record and LFS messages)
- Readback of THE RULE sent to main. Worktree ~/slates-work/g299-a625, branch agents/g299-a625 (pushed), scratch
  ~/slates-work/g299-scratch (real files, commit messages, docs drafts in a625p3-docs/).
- Slice A, the credential record (main's message: g294 keeps no keys; build ONE record): red 0f65a1b9b (11 tests over
  stubs), green b0a8b16ac. slates-db credentials (table, guard/apply, budget, snapshot), slates-ipc
  EnrollCredential/RevokeCredential + CredentialMaterial + replies (wire_order hand-published: db Op 98/99,
  CredentialKind; ipc RequestBody 87/88, ReplyBody 74/75, CredentialMaterial), slates-server credentials (ids, owner_of,
  proofs, token_text, key_blob_of_line, key_fingerprint, verbs, find), client, api Issuer, cli `slates credential`.
  Names sent to main for g294 (unbuilt at the time).
- Slice B, the edge's start + move: 72fae3533 (written while the Studio was offline). DaemonConfig::vcs_http
  (VcsHttpConfig, EdgeIdentity), vcs_http::start (daemon_edge.rs), Answer::Move + per-shard seats + bounded arrivals,
  async Authenticator (RecordedCredentials), Repositories::locate (ShardRepositories), Refused typed + counted.
  Red check planned on a mutated Studio copy (not committed stubs).
- Studio d1323898 OFFLINE from ~23:30 PST (main's message); probes every few minutes; nothing built yet.
- LEFT: the Studio runs (db, ipc? no: db/server/cli unit tests + fmt), fixes, docs (drafts ready), merge, land, report;
  LFS routes on the site (main's Streamed/AfterBody ruling) if it fits, else recorded for main; the operator flag
  `--vcs-http` and the fleet identity hand-in in the daemon role (owed); the live clone needs A-662.
- (later) fa995b7bb: cli --principal/--expires declared as value flags (found by reading: the grammar test would have
  failed). 178c14b8b: merged origin/main (clean, no markers), pushed. Branch tip 178c14b8b; nothing built yet.
- Run scripts ready in ~/slates-work/g299-scratch/runs/: red-a.sh (slice A's red commit 0f65a1b9b, expect 11 fail),
  green.sh (fmt, slates-db --lib, both wire_order checks, slates-server --lib credentials and vcs_, slates-cli --bin
  slates credential), red-b.sh (slice B mutated copy: no move + cleartext Basic untyped; expect the 3 new vcs tests
  red). Pass each as a kiln run on adas-mac-studio.local-2, sync false, one at a time, CARGO_BUILD_JOBS=4.
- Docs drafts (design status, GAPS, amendment) in ~/slates-work/g299-scratch/a625p3-docs/, EVIDENCE placeholders to fill.
- LFS on the site: recorded for main (needs the engine to decide a status after the body, and an objects store, A-662).
- Waiting on the Studio (offline since ~23:30 PST 2026-11-18); reported to main.

## Relaunch 2026-11-19 ~16:10Z (fresh g299; main's brief: merge main; red-a, red-b, green on jupiter; land; names to main)
- The worktree is ~/slates-work/g299-a625p3 (not g299-a625), branch agents/g299-a625. Readback of THE RULE sent to main
  (CLAUDE.md §8 on origin/main 4644dcec3).
- 080a55cd4: the predecessor's uncommitted docs drafts committed as found (design §4.13 status, §4.17.2 status, the
  amendment, GAPS), two `EVIDENCE` placeholders left to fill from jupiter's runs. Pushed.
- 2070b9112: merged origin/main 4644dcec3 (79 commits). Conflicts: main's A-664 appended `Op 98 IdentitiesSet`, the
  catalog's GitIdentity/IdentityRecord and the snapshot's `identities`; resolved main first, ours after: the credential
  ops are now Op 99 CredentialEnrolled, Op 100 CredentialRevoked (wire_order.txt edited by hand to match; the jupiter run
  re-publishes it and diffs), snapshot `credentials` after `identities`; the amendment log's end main's entries first.
  check/apply read as exhaustive (no wildcard) with both arms; both snapshot fields restore. No conflict markers. Pushed.
  main's side touched no ipc/client/api/cli files, so slates-ipc's tags (RequestBody 87/88, ReplyBody 74/75) stand.
- kiln r44 on jupiter (started ~16:22Z, script ~/slates-work/g299-scratch/relaunch/jupiter-runs.sh): red-a at 0f65a1b9b
  (db, server credentials, cli credential), green on 2070b9112 (fmt x6 crates, db lib, http-edge lib, server lib at 4
  threads vs the 23, cli credential, wire_order publishers for db and ipc + diff), red-b on the same tip tree mutated.
  Logs stay in jupiter:$HOME/.cache/g299-a625p3/logs until read (delete after).
- NEXT: read r44; fill the two EVIDENCE placeholders (GAPS, amendment's Tests); merge main again; land; run request
  (macOS owed + main's gates); message main the record's names for g294 (drafted below); END.

## Relaunch 2, 2026-11-19 ~17:00Z (fresh g299; main's brief: no readback; rerun red-a, green, red-b on jupiter; land; names to main)
- Found r44 STILL RUNNING on jupiter though kiln had lost it: bash 325972 (08:22 PST = 16:22Z) in red-a's
  `cargo test -p slates-server --lib credentials`. A cleanup `rm -rf ~/.cache/g299-a625p3` had already deleted most of
  its tree; its nine processes (3 bash, cargo proxy 327260, cargo 338280, aws-lc build script, 3 rustc) were listed by
  walking the tree from 325972 and stopped by PID (kiln's 13052 untouched); none left; the directory deleted.
- 8549fc545: ipc doc fix: EnrollCredential's proof comment named "slates.credential"; now "slates.credential-enroll" and
  the field order (principal, material, expires_ns), as server credentials::enroll_proof hashes.
- 37fab9d50: merged origin/main e3234e618 (main's 71f05f93d merge engine + e3234e618 chain-replay test; crates/merge and
  GAPS only), clean, no markers; default merge message (main carries such merges). Pushed agents/g299-a625 = 37fab9d50.
- kiln r101 on jupiter, started 17:17:12Z, script ~/slates-work/g299-scratch/relaunch2/jupiter-r2.sh: green on
  37fab9d50 (fmt x6; db, ipc, http-edge lib; server lib 4 threads vs the 23; cli credential; db+ipc wire_order
  publish + diff), red-b (mutated tip), red-a (0f65a1b9b), main e3234e618's ipc lib (baseline), main's server lib only if
  ours fails beyond the 23. Logs: jupiter:~/.cache/g299-r2/logs; green target kept at ~/.cache/g299-target (delete at END).

## Relaunch 3, 2026-11-19 ~19:25Z (fresh g299; main's brief: the Studio runs the focused tests, fix the proof comment in the same commit, merge, land, names to main)
- Found: relaunch 2 had made ~/slates-work/g299-a625land, branch agents/g299-a625-land (pushed) = 2070b9112 + merge
  438e28680 of main 92ffda926 (conflicts lib.rs module list + amendment log end; and NoPacks now returns
  PackRefused::PastAdmission, since A-640 part 3 removed PackRefused::Stalled: main's server lib tests have not compiled
  since f607f424c). It does not carry 8549fc545 (the ipc doc fix) nor 37fab9d50 (default merge message, kept off main).
  kiln no longer holds r101 (its results were never recorded): nothing of this piece has a recorded build.
- Main's correction (Ada): run the FOCUSED unit AND integration tests that exercise the change (wire_order for db and
  ipc; any edge or credential integration test), land on green of all. No integration test names vcs_http or the
  credential verbs; DaemonConfig is built only by derive; no exhaustive Op/ReplyBody match outside the piece's files.
- Iteration branch agents/g299-a625-try (worktree ~/slates-work/g299-a625try) = 438e28680 + daf2dd18e (8549fc545
  cherry-picked). The landing commit is made ONCE on agents/g299-a625-land (squash of try + EVIDENCE docs), then merge
  origin/main, final Studio run on that tip, push HEAD:main. try's commits never reach main.
- Studio set: fmt (db ipc server client api cli); db --lib credentials; db --test wire_order; ipc --lib protocol;
  ipc --test wire_order; http-edge --lib; server --lib credentials and vcs_http (4 threads); cli --bin slates credential.
  Studio dirs: ~/.cache/g299-r3 (clone + logs), ~/.cache/g299-target: delete at END.
- r17 (Studio, macOS arm64, daf2dd18e, tree 62227c1669a6, 19:20-19:26Z): fmt clean x6; db --lib credentials 5/5; db
  --test wire_order 2/2; ipc --lib protocol 5/5 (1 ignored); ipc --test wire_order 2/2; http-edge --lib 31/31;
  slates-server did not compile: E0599 x2 (credentials.rs 254/280, the Clock trait not imported) and E0308 x2
  (daemon_edge.rs 222, vcs_http.rs 1166: an Err arm returning count_refusal's u64). Non-test build warns: unused
  `pub(crate) use upload::{StoreLender, UploadServices}` (vcs_http/git.rs) -- ALREADY ON MAIN (git.rs:47 since
  9200bb014), so a finding for main, not this piece.
- 0a88675ea (try): the three fixes. Main's message: 93bd6e807 fixed the stub on main; merge main and run on the merged
  tree. b94e176f1 (try): merged origin/main 71a43cd97 (stub = main's no_pack, ours dropped; amendment log end main
  first). Op 99/100 after main's 98; ipc 87/88, 74/75 after 86/73. r46 launched on b94e176f1 ~19:45Z.
- CLAUDE.md §8 now (0c7a77bd3): focused --lib AND --test runs plus fmt, one commit, merge, push.
- r46 (Studio, macOS arm64, b94e176f1 = try over main 71a43cd97, tree b0b82c6dcda9 matched locally, 19:30-19:32Z): ALL
  GREEN: fmt x6 clean; db credentials 5/5; db wire_order 2/2; ipc protocol 5/5; ipc wire_order 2/2; http-edge 31/31;
  server credentials 9/9 (4 threads); server vcs_http 32/32; cli credential 1/1. Non-test server build warnings are
  main's already: vcs_http/git.rs unused `pub(crate) use upload::{StoreLender, UploadServices}` (main git.rs:47) and
  lfs.rs dead code (mod lfs has no caller outside itself on main).
- LANDING: agents/g299-a625-land: cceaa51b2 (THE one commit: ipc proof comment + the three compile fixes + EVIDENCE
  in GAPS and the amendment's Tests line; index verified == try 0a88675ea's tree before the docs) then 11f927399 merge
  origin/main a6306963e (conflicts Cargo.toml, server Cargo.toml (main's slates-hg/slates-vcs first, our slates-ssh
  after), the stub (main's no_pack), amendment end (main first)); diff --stat origin/main HEAD = the piece's 30 files;
  Op 99/100 after main's 98, ipc 87/88 74/75 after 86/73 (a6306963e). Pushed. r58 (Studio) on 11f927399, tree
  e2fc6a727817, started 19:36Z: if green, push HEAD:main (re-merge if main moved, re-run if main touched our crates).
- r58 (Studio) on 11f927399, tree e2fc6a727817: ALL GREEN (same counts). Main had moved: 49b6431fc merged main 2e4433040
  (A-626 part 3: crates/merge + anchor LAYOUT_VERSION 12; one conflict, amendment end); r70 on it (tree 8a7f96bec8f4)
  ALL GREEN. Main moved again (A-651 part 1, the SSH edge in slates-server): 06daf9301 merged main 3b357a256:
  vcs_http.rs module list (daemon_edge + main's pub(crate) git/waits); BOTH manifests held slates-ssh twice after a
  clean auto-merge (duplicate key) -> main's entry kept, our reason in its comment, Cargo.lock == main's; main's SSH
  site test stub `Held` implements Repositories, which our piece gave a required `locate` -> Held answers Located::Here
  (E0046 otherwise). Pushed land 06daf9301 (tree e0228c01f09d); r85 (Studio) runs the focused set + server --lib
  vcs_ssh. Note for g294: the SSH site calls `find`, which our ShardRepositories answers for this shard's partition
  alone; a repository another shard holds needs `locate` and a move, as the HTTP edge does.
- r85 also ran `vcs_ssh` (whole filter): 18/19, the one failure A-651's serve test (Closing sample 0), code unchanged
  by this landing; filed for main as a finding, hypothesis labelled.
- Main kept landing every ~3 min: merges cab0e1288 (787f5948b: A-660, A-640 Fragments, merge fix), 0a19aae4d
  (77e5ffb46: A-671 wildmatch into slates-vcs), 1f210ebb5 (acc2ac16f: A-662 vcs_store; tags re-checked, main's last
  still Op 98 / Req 86 / Reply 73), 50779a379 (af520142f: A-649 part 4 hg_base), each green on the Studio before the next
  push attempt (server credentials 9/9, vcs_http 32/32, vcs_ssh::site 6/6, fmt clean; db/ipc/edge/cli where touched);
  3329211e0 (effb30ac2: crates/svn alone, clean; merged and pushed per §8's "fetch, merge and push again"; its default
  merge message amended before the push).
- LANDED main effb30ac2..3329211e0 (2026-11-19 ~19:58Z). `git diff --stat origin/main HEAD` before the push: the
  piece's 30 files alone; no conflict markers. Confirmed on the landed commit (Studio, tree 8db568bdcb34,
  19:58-20:00Z): fmt x6 clean; db credentials 5/5; db wire_order 2/2; ipc protocol 5/5; ipc wire_order 2/2; http-edge
  31/31; server credentials 9/9, vcs_http 32/32, vcs_ssh::site 6/6; cli credential 1/1.
- Done: amendments.tsv A-625 row (backup amendments.tsv.bak-g299-r3); run-requests.md entry 20:05Z (main's gates,
  red-checks incl. 0f65a1b9b never built, the A-651 flake, live clone). Studio ~/.cache/g299-r3 (268 MB) and
  ~/.cache/g299-target (22 GB) deleted ~20:01Z. Worktrees ~/slates-work/g299-a625land (= main 3329211e0) and
  ~/slates-work/g299-a625try (scratch branch agents/g299-a625-try, never landed) are clean.
- Left on jupiter by relaunch 2 (not touched, per the brief): possibly ~/.cache/g299-r2 and ~/.cache/g299-target.
- LEFT (main starts each): LFS routes on the site; the operator's `--vcs-http` flag and the fleet identity hand-in;
  listing credentials; a consumer's revocation in the SSH key holders (A-651); hg/svn HTTP sites; the live clone.

## Next piece (2026-11-19 ~20:10Z, main's brief): the daemon role's `--vcs-http` flag and the fleet identity hand-in
- `slates daemon` starts the shared HTTP edge from the CLI with its address and identity. Failing test first; focused
  unit + integration tests (cli's daemon-role tests, server daemon_edge) + fmt on the Studio; one commit + the merge.
- Worktree ~/slates-work/g299-vcshttp, branch agents/g299-vcs-http-flag from origin/main.
- Built (A-625 part 3, continued): slates-api ProcessOptions::vcs_http + role::vcs_http_address (cleartext off
  loopback refused as usage without --fleet) + Role::parse/args + the daemon role wiring DaemonConfig::vcs_http (fleet
  node: plan's identity) and logging "slates daemon: the version-control HTTP edge serves HTTPS|cleartext on IP:PORT"
  before its serving line; CONTRACT 52 (main's A-664 took 51); slates-cli grammar + usage; slates-server
  deploy::FleetPlan::edge_identity (cert + key DER copied before the key moves into the transport Identity); docs
  (§4.17.2 status, amendment, GAPS, docs/cli.md, README).
- Red r143 (Studio, 9346082e8): the 4 new tests failed each for its own reason, 28 neighbours passed. Green on the
  scratch 85b1cfcc6 (20:19Z) and on 6814381b1 = the landing tree byte for byte (tree f8aea7cb475e, 20:22-20:24Z):
  api role:: 4/4, contract_history 1/1; server deploy:: 14/14, vcs_http::daemon_edge 1/1; cli the_grammar 11/11;
  cli --test cli vcs_http 1/1; fmt clean x3.
- LANDED main 9d685a9bc..3274e7bff (~20:26Z): fb6b66296 (the one commit) + merges 21120623f (b04c552b0: A-651 part 2,
  A-640 sparse, A-626 checkpoints) and the merge of 9d685a9bc (A-675 hg import) -- neither touches the piece's files;
  the piece's only server change is a FleetPlan field only deploy.rs builds; pushed under §8's fetch-merge-push.
  `git diff --stat origin/main HEAD` before the push: 12 files, the piece's alone; no markers. r187 confirms on the
  landed commit.
- Scratch: branch agents/g299-vcs-http-flag (worktree ~/slates-work/g299-vcshttp), never landed. Landing worktree
  ~/slates-work/g299-vcshttp-land (branch agents/g299-vcs-http-land).
- r187 (Studio) on the landed commit 3274e7bff (tree 72c5332a6d88, 20:26-20:30Z): fmt clean x3; api role:: 4/4,
  contract_history 1/1; server deploy:: 14/14, daemon_edge 1/1; cli the_grammar 11/11; cli --test cli vcs_http 1/1
  (daemon named 127.0.0.1:58038). Run request filed 20:30Z. Registry row extended (backup amendments.tsv.bak-g299-r4).
  Studio ~/.cache/g299-r4 and ~/.cache/g299-target deleted ~20:31Z. END of this piece.
- LEFT (main starts each): the anchor holding the edge's listener across daemon restarts (as the export's); the
  option on a library starter's anchor (slates_api::start); HTTPS end to end on a fleet node + the live clone (main's).

## Next piece (2026-11-19 ~20:35Z, main's brief): the anchor holds the version-control edge's listener across daemon restarts
- As it holds the daemon's other listeners (the export's), so a restart never drops the port; plus the matching option
  on slates_api::start's anchor if it is the same piece. Failing test first (a restart under an anchor keeps the edge's
  port and serves again); focused tests (anchor, deploy, cli's vcs_http) + fmt on the Studio; one commit + the merge.
- Worktree ~/slates-work/g299-edgeheld, branch agents/g299-edge-held from origin/main.
- (after the usage-limit restart, ~21:xxZ) worktree intact; red state committed on agents/g299-edge-held (scratch):
  slates-anchor ENV_VCS_HTTP + Supervisor::hold_vcs_http_listener (start() not handing it over yet); anchor test
  the_supervised_child_inherits_the_held_vcs_http_listener; server daemon_edge listens_at stub + test; api
  StartSpec::vcs_http field (role() ignoring it) + start::tests; cli test
  a_daemon_restart_keeps_the_vcs_http_edges_port_and_serves_again (+ shared bare_request / edge_port_before_serving).
- (relaunch, 2026-11-19 ~21:20Z, fresh g299; brief: finish the piece, land, END) Found the predecessor further than the
  entry above: scratch agents/g299-edge-held = f3e4f3901 (red) + 71e6fa164 (green candidate) + b03a50b29 (merge main
  4dca63df7), pushed; landing worktree ~/slates-work/g299-edgeheld-land, branch agents/g299-edge-held-land = ONE commit
  5705edca5 over main 4dca63df7 (code byte-identical to b03a50b29's tree ef668311c13d; docs differ only by the filled
  EVIDENCE lines), NOT pushed -> pushed at once (21:22Z). Its message records red r9 (20:54-20:59Z) and green on
  ef668311c13d (21:08-21:12Z), Studio. Reviewed the diff (anchor ENV_VCS_HTTP + hold_vcs_http_listener; api role binds +
  holds, StartSpec::vcs_http, CONTRACT 53; server held_listener shared with NFS (no new unsafe), listens_at adoption check,
  bind_vcs_http_listener; cli restart test); test helpers follow the file's accepted lint pattern.
- 39e9c0853: merged origin/main 3612d1ef9 (A-626 part 4: anchor layout 14 + anchor tests, server work_home/work_log/
  listing/verbs; svn fixture sums). One conflict, the amendment log's end: main's A-626 part 4 first, ours after; checked
  as the exact union (resolved-minus-main == the piece's hunks). diff --stat origin/main = the piece's 16 files (512+/56-).
  No markers. Pushed agents/g299-edge-held-land = 39e9c0853 (tree 6b2843316). Main touched anchor + server, so the
  focused set re-runs: kiln r81 (Studio, started ~21:27Z; clone ~/.cache/g299-r5, target ~/.cache/g299-target).
- r81 (Studio, macOS arm64) on 39e9c0853 (tree 6b2843316, 21:28-21:32Z): ALL GREEN: fmt --check clean (anchor, api,
  cli, server); anchor --test anchor the_supervised_child_inherits 2/2; api --lib start::tests role::tests
  contract_history 6/6; server --lib vcs_http::daemon_edge deploy::tests (4 threads) 16/16; cli --test cli vcs_http 2/2.
- Push refused (main moved): 8a27ef543 merged b5231f0ff (docs + a //! comment in vcs_http.rs: compiles to nothing);
  refused again: 5a9e93d5c merged 0577ab0bd (A-654 part 2, git_push in slates-server: our crate) -> r100 on 5a9e93d5c
  (tree 5894e7161, 21:40-21:54Z): ALL GREEN, same counts. Then 97bb26575 (34d10acb5: slates-db A-636 part 2a, svn
  tests), d2a269092 (9d54e0f9a: an SSH test harness fix), 225d7eead (092386776: slates-merge A-658 part 2): none of the
  piece's files or the code its tests run (checked by script: ~/slates-work/g299-scratch/edgeheld-merge-main.sh, which
  refuses on any change under anchor/api/cli, rt tcp, server daemon.rs/vcs_http*, manifests, the piece's docs; resolves
  only our entry vs main's at the amendment log's end, main first; verifies the docs hunks == the piece's and the
  16-file 512+/56- stat; commits; never pushes). Every merge's amendment-log conflict resolved main first, checked as
  the exact union; no markers.
- LANDED main 092386776..225d7eead (2026-11-19 ~21:57Z): the one commit 5705edca5 + merges 39e9c0853, 8a27ef543,
  5a9e93d5c, 97bb26575, d2a269092, 225d7eead. `git diff --stat origin/main HEAD` before the push: the piece's 16 files
  alone (512+/56-); the landing range on main is the same 16 files.
- Done: amendments.tsv A-625 row extended (backup amendments.tsv.bak-g299-r5); run-requests.md entry 21:59Z (Linux and
  Windows, clippy/xtask/docs, three mutants, the live restart under a client loop). Studio ~/.cache/g299-r6 (190 MB) and
  ~/.cache/g299-target (26 GB) deleted ~21:58Z (r5 deleted by r100's script); no g299 dirs remain there.
- Worktrees (clean): ~/slates-work/g299-edgeheld (scratch branch agents/g299-edge-held, never landed) and
  ~/slates-work/g299-edgeheld-land (agents/g299-edge-held-land = main 225d7eead). END of this piece.
- LEFT (main starts each): Windows keeps no listener across a restart (platforms.md gap 2); HTTPS end to end on a fleet
  node across a restart; the live clone once A-682's lender serves upload-pack.

## Next piece (2026-11-19 ~22:20Z, main's brief): A-684, the daemon starts the SSH edge (`--vcs-ssh`), anchor-held
- Mirror the HTTP piece: `--vcs-ssh <addr>` grammar (anchor/daemon) + StartSpec::vcs_ssh; the daemon starts A-651's
  listener and connection tasks; the anchor holds the listener (held_listener) AND the host key across restarts; the
  daemon's line names the address and the key's OpenSSH SHA256 fingerprint; authenticator over the credential record
  (g294's A-651 part 3 landing: merge main when it lands, build against the seam until then). Host key: the node's
  identity where the plan hands one; a lone anchor makes an Ed25519 key in RAM at its start and hands it to each daemon.
- Failing tests first: the grammar; a CLI test (`--vcs-ssh 127.0.0.1:0` names address + fingerprint before serving; the
  edge's host key matches; a restart keeps port and key). Focused: anchor, api, server SSH daemon edge, `--test cli
  vcs_ssh`, fmt, on the Studio. Then design, GAPS, amendments row; one commit + the merge; END.
- Worktree ~/slates-work/g299-vcsssh, branch agents/g299-vcs-ssh from origin/main f05af5288.

## Relaunch 2026-11-19 ~22:50Z (fresh g299; brief: A-684, the daemon starts the SSH edge; mirror the HTTP piece)
- Found ~/slates-work/g299-vcsssh (branch agents/g299-vcs-ssh) clean at f05af5288, nothing written, never pushed;
  fast-forwarded to origin/main b94ae6e90. Policy: CLAUDE.md §8 on origin/main (332 lines); relaunch brief re-read
  (Ada 22:45Z: completeness first; findings by reading go to main in one line, never chased).
- g294's A-651 part 3 (RecordKeys + SshSite::lookup, NoKeyRecord gone) not on main yet: building against NoKeyRecord.
- Plan: anchor SUP_SSH_HOST_PRESENT/SEED (384/392) + publish/read; ENV_VCS_SSH + hold_vcs_ssh_listener; api --vcs-ssh
  (ProcessOptions, Role, StartSpec, CONTRACT 54), anchor role draws + publishes the seed (no fleet) and holds the
  listener; server DaemonConfig::vcs_ssh (VcsSshConfig{address, identity}), vcs_ssh::daemon_edge::start (identity's
  Ed25519 key, else the anchor's seed, else a lone daemon's own), Daemon::vcs_ssh() {port, fingerprint}; daemon line
  names address + fingerprint; cli grammar + `--test cli vcs_ssh` (restart keeps port + key).
- (~23:20Z) main: A-651 part 3 landed (9cc9a378d, merged 17f4f7d6f): fast-forwarded, the daemon site uses RecordKeys.
  main: g083's stock-hg cli test owns `VCS_SSH_LINE` (same words as this piece's line): kept theirs, dropped mine.
- Scratch branch agents/g299-vcs-ssh: 657df042a (red: tests over stubs; also pushed as agents/g299-vcs-ssh-red),
  397dca2b5 (green candidate), 7c0b07654 (merge origin/main 770ae8826; cli.rs = main + this piece's block). Pushed.
- kiln r302 (Studio d1323898, ~23:25Z): red then green in one clone path, target ~/.cache/g299-target, logs
  ~/.cache/g299-a684/logs; script ~/slates-work/g299-scratch/a684/runs/red-green.sh. Green files saved in
  ~/slates-work/g299-scratch/a684/green/.
- Correction: the Studio's clock put r302 at 22:42-23:07Z (my earlier "~23:20Z" notes were estimates).
- r302 red (657df042a, tree 09e738558c92): every new test failed for its reason (anchor child exit 2; api UnknownFlag
  and None; cli grammar UnknownFlag; server 5 of 5; cli test: anchor ended at the unknown flag), 13 neighbours ok. The
  seed test's red was LayoutRefused{offset 4480, len 8, NotAWord}: the segment's declared words (AUD-29-09) lacked the
  new word and span; green 1 (7c0b07654) met it too (anchor 3/4, server 5/8, cli 2/3). Fixed: c1810019a declares
  SUP_SSH_HOST_PRESENT/SEED in segment_words, and start reads the anchor's seed only without an identity.
- r365 (c1810019a, pre-merge): ALL GREEN: fmt; anchor 4/4; server 8/8; api 8/8; cli grammar 12/12; cli vcs_ 3/3.
- main: A-651 part 4 (2f73eb07: serve takes Clone+Send maker; keep_here + EdgeState::arrivals on every shard) and A-685
  landed: 80e8038a7 merged origin/main 0462bd922 (conflicts: vcs_http re-exports, vcs_ssh doc, cli.rs = main + block,
  GAPS, design status + log end, main first); start now keeps arrivals on every non-control shard. Pushed.
- r382 (Studio) on 80e8038a7: the focused set again. NEXT: fill @@EVIDENCE-TESTS@@ (amendment) and @@EVIDENCE-GAPS@@,
  landing worktree ~/slates-work/g299-vcsssh-land (branch agents/g299-vcs-ssh-land from origin/main, merge --squash),
  one commit, merge main, push HEAD:main; amendments.tsv row; run request; report; delete Studio dirs.
- r382 (80e8038a7 over main 0462bd922): ALL GREEN (anchor 4/4, server 8/8, api 8/8, grammar 14/14, cli vcs_ 4/4; the
  SSH line 127.0.0.1:58134 + SHA256:cJyHlTnDIcFjiVlDz2XKXM7bZusEd/nBQAe6/n4OYTc before and after the restart).
- Landing: agents/g299-vcs-ssh-land (worktree ~/slates-work/g299-vcsssh-land): 9298592e4 (squash, evidence filled),
  merges b76d31066 (f1fbd2c44), c9da2ea1c (f7684fc20: A-689 --vcs-svn, contract 54 theirs -> A-684 = 55,
  held_or_bound_listener kept, my edge_listener dropped), 7165525d4 (2c3fa2bcc: A-682; main's word: the SSH site's
  services = ToolServices::new(ShardStores, room); docs updated). Pushed.
- MacBook disk hit 0 (No space left): removed my landed worktrees a625try, a625land, vcshttp, vcshttp-land, edgeheld,
  edgeheld-land (all clean, heads on main or pushed): 5.8 GiB free.
- Final: worktree ~/slates-work/g299-vcsssh-final, branch agents/g299-vcs-ssh-final from origin/main 2c3fa2bcc, squash
  of the landing branch staged (tree == 7165525d4's). kiln r445 runs the focused set on 7165525d4. NEXT: commit once
  with r445's numbers, merge main if moved, push HEAD:main, amendments row, report, END (delete Studio dirs).
- r445 (7165525d4): ALL GREEN (anchor 4/4, server 11/11, api 10/10, grammar 15/15, cli vcs_ 5/5).
- Final branch agents/g299-vcs-ssh-final (worktree ~/slates-work/g299-vcsssh-final): ONE commit 5908c2b8a (squash over
  main 2c3fa2bcc), merges 30a224ac8 (574185afa: A-679 made ToolServices<L, H> -> the daemon site takes
  ToolServices::new(ShardStores, ServedViews, room)), 41a0508f7 (477c85a1b: A-652 part 3 took contract 55 -> A-684 = 56;
  A-688's SvnServices needs a SvnStores lender that does not exist, so svn+ssh stays owed), 2b86aaaa1 (0b0c8e12e:
  slates-git/vcs + docs only). r463 (30a224ac8) and r489 (41a0508f7) ALL GREEN, same counts; SSH line e.g.
  127.0.0.1:50756 SHA256:W01CK/zbKxLeQsVA2SUKLuESiT6/5VvMQV3OWhPfgYE before and after the restart.
- LANDED main 0b0c8e12e..2b86aaaa1 (2026-11-20 ~00:10Z): `git diff --stat` = the piece's 26 files (1466+/47-).
- Note: 5908c2b8a's message says CONTRACT 55 and ShardStores alone; merges 30a224ac8 and 41a0508f7 record the change
  to ServedViews and to 56.
- Done: amendments.tsv A-684 row (backup amendments.tsv.bak-g299-r7); run-requests.md entry (2026-11-19T23:30Z,
  updated). Studio ~/.cache/g299-a684 (194 MB) and ~/.cache/g299-target (24 GB) deleted ~00:12Z. Local worktrees
  g299-vcsssh and g299-vcsssh-land removed (pushed); g299-vcsssh-final kept (clean, = main 2b86aaaa1).
- LEFT (main starts each): svn+ssh in the daemon's site once a SvnStores lender over A-662's map lands (A-688's owed);
  receive-pack over SSH (A-683); a daemon-level test of upload-pack / serve --stdio for an enrolled key over the SSH
  edge; ECDSA/RSA host keys for fleet nodes whose identity is not Ed25519 (P-256 nodes start no SSH edge today);
  Windows' held listener; main's live OpenSSH interop (ssh-keyscan fingerprint, git/hg clone ssh://, known_hosts strict
  across a restart); the seed test's publish-stub red-check (r302's red met the layout refusal first).
- Findings by reading, for main: vcs_http/git.rs's unused `UploadServices` re-export warned in every non-test build seen.
END of A-684.

## Next queue (main, 2026-11-20 ~00:20Z) -- NOT STARTED (context short; for the next g299)
Each its own piece: failing test first, one commit + the merge, focused unit + integration tests + fmt on the Studio,
`git diff --stat` checked for unexpected deletions before every commit (the MacBook's disk filled once), 3-line report
to main after each landing.
1. Stock interop over SSH through a real anchor's `--vcs-ssh`:
   - git 2.50.1 clone + fetch over ssh:// with an enrolled key, in crates/cli/tests/git_interop.rs (g309's file, from
     f64a6c2fd): split its SSH round trip so clone and fetch run now and only the push stays ignored on A-683 (g302).
   - hg 7.2 clone over ssh://, beside g083's stock-hg tests in crates/cli/tests/cli.rs (push stays ignored on A-696, g304).
   - Plan: anchor via `anchor_with_edges(&instance, &["--vcs-ssh", "127.0.0.1:0"])` (or a variant returning the issuer
     exports, as A-685's `start_anchor_with_vcs_http`); port + fingerprint from the daemon line with `ssh_edge_of`
     (cli.rs, A-684). The fingerprint alone cannot make a known_hosts line: read the key blob with A-684's
     `ssh_host_key(port, patience)` reader, check `slates_server::credentials::key_fingerprint(&blob)` == the printed
     fingerprint, then write `[127.0.0.1]:PORT ssh-ed25519 <base64 blob>` (needs a small RFC 4648 encoder in the test)
     to a known_hosts file and run ssh with `StrictHostKeyChecking=yes`, `UserKnownHostsFile=`, `IdentitiesOnly=yes`,
     `BatchMode=yes`, `-i <key>`. User key: stock `ssh-keygen -t ed25519 -N '' -f <dir>/id` in the test's scratch dir (use
     the dir helper git_interop.rs already uses). Enroll: `run_as_issuer(&instance, &issuer, &["credential", "key",
     "<id.pub line>"])` (issuer surface printed on macOS; Linux: skip that leg loudly, as A-685's test does). Repository:
     `slates repository create r.git --tool git` (A-685). git: `GIT_SSH_COMMAND="ssh -i .. -o .."` `git clone
     ssh://git@127.0.0.1:PORT/r.git`; hg: `hg clone -e "ssh -i .. -o .." ssh://git@127.0.0.1:PORT/<repo>`. Skip loudly if
     stock git/hg/ssh/ssh-keygen are absent (SLATES_TEST_HG names stock hg 7.2 on the Studio).
2. ECDSA host keys (P-256 at least, P-384 if cheap) and RSA if the plan can hand an RSA identity, so a fleet node
   whose identity is not Ed25519 still starts its SSH edge. Facts found in A-684: docs/deploy.md says a node's key is
   PKCS#8, SEC1 or PKCS#1 (so ECDSA and RSA both reach `FleetPlan::edge_identity`); rcgen's default (kind certs, live
   fleet, tests) is P-256. slates-ssh today: `hostkey.rs` Ed25519 only, `kexinit.rs` HOST_KEYS = [ED25519]. Needed:
   `ecdsa-sha2-nistp256` per RFC 5656 §3.1 (key blob: string name, string "nistp256", string Q uncompressed; signature
   blob: string name, string (mpint r, mpint s), from aws-lc-rs ECDSA_P256_SHA256_FIXED_SIGNING's r||s), and
   `rsa-sha2-256`/`rsa-sha2-512` host keys per RFC 8332 (key blob "ssh-rsa" e n). The server's EdgeState holds a seed
   (HostSeed) today: it must hold the identity's key source instead (PKCS#8/SEC1 doc, wiped on drop), and
   `EdgeIdentity::ed25519_seed` becomes a host-key choice by algorithm; KEXINIT then lists the key's own algorithm(s).
   Record which algorithms are supported and why in §4.17.2 and the amendment.
- svn+ssh in the daemon's site waits on g297's SvnStores lender: whoever lands last wires it (main).
