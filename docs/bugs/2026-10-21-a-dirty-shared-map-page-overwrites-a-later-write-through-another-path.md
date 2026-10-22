# A dirty page in a mount's shared map is written back over a later write through another path

**Status:** fixed on Linux FUSE (A-139). Open on macOS, where NFS v3 cannot carry the fix (below). Root cause
confirmed by measurement on both platforms and in the Linux 6.12 source.
**Severity:** loss of an acknowledged write. Bytes another path wrote, including bytes the map never touched,
revert to their earlier value.
**Found:** 2026-10-21, adversarial testing of the mount route for DAX (`docs/wip/virtiofs.md` §8 item 7).

## What happens

A process holds a writable `MAP_SHARED` map of a file through a slates mount and stores into it. So does a VM's
sharer, for DAX. The store makes the page dirty in that mount's kernel cache, and nothing reaches slates until the
page is written back. If another path writes the same page meanwhile (the SDK, another attachment, another mount),
that write is applied and acknowledged. Then the mount's kernel writes its dirty page back. That is the whole page:
the bytes from before the other write, plus the store. Slates applies it too, and the other write's bytes on that
page are gone.

On one local filesystem this cannot happen. Both writers share one page cache, so the later write overwrites the
store and no byte reverts.

## The evidence

A probe over a real CLI, anchor, daemon and mount, 2026-10-21. A two-page `map.bin` of `A` is mapped `MAP_SHARED`.
One byte `M` is stored through the map at offset 10, with no `msync`. The whole file is then replaced with `S`
through the SDK. After about 200 ms of waiting (on macOS with an `fstat` every 20 ms), slates' own read, then an
`msync`, then slates' own read again:

| | page 0 | page 1 |
|---|---|---|
| expected (one page cache) | all `S` | all `S` |
| macOS, NFS v3, 16 KiB pages, 2 runs | 16,383 `A` + 1 `M` | all `S` |
| Linux, FUSE, arm64 container, 4 KiB pages, 3 runs | 4,095 `A` + 1 `M` | all `S` |

On both platforms the stale page was already in the volume **before** the `msync`:

- **macOS:** the NFS client wrote its dirty page back when the `stat` made it revalidate.
- **Linux:** the stale page arrived within the 200 ms after the SDK's write, with no sync. That fits the kernel
  writing the dirty page back (laundering it) when slates' invalidation of that page arrives, which is the change
  notification `7deddf54` delivers promptly. The kernel's periodic writeback (30 s dirty expiry) is too slow to
  explain three runs of three; the kernel itself was not traced. Before `7deddf54` the notification waited for the
  mount's next request, so the stale page came back later, at the next writeback or sync: the same loss, later.

## Root cause

Slates applies a write from one path without first collecting the dirty state other paths' caches hold for the same
bytes. The design has that protocol for one cache only, an NFSv4 client's write delegation. The recall gate (§4.6
A-79, `crates/vfs/src/recall_gate.rs`) refuses a change to a delegated file until the delegation is returned, and
RFC 8881 §10.2.1 obliges the client to flush its modified data before returning it. So the client's dirty pages
arrive before the other write, not after it.

FUSE and NFS v3 mounts hold write-back state with no recall:

- A **FUSE** mount's only dirty pages are writable shared maps. Slates refuses the writeback cache at `FUSE_INIT`,
  so `write()` is write-through.
- An **NFS v3** client buffers writes and dirty map pages until it commits, and the protocol has no way to call it
  back.

## Impact

Affected:

- **DAX through a mount.** A guest's DAX stores are dirty pages of the host's mapping until written back, so a
  host-side writer of the same file (an agent through the SDK, a tool through another mount) can have its
  acknowledged bytes reverted.
- **Two host processes sharing a file across paths,** when one writes through a shared map.

Not affected:

- Writers that all go through one mount (one page cache).
- `write()` through a FUSE mount (write-through).
- An NFSv4 client holding a delegation (the recall gate).

## The fix (A-139): a write-back is judged by when its page was fetched

