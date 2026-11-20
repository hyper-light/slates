# main <- w-balance

## 2026-11-20: record v3 (names by identity) needs your ruling on durability across nodes

w-perf landed a3 (782eb711c) and handed me v3's record.rs call site. Done so far:
- 36212ac98: the names' codec, `vcs_store::record_names` (names_to_content / names_from_content).
- 3f4996b5f: the homes image barrier is now driven by the daemon. Before this, `image_begun`/`image_published` were
  called only by tests, so no ticket would ever have answered durable. `held_committed` calls
  `homes::publication_committed` at each committed checkpoint and delta. A publication captures and commits in one turn,
  so this is exact. Red first: `homes::tests::a_committed_publication_publishes_the_tickets_of_windows_kept_before_it`.

Two facts change the v3 shape:
1. a3 images WINDOWS only. Lists (any content past one window) are `Kept::Here`, in memory, not in the image. After a
   restart a record naming a multi-window names content by its root finds no root. So v3 must name the names' windows
   themselves: the row count plus each window's identity, 32 B a window and no root. The alternative is w-perf imaging
   lists. I propose the former; it stays within A-724.
2. A v3 record replayed on ANOTHER node (a takeover, a new replica) finds no names windows: they're in the old owner's
   shard image. v2 records are self-contained (pack and names inline), so a takeover replays them anywhere.

Proposal (please rule):
- (ii, recommended) Write v3 only when the names' windows are homed in the region at f+1 (piece 6's region half:
  `slates_cluster::homes`, `serve_remote` acknowledges after its home's image is published, now that the barrier is
  driven). The append waits for the f+1 `Kept`, one barrier per shard publication (group commit). Replay reads the
  windows locally when present, else from their region homes, and holds each by identity so `release_unheld` keeps them.
  Until the region path is proven by a takeover test, records stay v2.
- (i) v3 on the owner's local image alone: smaller now, but a takeover's replay is refused. I don't recommend it.
Either way the push's latency gains up to one publication interval. I'll measure it before landing.

### Update after the identity rule (w-perf 2db786af6)
v3 names the rows by the BLAKE3 of their whole bytes plus the count (through `rows::put`, which becomes BLAKE3 with
w-perf's store change), not by window identities: windows are node-local cuts. One gap, asked of w-perf: after a
restart, the alias identity -> root has to survive (an image piece), or a v3 replay can't find rows longer than a
window. Homes' region Put will carry an object keyed by its BLAKE3, cut at each home, since a Mac and a Linux home cut
differently. The f+1 recommendation above stands.

### 2026-11-20: (ii) built and landed, gated off (base 96e24c824 -> 7317f6cce)
- 09427301d: record v3 codec, names by BLAKE3 of their rows plus the count. Replay reads them by identity and holds them.
- c42b938f2: `homes::home_in_region`, f+1 `Kept` (the local home only after its image is published).
- 7317f6cce: the push's decision homes its names before the record (edges await off-step), v3 written only if still the
  same names, else v2. Gate `vcs_store::write_version_3()` = false.
Still owed before the gate turns on:
1. The takeover test (owner killed after a v3 push, successor on another node replays and serves the names). That needs
   replay to FETCH missing names from region homes. The record replay is sync, so I'll add a prefetch before a v3
   record's replay.
2. The release A/B, v2 against v3, with the publication-interval cost.
3. Names past one window: w-perf's BLAKE3 alias image piece, and an appended put-in-parts on the wire (one message caps
   at 1 MiB). Until then pushes whose names exceed a window write v2.

### 2026-11-20: v3's takeover test is blocked on repository takeover itself (question, base c655a45c1)
Landed since: 36c6ed55d (a rebuild reads a v3 record's names from their region homes, `homes::read_in_region`; tested
with the home on another shard) and 6e9d2eeaf (a home's wait publishes its shard itself when nothing has; before,
publications were event-driven only, so a homed window could wait forever).
The takeover test you asked for (owner killed after a v3 push, a successor on another node replays it and serves the
names) needs the successor to HAVE the push records. `takeover::rebuild_repository` applies
`Op::RepositoryRebuilt { record, parts }` (parts = the names/phases/markers/locks logs) and then `vcs_store::rebuild`,
which replays `object_record`s from the successor's own partition register. I find nothing that ships a departed
owner's push records (the register's `objects`) to a successor, and no test proves it for v2 either. The only takeover
test (`fleet::tests::a_successor_rebuilds_a_repository_whole_from_the_logs_it_holds`) holds names logs alone.
Question: who owns A-636's repository takeover, and do push records reach a successor today? If they do, point me at
it and I'll write the test (in-crate, the gate on for the test's shards). If not, the v3 gate stays off until A-636
ships push records, and I'll take that work if you assign it.
Meanwhile the A/B (v2 against v3, release, best of N with all N, the publication cost) is ready to run on the 32-core
Studio as soon as it answers kiln again (it hasn't for hours; SSH is refused there).

### 2026-11-20 ~09:15: the durability barrier lands ALONE within the hour (ETA ~10:15 PST)
Shape in inbox/w-svn-from-w-balance.md: `vcs_store::content_ticket(&state)` and `vcs_store::content_durable(origin,
ticket, budget)`, group commit over the barrier already driven by committed publications. Red test: a crash between a
put and its entry's append loses nothing named. Then back to v3 at f+1.

### 2026-11-20: the durability barrier LANDED alone: db779baa3 (base 982ea33d5)
`vcs_store::content_ticket` / `content_durable` (group commit, self-publishing, typed refusals); w-svn told
(inbox/w-svn-from-w-balance.md). Red test `daemon::tests::t_a727b_...`: with the barrier bypassed, a crash after the
entry leaves the successor refusing it (replay_refused 1); with it, recovered whole. The release latency measurement
waits on the Studio (kiln unreachable). Back to v3 at f+1; the takeover question above still stands.

### 2026-11-20 ~10:30: status for the UI (what is complete, what is not)
COMPLETE, on main (please mark these steps done):
- A-727 slice b, the content plane's durability barrier: db779baa3 (`vcs_store::content_ticket` / `content_durable`,
  red crash test `daemon::tests::t_a727b_...`). w-svn has the interface and the commit.
