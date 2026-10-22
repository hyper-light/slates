# A dirty page in a mount's shared map is written back over a later write through another path

**Status:** fixed on Linux FUSE (A-139; its basis revised 2026-10-22 by A-140, after revision 3 refused a long-lived
map's write-backs). Open on macOS, where NFS v3 cannot carry the fix (below). Root cause confirmed by measurement on
both platforms and in the Linux 6.12 source.
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

## The fix (A-139, revised by A-140): a write-back is judged by what the mount's kernel holds of each byte

A cache write-back is marked: every page-cache write-back carries `FUSE_WRITE_CACHE` in its `write_flags`, whether
it is periodic, an `msync`, or the launder inside an invalidation (`fs/fuse/file.c:2081`). Its bytes are the kernel's
copy of the page as it was fetched, plus the process's stores since. So they are stale exactly where another path
changed the file after the kernel's copy of those bytes was fetched. The mount's own writes since are the exception,
because its own `write(2)` refreshed those bytes in its kernel.

The mount applies the write-back everywhere else and answers the whole size: the volume then holds what one shared page
cache would. In the parts:
- **Byte states.** For each file open for writing on the mount, `PageStates` (`crates/bridge-fuse/src/coherence.rs`)
  keeps, for every byte, the journal point the kernel's copy is as of and whether another path changed it since:
  - a READ served at head `h` makes its bytes (`h`, fresh);
  - the mount's own record at `s` makes bytes with an older point (`s`, fresh);
  - another path's record at `s` makes bytes with an older point stale.
- **Why a READ and an own write may reset bytes, and nothing else may.** Linux 6.12 keeps each write-back in the
  inode's in-flight tree until the daemon answers it (`fs/fuse/file.c:2124`; removed at `:1931`). A cache read
  (`:872`; readahead page by page at `:1020`) and a write (`:1141`) wait on that tree before they are sent. So when
  the daemon serves either, every earlier write-back of those pages has been answered. The end of an invalidation
  round gives no such guarantee (revision 2, below).
- **Fed as the journal is consumed.** The records come from `Bridge::data_changes` (`crates/bridge-core/src/changed.rs`):
  each data record after a point as its sequence, file and bytes, with a truncation or a shift as its tail.
  - The mount feeds them at each invalidation round's gather, from the same point in the same turn.
  - It feeds its own request's records as soon as the request is served.
  - It tells its own records from others' by bracketing each request with the journal's head (`OwnRecords`).
  - Once every record through the cursor is consumed, the points at or below it are raised to it, and the ranges they
    split merge. What remains is one range per run of stale or fresh bytes.
- **A clean start.** A file's first writable open on the mount clears `FOPEN_KEEP_CACHE` from its reply, so the kernel
  drops its cached pages as it opens it (`fs/fuse/file.c:279`). Every byte starts fresh as of the open.
- **Judging.** A write-back's stale bytes are the file's stale states within it, plus the records after the cursor (a
  few, not yet consumed) applied in order to a scratch copy.
- **The log.** The daemon's write log records only the parts applied (`Dispatched::written`), so a recovery never
  replays a stale byte.
- **Refusal.** A file is lost when its states would need more ranges than the journal's capacity, and every file is
  lost when the mount falls behind the journal's retention. A lost file's write-backs are refused `EIO` rather than
  guessed, until its last writable release; they are counted as `fuse.writebacks_unknown`. The bytes kept back are
  counted as `fuse.writeback_bytes_kept_back`.
- **Lifetime.** A file's states go with its last writable handle. A write-back holds its file open until it is
  answered, so the file's `RELEASE` comes after every write-back through it.
- **A restored mount.** A mount taken over across a daemon restart (A-61) has files whose opens predate this daemon.
  They have no states, so a write-back of one is judged as fresh at the mount's start against every record since. That
  is revision 3's walk, and it is refused past the journal's retention (owed, in the ledger).

**Measured-and-rejected: walking the journal back from each page's fetch (revision 3, `d7826c8b`).** It kept each
page's fetch point and, at write-back, walked the journal from that point. The journal keeps a 10‰ share of the volume's
quota (`JOURNAL_SHARE_PERMILLE`): 80 KiB, about 850 records, for an 8 MiB volume. A long-lived map has an old fetch
point, and that is exactly what DAX keeps. The test written for it,
`a_long_lived_maps_write_back_survives_unrelated_activity_past_the_journal`, stores a byte through a map, writes another
file through the SDK 2,000 times, then syncs. It failed **2 runs of 3** with `msync` answering `EIO`: the walk could not
reach back to the fetch, and the write-back was refused. Revision 4 consumes the records as the mount's rounds walk them
anyway, so no record older than the cursor is needed.

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

## Verification

**Revision 4** (A-140): a Linux arm64 container, 6 CPUs, fuse3, kernel 6.12 (Docker Desktop's linuxkit), 2026-10-22, on a
snapshot of the change (`d7826c8b` plus the working diff, hash `92c7a41a4369`), with the host's load average 14 to 33
from other sessions:
- **The long-lived map test**, `a_long_lived_maps_write_back_survives_unrelated_activity_past_the_journal`: green 25
  runs of 25 (20 alone, 5 with the other mount-route tests); under revision 3 it failed 2 of 3.
- **The page-at-a-time map test**, `a_map_fetched_a_page_at_a_time_under_activity_keeps_its_write_backs` (new): green
  10 of 10 (5 alone, 5 with the others). It reads a quarter more pages than the journal holds records, one READ each.
- **Revision 3's tests, unchanged:** the bug's test, the unsynced chaos history, the synced three-writer history and
  the two earlier mount-route tests, 5 rounds of all seven, all green; the shrink test 5 of 5.
- **The states' oracle** (`crates/bridge-fuse/src/coherence.rs`, 2,000 generated histories of fetches, the mount's own
  records, other paths' records and consumption, against a per-byte model that never raises a point), and two tests of
  the bound. Each red-checked on macOS:
  - letting another path's record mark a byte fetched after it fails the oracle, with a shrunk history;
  - raising no point fails the bound test, while the oracle still passes, as it must, since raising changes no answer;
  - raising points one past the cursor fails both.
- **By use, red-checked on Linux** (the bare-metal box: x86_64, kernel 7.2.9, each mutation built in a target directory
  of its own):
  - with `PageStates::consumed` raising nothing, the page-at-a-time test fails 3 of 3, `msync` answering `EIO` (the
    file lost past the bound);
  - with the feed counting every record as the mount's own, the bug's test fails 5 of 5 ("the later replacement
    survives the dirty page's write-back").
  A first run of the second check shared the first's target directory, compiled nothing and ran the first's binary,
  so its 5 passes were void. Cargo's records name the first tree's absolute paths, so a copy at another path is not
  isolated.
- **The same tree on that box:** workspace clippy and the doc gate clean; the `bridge-core` (11) and `bridge-fuse`
  (18) suites; the seven map tests in 3 rounds and the shrink test, all green. That is a kernel past 6.12, so the rule
  holds there too.
- **The feed**, `crates/bridge-core/tests/data_changes.rs`: sequences, ranges and tails through a real `VolumeBridge`,
  and `false` once the journal has let go of records since the point. Red-checked: with the eviction check removed,
  the retention test fails.
- **Regression:** the `bridge-core` (11 suites) and `bridge-fuse` (18) suites, `slates-vfs` (38), `slates-server --test
  fuse_mount` (3 of 3) and the recovery oracle (41 of 41), all green on Linux; clippy on the five crates the change
  touches, on Linux; the two crates' suites on macOS.

**Revision 3** (2026-10-21, the same container): the bug's test green 25 of 25 and red 5 of 5 with the rule disabled
(write-backs applied whole); under revision 2, the round-based mask, 1 of 10. One run of that red-check lost its
fixture's mount, not to the rule: `fusermount3` did not answer within the helper's 10 s deadline on the loaded host.

## Exact edits

- `crates/vfs/src/journal.rs`: `OpLog::oldest_seq` and `OpLog::capacity`; `since` uses the former.
- `crates/bridge-core`:
  - `src/changed.rs`: `DataChange`, `TO_THE_END` and `data_change`, the mapping from a journal record to the bytes it
    changed.
  - `src/lib.rs`: `Bridge::data_changes` and `Bridge::journal_capacity`, with defaults.
  - `src/volume_bridge.rs`: the implementations, `false` past eviction.
  - `src/scoped.rs`: delegation, keeping the changes within the scope.
  - `tests/data_changes.rs`.
- `crates/bridge-fuse`:
  - `src/abi.rs`: `WRITE_CACHE`.
  - `src/request.rs`: `WriteIn::write_flags`.
  - `src/bridge.rs`: `cache_writeback`, `unmasked_segments`, `serve_cache_write`, `refuse_with_eio`,
    `granted_handle`, `created_node`, `opens_for_writing` and `clear_keep_cache`.
  - `src/coherence.rs`: `OwnRecords` (with `took`), `WritableOpens` and `PageStates` (with `consumed`), with their
    tests.
  - `src/channel.rs`: `dispatch_request`, `note_opens_and_fetches`, `forget_unreported_open`, `note_own_records`,
    `feed_states` (called at each round's gather), `serve_writeback` and `stale_in_writeback`; `Dispatched::written`
    returns the applied parts; `reclaim_dispatched` takes the state mutably.
  - `tests/writeback.rs`; `tests/owner_turn.rs` follows `reclaim_dispatched`.
- `crates/server/src/fuse.rs`: `log_write` logs the applied parts; `fuse.writeback_bytes_kept_back` and
  `fuse.writebacks_unknown` are drained after each turn.
- `crates/cli/tests/cli.rs`: the five tests above; `MappedThroughAMount::sync_map`; `SecondMount::truncate`.

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
- Found while building revision 4, each in the ledger:
  - **`O_DIRECT` reads.** A direct READ looks like a cache READ on the wire but leaves the cached page in place, so
    counting it as a fetch can clear a staleness that page still holds. Found by reading the source, not reproduced;
    present since revision 3.
  - **The journal's budget.** The code takes 10‰ of the quota; §4.5 derives it from the mutation rate and the longest
    subscriber lag. The constant cites a ratification in GAPS §5 that is not there.
  - **The invalidation feed's eviction gap.** Under it, the write-back rule now refuses `EIO` rather than guessing; the
    feed itself still does not notice.
  - **A restored mount's files.** They are judged from the mount's start, as revision 3 judged every file.
  - **A takeover sends no invalidation.** A restored mount's first round starts at the new daemon's head, so a change
    the dead daemon had not yet delivered stays in the kernel's cache. Found by reading `crates/server/src/fuse.rs`,
    not reproduced.
- A performance observation, not a defect: the daemons' process ids rose by about 2,000 across the test with 2,000 SDK
  writes. That fits one notify thread started per invalidation round (the 2026-10-17 design), so a busy volume pays a
  thread start per change on each mount. Not measured further.
