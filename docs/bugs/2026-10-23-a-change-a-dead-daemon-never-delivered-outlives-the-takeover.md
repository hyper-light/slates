# A change a dead daemon never delivered outlives the takeover

**Status:** fixed 2026-10-23 (A-158). GAPS: "a takeover sends the kernel no invalidation", closed.

## Description

A FUSE mount survives its daemon's death (A-61): the anchor holds the device, and the successor takes the mount over.
The successor never tells the mount's kernel anything about changes the dead daemon made but had not yet delivered.
Under the `Invalidated` cache posture the kernel keeps names, attributes and pages with unbounded lifetimes, so it goes
on serving the old state indefinitely:
- the bytes of a file another path replaced;
- a name another path renamed away.

Reproduced on the box (Linux 7.2.9-2-cachyos), 4 runs of 4, by two new tests in `crates/cli/tests/cli.rs`:
- `a_replacement_the_dead_daemon_never_delivered_does_not_outlive_the_takeover`:
  - read `f.txt` through a held mount;
  - replace it through the SDK with bytes of the same length;
  - kill the daemon one syscall after the reply.

  After the takeover: "cycle 1: the mount serves "00000000" after the takeover, not the replacement "00000001" its dead
  daemon never told it of", still stale after `WAIT_FOR`, in both runs.
- `a_rename_the_dead_daemon_never_delivered_does_not_outlive_the_takeover`: the same with an SDK rename. After the
  takeover the mount still named the old file, in cycle 2 of one run and cycle 1 of the other.

## Root cause

