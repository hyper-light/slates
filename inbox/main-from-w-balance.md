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