A cache write-back is marked: every page-cache write-back carries `FUSE_WRITE_CACHE` in its `write_flags`, whether
it is periodic, an `msync`, or the launder inside an invalidation (`fs/fuse/file.c:2081`). Its bytes are the kernel's
copy of the page as it was fetched, plus the process's stores since. So they are stale exactly where another path
changed the file after the daemon served that page's fetch. The mount's own writes since are the exception, because
its own `write(2)` refreshed those bytes in its kernel.

The mount applies the write-back everywhere else and answers the whole size: the volume then holds what one shared page
cache would. In the parts:
- **Fetch points.** While a file is open for writing on a mount, each READ served records the journal's head as the
  fetch point of the bytes it returned (`FetchPoints`, `crates/bridge-fuse/src/coherence.rs`).
- **A clean start.** A file's first writable open on the mount clears `FOPEN_KEEP_CACHE` from its reply, so the kernel
  drops its cached pages as it opens it (`fs/fuse/file.c:279`). Every page cached after that has a known fetch, or was
  written whole by the mount.
- **The walk.** What changed after a point is a walk over the volume's journal (`crates/bridge-core/src/changed.rs`,
  `Bridge::changed_since`): another path's write, truncation or shift adds its bytes, and the mount's own records clear
  theirs. The mount knows its own records by bracketing each request it serves with the journal's head (`OwnRecords`).
- **The log.** The daemon's write log records only the parts applied (`Dispatched::written`), so a recovery never
  replays a stale byte.
- **Refusal.** Where the journal no longer holds every change since a page's fetch, the write-back is refused `EIO`
  rather than guessed (counted as `fuse.writebacks_unknown`). The bytes kept back are counted as
  `fuse.writeback_bytes_kept_back`.
- **Lifetime.** A file's fetch points go with its last writable handle. A write-back holds its file open until it is
  answered, so the file's `RELEASE` comes after every write-back through it.

**Measured-and-rejected: judging by invalidation rounds (the approved revision 2).** The first built version masked a
write-back over the changes made after the mount's last *completed* invalidation round. The bug's test still failed
**9 of 10 runs**. A temporary trace of each link showed the stale write-back arriving after its round had ended, with
an empty mask, and applied whole. The cause is in Linux 6.12: FUSE writes a page back from a temporary copy and ends
the page's write-back at once (`fs/fuse/file.c:2092`, `fuse_writepage_locked`; the batched path likewise). An
invalidation that comes next finds the page clean, drops it without laundering (`mm/truncate.c:645–656`), and returns,
while the stale copy is still queued. A round's end says nothing about write-backs in flight. A page's fetch point does.
The one passing run had passed by timing: the store's page was written back before the replacement.

**What remains, stated in the design.** A store into the very bytes another path is changing, made after its page was
fetched and before the change is delivered, is kept back too. That is two writers racing on the same bytes. For
writers that hold no kernel lock it closes once they are answered after delivery (AC-3.3, open in the ledger).

