# FUSE invalidations waited for a kernel request, and a refused gather lost changes (AUD-02)

Date: 2026-09-19. Contracts: §4.6 "Cache posture" ("infinite cache lifetimes require proven
invalidation delivery and recovery for every mutation source; a notifier encoder alone cannot justify
them"); AUD-02 in `docs/bugs/2026-09-14_AUDIT.md`; GAP-A9-3/-4. Source-confirmed by the September 14
audit; the mounted reproduction ran on Linux for this fix.

## Symptom

The FUSE bridge advertises an unbounded kernel cache for the volume's own objects
(`CacheLifetime::Forever` → `entry_valid`/`attr_valid` of `u64::MAX`), so once the kernel has a name
and its attributes it answers `stat`, `lookup` and permission checks from its cache and sends the daemon
nothing. Two defects in the serve loop (`crates/bridge-fuse/src/channel.rs`) meant the cache could
stay wrong:

1. **No wake path.** The loop delivered owed invalidations only *before serving a kernel request*, and
   between requests it blocked in `read(/dev/fuse)`. A change made by another mutation source — the
   SDK, another attachment on the volume, an outsider's change beneath a base directory — while the
   kernel was answering from its cache produced no request, so nothing woke the loop and the
   invalidation was never delivered. A reader saw the old size forever, or until some unrelated
   uncached request happened by.
2. **A refused gather lost changes.** `serve_one` ignored a refusal from `Bridge::invalidations`
   (the cursor stayed, correctly) but then, after dispatching the request, unconditionally took the
   cursor past the request (`cursor = bridge.seen(cx)`), skipping every change the refused gather had
   not reported. The comment promised a retry; the control flow forgot the changes for good.

## Root cause

Coherence was a side effect of request service rather than a discipline of its own: the loop had one
wake source (the device), and the cursor's advance was tied to "a request was served" rather than to
"a round was delivered". The design's rule for an infinite lifetime is delivery **for every mutation
source**; the loop could only hear one.

## Fix

- **A pure delivery discipline** (`crates/bridge-fuse/src/coherence.rs`, cfg-free, unit-tested on
  every host): `Coherence::deliver` gathers what is owed since the cursor, hands each invalidation to
  the transport's sink, and advances the cursor **only past what was gathered and written**; a seam
  refusal keeps the cursor and counts (`gather_refusals`), a sink refusal returns with the cursor
  unmoved. `Coherence::served_own_request` moves the cursor past the transport's own request only when
  the round before it was delivered whole — after a refused gather the cursor stays, so the missed
  changes are delivered at the next wake (the kernel's own change with them, a harmless redundancy).
- **A change signal and a poll-driven step** (`channel.rs`, Linux): `ChangeSignal` (an `eventfd`)
  with `ChangeNotifier`s for other mutation sources; `wait` polls the device and the signal together;
  `serve_step` runs one round on a `Change` wake with no request, or reads and serves a request (the
  round first, then the dispatch, then the own-request advance); `serve_blocking` is the loop over
  them, and an owner that interleaves other work with the mount (the daemon's shard serving verbs)
  drives `wait` and `serve_step` itself, applying its changes between them and signalling. `Step`
  reports what each step did (the opcode served, the round delivered) — the counters the tests read.

## Failing test first, and regressions

- `crates/bridge-fuse/tests/coherence.rs` (every host, a real volume bridge with a recording sink):
  a change through another attachment is delivered as the file's inode invalidation and the
  transport's own change is not; a refused gather keeps the cursor, the request served under it does
  not move the cursor past the missed change, and the next round delivers it; a sink refusal keeps
  the cursor so the round is delivered again.
- `crates/bridge-fuse/tests/coherence_mount.rs` (Linux, a real `fusermount3` mount, skipping loudly
  without `fusermount3` or `/dev/fuse`): the audit's scenario. A file is written through the mount and
  `stat`ed (the kernel's cache warmed); a second `stat` sends the daemon **no** `GETATTR` (the loop's
  counter, unchanged — the cache is warm and unbounded); another attachment truncates the file with no
  kernel request in flight, signalling the loop; the next `stat` through the kernel reports the new
  size and the counter moved by exactly one (the invalidation forced it). Then the injected collection
  failure: a further truncate is made and the seam refuses the gather of the round a kernel request
  runs (a lookup of an absent name); the kernel still reports the old size (owed, not lost); the next
  wake delivers it and `stat` reports the newest size, the refusal counted once. Before the fix the
  first half could not run at all (the loop had no wake but the device) and the second half lost the
  change (the cursor taken past the request).

## Validation (2026-09-19)

- macOS (this box): `cargo test -p slates-bridge-fuse --test coherence`: **3 passed**; the whole
  crate green; `cargo clippy -p slates-bridge-fuse --all-targets -- -D warnings`, `cargo fmt --check`,
  `cargo xtask check`: clean.
- Linux (a `rust:1.98` container on this box: `docker run --device /dev/fuse --cap-add SYS_ADMIN`,
  `fuse3` installed — kernel 6.12.76-linuxkit, `fusermount3` 3.17.2; the CI Linux lane runs the same
  test under `cargo test --workspace`, skipping loudly if its runner lacks `fusermount3`):
  `cargo test -p slates-bridge-fuse --test coherence_mount`: **1 passed, 0.13 s, three runs in a
  row**; the crate's other suites green on Linux; `cargo clippy` (Linux, all targets, `-D warnings`)
  clean.

## What the real kernel found on the way

The mounted regression was the FUSE serve loop's first run against a real kernel, and it failed
twice before it could test coherence at all — each a bug of its own, fixed under this change:

1. Every attribute reply lacked the file-type bits, so the kernel marked the root inode bad and the
   first `open` through the mount answered `EIO`
   (`2026-09-19-fuse-attribute-replies-carry-no-file-type-bits.md`).
2. `FUSE_INIT` asked for writeback cache, under which the kernel owns a regular file's size and
   ignores the invalidation of a change made behind it: the truncate was delivered and accepted, and
   `stat` still answered the old size — deterministically, three runs
   (`2026-09-19-writeback-cache-made-the-kernel-the-size-authority.md`).

## Scope and siblings

- The daemon does not yet serve `/dev/fuse` (Linux mounts reach the daemon through the NFS export;
  `docs/wip/conformance.md` records the Linux lane LIMITED for that reason). The step API is the shape
  that integration needs — a poll source plus one step per wake, the shard applying verbs between
  steps and signalling — and the discipline is proven on the real kernel here.
- The base-plane watcher's hints (an outsider's change beneath a base directory) are gathered by the
  same round; the watcher's own descriptor as a third wake source is that integration's, so that an
  outsider's change wakes the loop too. Until then a live-source entry carries the base filesystem's
  bounded lifetime (`cache_lifetime`), never the unbounded one, so the kernel revalidates it.
- The NFS export has no unbounded cache (`actimeo=1`, `crates/cli/src/mount.rs`), so it needs no
  delivery path; the audit's finding was FUSE-specific.