- **The undelivered window.** A change's invalidation reaches a mount's kernel about 82 µs after the writer's reply
  (A-142's measurement). A daemon that dies inside that window has made the change durable but never told the kernel.
- **The successor forgets the cursor.** A restored mount's coherence cursor is process-local. Its first round starts at
  the successor's head (`Coherence` gathers from `bridge.seen` at its first round), and the successor's journal starts
  empty after recovery (`OpLog::new` in `Volume::recovery_shell`). So nothing names the change.
- **A-141 cannot fire.** Its "tell everything" needs the mount's `KernelHolds`, which starts empty on a restored mount,
  since the kernel's lookups predate the successor (`ServeState::restored`).
- **Nothing runs at all.** A restored mount runs no turn until a request or a volume change wakes it. A kernel serving
  from its cache sends neither.

## What a fix can know (the analysis behind it)

- **Every acknowledged change is durable before its reply.** A mutation's reply waits for its barrier, which publishes
  (`a_mutations_reply_waits_for_its_barrier_and_a_refused_barrier_answers_eio`). A FUSE data write is logged in the
  write log before its reply (A-63).
- **A published change carries the mount's references.** A publication records each attachment's references with it
  (A-61, A-152). A kernel holding stale bytes of an inode had looked the inode up before the change, so the change's
  publication recorded that reference.
- **A logged write is in the replay.** A FUSE write logged after the last publication is replayed into the volume at
  recovery (`replay_writes`).
- So the inodes a restored mount's kernel can hold stale state for are within two sets: the restored references of
  the mount's owner, and the inodes the replay wrote.
- **Names are another matter.** A renamed-away name is in no such set. Linux FUSE 7.44 added `FUSE_NOTIFY_INC_EPOCH`
  (the box's `/usr/include/linux/fuse.h`: "7.44 - add FUSE_NOTIFY_INC_EPOCH", code 8). It invalidates every dentry
  the kernel caches, each revalidated at its next use (LKML, Luis Henriques' patch v8, 2025-02-26). Linux v7.2-rc2
  also checks the epoch against cached readdir results; on older kernels a directory's own invalidation drops them.

## Impact

- **Stale bytes, attributes and names on any mount restored after a crash inside the window.** The window is about
  82 µs after each change made through another path (the SDK, NFS, a second mount, a merge). The staleness has no end
  short of the kernel's own eviction, because the posture's lifetimes are unbounded.
- **A write-back over a stale page** is judged against byte states that never saw the change. Since A-156 they start
  fresh as of the mount's start, so such a write-back would land, undoing the change.
- **No data in slates is lost:** the successor holds the change. The kernel's view is what is wrong.

## Exact edits

- `crates/bridge-fuse`:
  - `init.rs`: `InitNegotiation::kernel_minor`, the minor the kernel offered (the agreed one stays `min(kernel, 31)`);
  - `session.rs`: `Session::kernel_bumps_epoch` (offered minor ≥ 44, `EPOCH_MINOR`), carried across the takeover as bit
    `1 << 2` of format 1; golden vectors for all 8 sessions (the 4 older ones unchanged), hostile and bit-flip tests;
  - `notify.rs`: `Notify::IncEpoch` (8) and `inc_epoch`, a bare header;
  - `channel.rs`: a restored mount's rounds open with a slice of its takeover round until it is sent whole
    (`takeover_told`, `Takeover`, `TakeoverSlice`). The first slice carries the epoch bump (where the kernel takes it)
    and the root's invalidation. Every slice carries inode invalidations with all data, walked in inode order from
    where the last ended, for at most the owner's budget (`ServeState::slices_within`) and at least one inode.
    `ServeState::owes_takeover_round` holds while a slice is owed and no round is out; `take_takeover_counts` reports
    the bumps, the unsupported bumps and the inodes.
- `crates/vfs/src/volume.rs`: `Volume::replayed` with `mark_replayed`, and `Volume::visit_takeover_inodes(owner, after,
  visit)`: the owner's references merged with the replayed inodes, in order, each once, resumable after an inode.
- `crates/bridge-core`: `Bridge::takeover_inodes(cx, after, visit)`, defaulting to none; `VolumeBridge` walks the
  volume's set for `cx.owner`; `ScopedBridge` and the test bridge delegate.
- `crates/server/src/fuse.rs`:
  - `wait_for_work` answers `Woke::Change` while the mount owes a slice, so the round starts at once, without a request
    or a change;
  - each turn bounds the slice to half the shard's live step quantum (`TAKEOVER_SLICE_PERMILLE`);
  - counters `fuse.epoch_bumped`, `fuse.epoch_unsupported`, `fuse.takeover_invalidated`.
- `crates/server/src/verbs.rs`: `replay_one` marks every inode a logged write names, landed or refused.

## Verification (the box: Linux 7.2.9-2-cachyos, Ryzen 9 9950X3D2, load 15–82 from other sessions)

- **Red, on the final test text**, in a scratch tree with this change's source files restored to `HEAD`:
  - `a_rename_the_dead_daemon_never_delivered_does_not_outlive_the_takeover`: "cycle 1: after the takeover the mount
    still names n0.txt (or not yet n1.txt)";
  - `a_replacement_the_dead_daemon_never_delivered_does_not_outlive_the_takeover`: "cycle 1: the mount serves
    "00000000" after the takeover, not the replacement "00000001"" (and, on a run whose kill missed the window, its
    takeover-round counter at 0);
  - `a_generated_history_of_changes_and_kills_leaves_the_mount_seeing_what_the_volume_holds`: "seed
    0x9e3779b97f4a7c15, cycle 1: ... c6.txt: "06v00000" through the kept descriptor, "06v00000" by name, not
    "06v00001"".
- **Green with the fix:**
  - the two kill-in-window tests, 3 of 3 runs at load 41–55, 5 takeovers each. Every takeover bumped the epoch once
    and invalidated 2 inodes (the root and `f.txt`).
  - the chaos test, 3 of 3 runs, 8 takeovers each. Every takeover round invalidated 13 inodes (12 held files and the
    root).
  - `crates/bridge-fuse/tests/takeover_round.rs` (a socket pair as the device), 3 of 3 runs: the epoch bump first and
    once, the root and 40 files each once and nothing else, in 40 slices at a zero budget; the counts `(1, 0, 41)`, or
    `(0, 1, 41)` for a kernel before 7.44. Mutation-checked: resuming the walk at the cursor rather than after it
    re-sends the last inode forever (`(0, 0, 1)` still counted in the quiet window); an unbounded slice sends 41 at
    once.
  - `slates-bridge-fuse` (140 tests), and the crosvm guest DAX tests 2 of 2 on the final code, which take a mount over
    under a guest's live DAX map;
  - the full `slates-cli` suite: 8 of 8 runs after the premise fix, 6 of 6 after the slicing, then 18 of 20 with the
    chaos test. The 2 failures were in two older tests, `refused: NotFound` from a CLI attachment the reaper removed
    once its `attach` process exited. A 2.5 s pause after `attach` reproduces it 3 of 3; 16 interleaved runs at load
    51–76 passed, 8 with the chaos test and 8 without. Opened in GAPS as its own entry.

## A test premise found on the way

Under the full suite the replacement test first failed its non-vacuity check: the takeover round named 1 inode, not 2,
4 runs of 6. A trace of the references showed why. The dead daemon saw two lookups of the just-created `f.txt`, then two
`FORGET`s of one each (owned 2, 1, 0), before the first cycle's publication. The mount's round for the SDK's create
reached the kernel late under load, after the test's lookup. It invalidated the name the kernel had just looked up, and
the kernel evicted the unused inode. So the kernel held nothing of the file, and the set was right to leave it out.
The tests now wait until the mount's journal reader is at the head before caching anything, and the replacement test
holds the file open, so the premise holds: 8 of 8 suite runs after.

## Siblings (reported, not fixed here)

- **A dirty page from before the takeover.** The round's inode invalidation launders a dirty shared-map page, and the
  write-back is judged by A-156's fresh-at-start states. So its stale bytes can undo an undelivered change in the same
  window. The round makes this happen sooner, not differently: the kernel would write the page back anyway. The
  precise fix marks as stale, in adopted files' states, the ranges of the last publications and of the replayed writes.
- **An effect the dead daemon served before its publication.** A mutation is applied, then published before its reply.
  A mount that read it in between, before the daemon died unpublished, holds bytes the successor lacks. If the kernel
  looked the inode up after the last publication, the set does not name it.
- **An outsider's change beneath a base during the gap**, to a base file the kernel looked up after the last
  publication: no reference names it, and the successor's watcher did not run in the gap.
- **Kernels before FUSE 7.44** keep a name renamed or removed away after a takeover; counted as
  `fuse.epoch_unsupported`. So does a mount whose session a daemon before A-158 recorded, until it is mounted again.
- **Unsliced rounds.** A-141's `everything_told` and the journal gather each build a whole round in one turn, against
  the bounded-work rule this change follows for the takeover round.
- **A CLI attachment dies about a second after its `attach` process exits** (above): a dead client's attachments are
  removed by design, so `docs/cli.md`'s `attach` then `write`/`detach` from later processes races the reaper.
- **A create's entry invalidation.** Since A-137 no lookup miss is cached, so the entry invalidation that follows
  another path's create can only drop a positive name the kernel looked up after the create. The kernel then evicts the
  inode and looks it up again, which is the premise failure above.
