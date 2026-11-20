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
