# A FUSE mount hears of another path's change only on its next kernel request

**Status:** fixed (this change). Root cause confirmed by measurement and code; the fix verified on a real Linux FUSE
mount, red-checked.
**Severity:** a correctness defect, not a security one. On an idle Linux FUSE mount, a change made through any other
path stays invisible to readers the kernel serves from its cache — indefinitely.
**Found:** 2026-10-21, proving the mount route for DAX (`docs/wip/virtiofs.md` §8 item 7).

## What happens

A slates FUSE mount on Linux keeps the kernel's cache lifetimes for pages, attributes and entries long, and relies
on telling the kernel when something changes: an unsolicited `FUSE_NOTIFY_INVAL_*` notification. Those
notifications are delivered only when the mount's serve loop runs, and in the daemon the loop runs only when the
kernel sends a request. A change made through another path — another attachment's `slates write`, the SDK, the NFS
transport, a merge — never wakes it. So its notification waits, owed but undelivered, until the mount happens to get
a request for anything at all.

Readers that go through the kernel's cache without sending a request see the old bytes for as long as that takes:
- a process holding a shared memory map of the file;
- **DAX through the mount** — a VM's sharer maps the host file into the guest's window, and the guest's reads are
  memory loads that send nothing;
- a long-lived descriptor re-reading a file whose pages are cached.

An `open`, a `stat` or a listing is a request, so open-read-close readers are mostly unaffected, which is likely why
this went unnoticed.

## The evidence

`probe_a_shared_map_through_a_mount` (`crates/cli/tests/cli.rs`), a real CLI, a real anchor and daemon, and a real
FUSE mount, in an arm64 Linux container (`--device /dev/fuse`, fuse3), 2026-10-21. The volume's `map.bin` is mapped
`MAP_SHARED` through the mount; then `slates write` replaces it through another attachment:

| reader | first saw the change |
|---|---|
| the map, read as memory only | **never** — 8 s of polling, two runs |
| the same map, after a listing of the mount **root** — a different object, `map.bin` untouched | **117 µs** |
| the same map, with the file `fstat`ed | 22 ms; 43 µs on the second run |

The middle row is the proof of the cause: the notification for `map.bin` was waiting, and a request about something
else entirely let it through.

The write direction is sound on Linux and macOS alike: a write through the shared map, `msync`ed, reads back through
`slates read` — bypassing the mount — as written.

## Root cause

`crates/server/src/fuse.rs`, `serve`: the mount's task waits on one thing, `slates_rt::readiness::readable(raw)`, the
FUSE device. Delivery happens in the turn that follows.

`crates/bridge-fuse/src/channel.rs` provides the second wake source, `ChangeSignal`/`ChangeNotifier`, and its module
documentation describes exactly this case: *"a change another mutation source signalled through a `ChangeSignal` —
… a change through the SDK or another attachment … — … so … a change while the kernel is answering from its cache is
told without waiting for a request."* The daemon never creates one, and no mutation path ever calls `notify`. Every
use is in bridge-fuse's own tests (`tests/ipc_mount.rs`, `tests/coherence_mount.rs`). The mechanism was built and
proven in isolation, and never wired into the daemon.

## The design rule it breaks

§4.6, the Linux FUSE cache posture: *"Infinite cache lifetimes require proven invalidation delivery and recovery for
every mutation source"*, and *"Positive entries, attributes, pages and listings keep their lifetimes, because the
kernel honours their invalidation."* The long lifetimes are justified by a delivery that, for every mutation source
but the mount's own kernel, does not happen until something else wakes the loop.

## Siblings to check (reported, not fixed here)

- **Outsider changes beneath a base directory.** The channel's documentation lists them as the other source that wakes
  the loop through a `ChangeSignal`. Since the daemon has no signal, they presumably wait for a request too.
- **macOS has the same symptom with a different cause.** The NFS v3 client cannot be told of a change at all. It
  revalidates on its own attribute timeout (`actimeo=1`) when something asks, and a memory-only reader there was seen
  both never refreshing in 8 s and refreshing at 0.88 s, depending on what else touched the file. Not fixable on the
  server side over v3; recorded so it is not mistaken for this bug.

## The fix

