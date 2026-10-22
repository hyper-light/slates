# A mount that falls behind the journal never hears of the changes it missed

**Status:** fixed 2026-10-22 (A-141; option 2 below, approved by Ada in plan mode). Root cause confirmed by logs.
**Severity:** a FUSE mount serves an acknowledged write's old bytes indefinitely. The kernel holds them with an
unbounded lifetime under the `Invalidated` posture, so nothing expires them. DAX through a mount maps that stale page
into the guest.
**Found:** 2026-10-22, adversarial testing of the invalidation feed at its bound, taken from the ledger's open item
("the invalidation feed's eviction gap", recorded while fixing
`docs/bugs/2026-10-21-a-dirty-shared-map-page-overwrites-a-later-write-through-another-path.md`).

## What happens

A FUSE mount learns of another path's change by walking the volume's journal from its cursor at each of its rounds
(§4.6 "Cache posture"; `Bridge::invalidations`, `crates/bridge-core/src/volume_bridge.rs`). The journal keeps a
bounded share of the quota and drops its oldest records past that. If more changes than the journal keeps land between
two of a mount's rounds, the oldest are gone before the mount gathers them. `OpLog::since` then starts at the oldest
record still held, and the cursor moves on to the head. The kernel is never told of the missing changes, and it keeps
serving what it cached before them.

## The evidence

The bare-metal Linux box (x86_64, kernel 7.2.9, load average 25 to 34 from other work), 2026-10-22. The test
`a_change_reaches_the_mount_even_when_more_follow_than_the_journal_keeps` (`crates/cli/tests/cli.rs`) does this:
- it uses a volume with a 256 KiB quota, whose journal is the one-page floor, some 46 records;
- it caches a 64-byte `a.bin` through a FUSE mount;
- it pipelines a replacement of `a.bin` and 31 replacements of `b.bin` through one SDK client (one ring's worth;
  each replacement journals two records), then collects every reply;
- it reads `a.bin` through the mount for up to 10 s.

It failed **2 runs of 33**: the mount served the old bytes for the whole 10 s.

A temporary trace of the journal's appends and of each gather (removed) shows the mechanism. `a.bin` is inode
`…658`, and its replacement is records 8 and 9:

| | gathers |
|---|---|
| a passing run | `since 7 head 9` (the mount ran between `a.bin`'s write and the batch), then `since 9`, oldest held `26`, head `71`: only `b.bin`'s records were lost |
| the failing run | `since 7`, oldest held `16`, head `61`: records 8 to 15, `a.bin`'s among them, were gone before the mount gathered |

Eviction itself was common: 10 of 12 traced runs lost records. The failure needs the change the kernel caches to be
among those lost. That happens when the mount does not run between that change and the rest of the batch.

## Root cause

The journal's retention is set by the quota alone (`JOURNAL_SHARE_PERMILLE`, `crates/server/src/verbs.rs`), not by its
readers. Nothing stops it dropping records a live mount has not read, and the mount's gather does not notice when that
happens. §4.6 allows unbounded cache lifetimes only where invalidation delivery is proven. That proof fails once a
mount falls behind retention. §4.5 sizes the journal from "mutation rate × max subscriber lag × record size", but no
code holds that, and nothing says what a reader must do when it falls behind anyway.

## Impact

- Any FUSE mount on Linux whose round falls behind by more records than its volume's journal keeps:
  - a small volume, where the journal is a page (one client's pipelined ring of writes does it);
  - a busy one, where several writers outpace one shard's turn;
  - one verb that journals many records at once, such as a merge that applies a large change set to a mounted volume.
- DAX through a mount: a guest's DAX window maps the host's stale page.
- The write-back rule (A-140) already refuses `EIO` here, since it loses its byte states rather than guess. The
  invalidations are what is lost silently.

## The fix (A-141): an overrun is reported, and the mount drops everything its kernel may hold

- **Detect** (`crates/bridge-core`). For the current view, a gather whose cursor is older than the journal's oldest
  record pushes `Invalidation::Everything` instead of walking, and returns the head. `ScopedBridge` passes it through.
  The base plane needs nothing: `stale_since` keeps each directory's latest hint, so it cannot overrun.
- **Know what the kernel holds** (`crates/bridge-fuse`). `KernelHolds` (`coherence.rs`) follows the FUSE lookup
  protocol as each reply reaches the kernel:
  - every entry reply (LOOKUP, MKNOD, MKDIR, SYMLINK, LINK, CREATE, TMPFILE, each READDIRPLUS entry) adds a lookup and
    its name;
  - `FORGET` and `BATCH_FORGET` give lookups back, and a node with none left goes with every name it had;
  - the mount's own UNLINK, RMDIR and RENAME take names away or move them;
  - an invalidation of a name takes it away when its round is built. A lookup answered while the round is out then adds
    the name back, so a name the kernel holds is never missing.

  The reader is `kernel_changes` (`bridge.rs`). It reads entries where `reclaim_unreported` reads them. It is fed only
  for a reply the kernel received, on both serve paths (`serve_request`; `send_reply` for the daemon), so a reply that
  is reclaimed is never counted as held.
- **Expand** (`channel.rs`). An `Everything` becomes:
  - an attribute-and-data invalidation of every tracked node and of the root;
  - an entry invalidation of every tracked name, expire-only where negotiated.

  It goes out on the notify thread like any round. Its `ENOENT`s are expected and not counted as refusals. Each overrun
  is counted as `fuse.journal_overruns`.
- **Bounded by the volume.** Nodes are volume inodes, so they are at most its inode allowance. Names are its entries
  naming referenced nodes, plus names in flight until their next invalidation.

**Weighed and not taken.**
- *Retention waits for live readers:* writers would wait on mounts (the AC-3.3 machinery), and a mount whose round is
  blocked in its kernel would hold every writer.
- *Changes pushed per mount as they are journaled:* a second record of every change beside the journal, kept per mount
  for as long as it lags.

## Verification (2026-10-22)

**The bare-metal Linux box** (x86_64, kernel 7.2.9, load average 16 to 28 from other work). Each tree was built in a
target directory of its own.
- **The bridge test, deterministic, every host** (`a_transport_behind_the_journals_retention_is_told_everything`): red
  before the fix, where the gather returned only the retained records of another file, and green after.
- **The real-mount test, deterministic** (`bridge-fuse/tests/coherence_mount.rs`,
  `a_mount_behind_the_journal_drops_everything_its_kernel_holds`). The serve loop applies, in one wake, a replacement
  of a cached file, a removal of a cached name, and twice the journal's records of writes elsewhere. Green on the final
  tree. Each red-check failed 3 runs of 3:
  - **the overrun not reported:** the test's overrun count stays 0;
  - **the expansion without nodes:** the replaced file's page is still served after 10 s;
  - **the expansion without names:** the removed name is still visible after 10 s.
- **The CLI test** (`a_change_reaches_the_mount_even_when_more_follow_than_the_journal_keeps`): green 40 runs of 40
  (30 before its refactor into a fixture, 10 after), against 2 failures in 33 before the fix.
- **`KernelHolds` against a model** (2,000 generated histories of handed-out entries, forgets, own unlinks, renames and
  exchanges) and **the reply reader by round trip** through a real volume bridge (every entry-granting opcode,
  READDIRPLUS, TMPFILE, an error reply, the mount's own unlink, rename and exchange, FORGET and BATCH_FORGET): green on
  macOS and Linux.
- **Regression on the box:** the `bridge-core` (11) and `bridge-fuse` (19) suites, the seven map tests (2 rounds) and
  the truncation test, `slates-server --test fuse_mount` (3 of 3) and the recovery oracle (41 of 41). Workspace clippy
  and the doc gate clean.

**macOS:** the two crates' suites, fmt, workspace clippy, the doc gate and `cargo xtask check`.

## Exact edits

- `crates/bridge-core`:
  - `src/lib.rs`: `Invalidation::Everything`;
  - `src/volume_bridge.rs`: the overrun check in `invalidations`;
  - `src/scoped.rs`: passes it through;
  - `tests/invalidation.rs`: the deterministic test;
  - `tests/volume_bridge.rs`: its exhaustive match.
- `crates/bridge-fuse`:
  - `src/coherence.rs`: `KernelHolds` with its model test;
  - `src/bridge.rs`: `KernelChange` and `kernel_changes`;
  - `src/channel.rs`: `ServeState::holds`, `journal_overruns` and `take_journal_overruns`; `note_delivered`, fed from
    `serve_request` and `send_reply`; `everything_told`; names taken at a round's build in `deliver_owed`; the two
    matches;
  - `tests/kernel_changes.rs` (new); `tests/coherence_mount.rs` (the new test, the mount setup shared through
    `with_mounted_loop`, a scratch directory per test); `tests/common/mod.rs` (`JOURNAL_BYTES`).
- `crates/server/src/fuse.rs`: `fuse.journal_overruns`, drained after each turn.
- `crates/cli/tests/cli.rs`: `OnePageJournal` and the CLI test.
- Docs: §4.6, A-141, GAPS, this record, and the dirty-page record's sibling list.

## Reproduction

`SLATES_TEST_CLI=1 cargo test -p slates-cli --test cli a_change_reaches_the_mount -- --test-threads=1` on Linux with
FUSE, repeated: 2 failures in 33 runs before the fix. Deterministically:
`cargo test -p slates-bridge-fuse --test coherence_mount a_mount_behind` with the overrun check removed.