**macOS (open, Ada's decision).** NFS v3 has no write-back mark and no callback, so the server cannot tell a stale
write-back from a new write. The options are NFSv4 with delegations, where the recall gate already does this (the Mac
client is v4.0, which D-2 rejects), or recording such sharing as unsupported on the Mac.

## Verification (Linux arm64 container, 6 CPUs, fuse3; 2026-10-21)

- **The bug's test**, `a_dirty_shared_map_page_never_undoes_a_later_write_through_another_path`:
  - green 25 runs of 25 (20 alone, 5 with the other mount-route tests);
  - red 5 of 5 with the rule disabled (write-backs applied whole);
  - under revision 2, the round-based mask, 1 of 10.
- **Shrink**, `a_dirty_page_past_a_truncation_never_grows_the_file_back`: 5 of 5.
- **Chaos**, `unsynced_map_stores_a_second_mount_and_the_sdk_agree_at_every_sync_point`: 5 of 5, with the synced
  three-writer history and the two earlier mount-route tests likewise.
- **Oracles, each red-checked:**
  - the walk (`crates/bridge-core/src/changed.rs`, 2,000 generated histories against a per-byte model) fails when own
    writes stop clearing;
  - fetch points (`crates/bridge-fuse/src/coherence.rs`, 2,000 histories) fail when neighbours of different points
    merge;
  - the by-use walk (`crates/bridge-core/tests/changed_since.rs`) fails when the eviction check goes.
- **Regression:** the `bridge-core`, `bridge-fuse` and `vfs` suites, `slates-server --test fuse_mount` (3 of 3) and the
  recovery oracle (41 of 41), all green on Linux; the three crates' suites and the mount-route tests on macOS.
- **One run in the red-check lost its fixture's mount**, not to the rule: `fusermount3` did not answer within the
  helper's 10 s deadline on the loaded host, and slates refused the mount. That deadline under load is a separate
  observation.

## Exact edits

- `crates/vfs/src/journal.rs`: `OpLog::oldest_seq` and `OpLog::capacity`; `since` uses the former.
- `crates/bridge-core`:
  - `src/changed.rs`, new: `Changed`, `TO_THE_END`, `changed_ranges`, and its oracle.
  - `src/lib.rs`: `Bridge::changed_since` and `Bridge::journal_capacity`, with defaults.
  - `src/volume_bridge.rs`: the implementations, `Unknown` past eviction.
  - `src/scoped.rs`: delegation behind the scope check.
  - `tests/changed_since.rs`, new.
- `crates/bridge-fuse`:
  - `src/abi.rs`: `WRITE_CACHE`.
  - `src/request.rs`: `WriteIn::write_flags`.
  - `src/bridge.rs`: `cache_writeback`, `unmasked_segments`, `serve_cache_write`, `refuse_with_eio`,
    `granted_handle`, `created_node`, `opens_for_writing` and `clear_keep_cache`.
  - `src/coherence.rs`: `OwnRecords`, `WritableOpens` and `FetchPoints`, with their tests.
  - `src/channel.rs`: `dispatch_request`, `note_opens_and_fetches`, `forget_unreported_open`, `note_own_records`,
    `serve_writeback` and `stale_in_writeback`; `Dispatched::written` returns the applied parts; `reclaim_dispatched`
    takes the state mutably.
  - `tests/writeback.rs`, new; `tests/owner_turn.rs` follows `reclaim_dispatched`.
- `crates/server/src/fuse.rs`: `log_write` logs the applied parts; `fuse.writeback_bytes_kept_back` and
  `fuse.writebacks_unknown` are drained after each turn.
- `crates/cli/tests/cli.rs`: the three tests above; `MappedThroughAMount::sync_map`; `SecondMount::truncate`.

## Reproduction

The probe, as it was run and removed, over the `MappedThroughAMount` fixture in `crates/cli/tests/cli.rs` (`0106c51f`),
with `SLATES_TEST_CLI=1`. It landed as the fix's failing test,
`a_dirty_shared_map_page_never_undoes_a_later_write_through_another_path`.

```rust
let mut fixture = MappedThroughAMount::with_pages("mapdirty", b'A', 2);
fixture.view()[10] = b'M';               // a store, left dirty: no msync
fixture.replace_through_slates(b'S');    // a later write through another path, acknowledged
for _ in 0..10 {
  if !cfg!(target_os = "linux") {
    let _ = rustix::fs::fstat(&fixture.file);
  }
  pause();
}
let before_sync = fixture.volume_bytes(); // page 0 already stale: 'A' except the 'M'
// msync(map, length, MS_SYNC), then fixture.volume_bytes() again: unchanged.
// Correct: every byte 'S'.
```

## Siblings (reported, not checked here)

- The direct virtio-fs device (`crates/bridge-virtiofs`) and WinFsp: whether a guest's or a Windows client's
  write-back state has the same hazard.
- AC-3.3 (the mount told after the writer is answered): the same "park a writer on a mount" machinery.