**A volume wakes whoever waits on it, at the one place every change passes through.** Every mutation reaches the
journal through `Volume::record`, and every outsider change beneath a base directory advances the base plane's hint
sequence. Both now move a change generation and wake every parked waiter (`ChangeWaiters`, in the shape of the recall
gate's `wait_for_release`). Because the hook is where changes are journaled, no mutation path — SDK, another attachment,
NFS, a merge, the watcher — can forget to signal: there is nothing for it to remember.

**The mount's task waits on three sources instead of one** — its device, its `ChangeSignal`, and the volume's change
generation past what it last delivered — and on a change runs a *change turn*: the same gating as a request turn,
calling the existing `serve_step(…, Wake::Change)`, which delivers the owed round without reading a request. The
`ChangeSignal`'s notifier now goes to `ServeState::wakes_with`, so a finished (or refused) round of notifications wakes
the task as well.

## A defect in the first version of the fix, found only by instrumenting it

The first version passed the VFS tests and still failed the end-to-end test. Temporary prints at each link showed the
wake worked: the replacement's `Truncate` woke the parked task. But the task, seeing its device readable, ran a
**request** turn, found no request waiting, and delivered nothing, because `dispatch_ready` returns before delivering
when nothing waits. Then it advanced the mount's `seen` generation anyway, marking the change handled when the kernel
had been told nothing.

The rule is now that `seen` advances only after a turn that **delivered**: a change turn, or a request turn that
dispatched a request. An idle turn leaves it, so the next wait still sees the change and runs a change turn. With that,
the trace reads `change_turn … Delivered { written: 2 }` — the truncate and the extend told to the kernel with no
request on the mount — and the map follows.

## Verification

- **Linux** (arm64 container, 6 CPUs, `--device /dev/fuse`, fuse3): both mount-route tests pass. With the map polled
  continuously, 51 replacements reached the map **52–193 µs** after the writer's reply, median 82 µs, p90 160 µs, the
  read fault after the invalidation included. Before the fix: never, on an idle mount. With the test's own 20 ms poll,
  most readings show as one poll, which is the poll and not the delivery.
- **Red-checked with the committed tests:** with the wake in `Volume::record` removed, the single-change test fails
  ("the replacement reached the map within 10s"), and the generated sequence misses 13 of its 16 changes (3 to 15).
  The first three got through only because the mount was still receiving kernel requests just after it was made,
  and any request lets an owed notification through: the bug's own mechanism.
- **macOS** (NFS v3): both tests pass five runs in five. They assert only what NFS v3 can give: the map follows once
  the file is `stat`ed.
- The whole CLI suite passed on Linux in six of seven runs; in the seventh one test lost its start to an anchor that
  exited during its own boot, unexplained and open in the ledger. The VFS suite passes; its host differential test
  needs a host directory
  that keeps extended attributes, which a Docker Desktop bind mount does not (`listxattr` answers errno 95); on a
  tmpfs it passes.

**What the fix does not do.** The mount is told after the writer is answered, not before, so a reader of the mount
in that 52–193 µs can still see the old bytes. AC-3.3 says it must not. That part is open in the ledger
(`docs/wip/GAPS.md`, the entry for this bug), with which writers could be made to wait and why a write through one
FUSE mount cannot wait on another.

## The tests' own defects, found while verifying

Two things in the verification were wrong before the fix could be judged by it, both in test code:

- **The mount-route fixture's writer.** It replaced `map.bin` through `slates write` under an attachment a
  `slates attach` process had made. An attachment belongs to its client and is removed once that client is reaped
  (`crates/server/src/reap.rs`; §4.7, D-16), and that process exits as soon as it prints. On macOS, where each
  replacement waits for the NFS client to revalidate, the generated sequence outlived the attachment: a write was
  refused `NotFound` at change 3 to 12 in five of six runs, a temporary print showing the record gone from its
  partition. Linux passed only because its map follows in microseconds. The fixture now holds an SDK client for its
  life, so the writer's attachment lives exactly as long as the map it changes.
- **The CLI suite's start wait** counted attempts from the anchor's spawn, so a loaded host's boot spent the budget
  on fast refusals: `2026-10-21-the-cli-tests-start-wait-spent-its-attempts-on-the-anchors-boot.md`.

## Exact edits

- `crates/vfs/src/volume.rs` — `ChangeWaiters` (generation, parked wakers), `Volume::change_generation`,
  `Volume::wait_for_change`, the wake in `Volume::record`.
- `crates/vfs/src/base.rs` — `process_hints` drains through `drain_hints` and wakes when the hint sequence moved.
- `crates/bridge-fuse/src/channel.rs` — `ChangeSignal::raw_signal` and a public `drain`, so an owner awaiting
  readiness through a runtime can wait on the signal itself.
- `crates/server/src/fuse.rs` — `FuseMount.change` and `.seen`; `change_signal`; `wait_for_work` racing the three
  sources; `turn` taking `TurnKind`; `seen` advanced only on a delivering turn.
- Tests: `crates/vfs/tests/change_waiters.rs` and `an_outsiders_change_beneath_a_base_directory_wakes_a_task_parked_on_the_volume`
  (`crates/vfs/tests/base.rs`), both red-checked; `crates/cli/tests/cli.rs`
  `a_shared_map_through_a_mount_writes_back_and_follows_another_paths_change` and
  `a_shared_map_through_a_mount_follows_every_change_in_a_generated_sequence`, over a real mount, through the
  `MappedThroughAMount` fixture and its held SDK writer.