- A-724 piece 3c, per-repository turns: 3283dd758, proof recorded in BENCHMARKS (369102a32).
- A-724 piece 8c's parts so far: v3 codec 09427301d, `home_in_region` c42b938f2, writer 7317f6cce (gated off), region
  fetch 36c6ed55d, self-publishing barrier 6e9d2eeaf.
NOT COMPLETE (keep open):
- v3 at f+1: the gate stays off until a takeover test passes, which waits on your answer above (do a departed owner's
  push records reach a successor?).
- The release latency A/B (v2 against v3, which also measures the barrier's cost). First runs were invalid: one compared
  v2 with v2 (cargo reused the build), and the second ran under memory pressure on the Studio (load 34, 595 MB free).
  Locally, through the real HTTP edge, the v3 build homes its names (`homed_in_region` 1, `published_for_ticket` 1). A
  clean run is going now (r71), waiting for load < 8 and memory pressure to clear.
- Run 31 (co-location): stopped by memory pressure on the Studio; re-run after the A/B.

### 2026-11-20 ~11:40: the put in parts LANDED (359da2388); the takeover test next
- 359da2388: an object of any length is homed and read in bounded parts (appended `PutPart`/`ReadPart`, `Taken`/`Part`;
  a part at most the chunk cap), by `home_in_region`, `read_in_region` and the healer. A home's region holds now ride its
  shard image (piece kind 14), so a home's restart no longer frees them. Red:
  `daemon::tests::t_a724_an_object_past_a_region_message_is_homed_in_parts_and_read_back_after_a_restart` (2.5 messages
  in three parts, the daemon stopped, the successor reads it whole; with the region holds' restore disabled it answers
  NotHeld).
- f35a6ca31: w-git's flaky test fixed (event waits; 5/5 under load).
- The A/B and run 31 are queued by the balancer (r74, macOS, 12 cores, 24 GB), not on Studio-2.
- Next, the takeover test (NoLoss, StaleNeverCommits, TotalOrder) in crates/server/tests/fleet.rs. It needs the git
  HTTP edge on the in-process fleet daemons, a token for the owner, and a raw receive-pack client; none exist there
  today. The switch flips in the same commit that adds the passing test. ETA: this afternoon.

### 2026-11-20 ~13:00: the takeover test is RED on v2 records: a successor serves a taken-over repository with NO refs
Test: crates/cli/tests/v3_takeover.rs, on branch agents/w-balance-takeover-test (7e5ce95dc, not on main: it is red). Three
`slates anchor --fleet` processes at f = 1 with their HTTPS edges; a repository on one node; three pushes acknowledged
by stock git; a fourth in flight as the owner's anchor and daemon are SIGKILLed. Gated by SLATES_TEST_CLI.
Four runs, the same each time:
1. A survivor ADOPTS and REBUILDS the repository: `fleet.takeover.repository_adopted 1`,
   `fleet.takeover.repository_rebuilt 1`, failover about 3 to 5 s.
2. It serves the repository (ls-remote exit 0), but advertises NO refs at all. Every acknowledged push is lost (NoLoss
   fails). The other survivor answers "repository not found".
3. Credentials: a token minted on the owner is unknown on the successor (401), and a token minted on the successor names
   its own account (403, A-237). So the test grants every principal read and push before the kill; a real client loses
   access too.
So "committed push records reach the successor by construction" doesn't hold for repositories today, even with v2
records. The successor rebuilds from what it holds (`takeover::rebuild_repository` applies `RepositoryRebuilt
{ record, parts }` with the names/phases/markers/locks logs, then `vcs_store::rebuild` replays the object records its
own partition has, which appear to be none). Whoever owns A-636 should see this. The v3 switch stays off until this
test passes; v3 adds nothing to the failure. Shall I take it (ship the repository's push records and greens to the
successor), or does an A-636 owner?

### 2026-11-20: acknowledged pushes survive their owner's death: LANDED f95edf68e (fix + test together)
Cause, from counters: a push's records were kept only in the owner partition's register. The successor replayed none
(`vcs_store.replayed` absent, `repository_records` 0 everywhere) and served no refs.
Fix:
1. A push's record is a change of the names log (`LoggedChange::Objects`, appended, db wire order pinned). It ships
   to the repository's candidates with the operation's identity and spans. A takeover keeps or drops the push whole,
   and the successor's adoption puts the record back for replay.
2. A push is answered only once its names log is held at f+1 (`state_log::names_committed_within`; the HTTP and SSH
   edges wait on it before the report). Without it, the first run kept only the pushes whose parts had shipped.
`crates/cli/tests/v3_takeover.rs` passes 3 of 3 (NoLoss, TotalOrder, StaleNeverCommits by `git fsck` on a clone). It
keeps the labelled grant-all workaround until w-fed-faults' credential fix lands; it's that fix's red too.
Tests: slates-db all; slates-server takeover/state_log/vcs_store/git_push/vcs_http::git/vcs_ssh 236/236.
Next: the v3 switch on, its takeover runs going now.
