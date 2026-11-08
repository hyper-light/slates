# A refused second anchor unlinked the live instance's segment

**Amendment:** A-254, under the rule of A-176 and A-178.

**Found by:** a loaded run of a Shards test on macOS (2026-11-07) failed a shard's start with `shm_open` `ENOENT`. Its
cause there was a test fixture's shared segment name (`common::fixture_object` fixed that). Reading the same paths in the
product found this.

## Description

On macOS, starting a second `slates anchor` for an instance that is already running, or a second daemon run alone, was
refused, as it should be. But the refusal came after the second start had created its shared-memory segment. That
creation and its drop each removed the name of the live instance's segment. The live anchor kept running, and its next
daemon could not attach the segment:

    slates: attach: shared memory: shm_open refused (code Some(2))
    slates anchor: the daemon is in a crash loop (last exit Some(4)); restarting it in 8000 ms

## Root cause

A macOS shared-memory name belongs to the user. `slates-mem`'s `create_object` first unlinks any object of that name (for
a stale object left by a crash), and the creator's drop unlinks the name again. Both start paths created the segment
before they asked who owned the instance:

- `crates/cli/src/anchor.rs` created the segment, then held the rendezvous many lines later.
- `Daemon::start` with `SegmentSource::Create` created the segment, then opened its listener after starting its shards.

The owner's token (A-176 on Windows, A-178 on macOS, the listening socket on Linux) was correct, and refused the second
start, but by then the damage was done.

Linux has no segment name (a memfd), and on Windows a live section's name makes the second creation fail, so neither lost
a segment. Both still took the same wrong order.

## Impact

One mistaken `slates anchor` for a running instance, on macOS, left that instance unable to restart its daemon. The next
daemon death of any kind became an endless crash loop, every hard NFS mount on it hung, and the volumes in the segment
could not be served again until the anchor itself was restarted.

## Edits

- `crates/cli/src/anchor.rs`: `HeldRendezvous::hold` comes before the segment is created.
- `crates/server/src/daemon.rs`: `Listener::open`, and the seal before it, come before the segment is created or attached.
- `crates/cli/tests/cli.rs`: `a_refused_second_anchor_leaves_the_live_instance_its_segment_and_its_bytes`.
- `crates/server/tests/daemon.rs`: `a_refused_second_daemon_leaves_the_live_ones_segment_in_place`.

## Evidence

On this Mac, both tests on the old order:

- The CLI test: the second anchor was refused, and each of the live anchor's next daemons failed `shm_open` `ENOENT` into
  a crash loop.
- The daemon test: the live daemon's segment could not be attached (`shm_open` code 2).

With the fix, both pass on this Mac: the second anchor is refused, the daemon killed after it is succeeded by one that
adopts the segment by name, and the file reads back whole.
