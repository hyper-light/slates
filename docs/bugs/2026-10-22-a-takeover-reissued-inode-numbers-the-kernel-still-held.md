# A takeover reissued inode numbers the kernel still held

**Status:** fixed 2026-10-22 (A-150, approved by Ada in plan mode). The root cause was shown by a test that failed on the box (Linux 7.2.9).

**Severity:** cross-file reads, and through a write cross-file corruption, after a daemon takeover (A-61). The successor
issues again the inode numbers the dead daemon handed out after its last publication. A FUSE node id is the inode
number, and the bridge reports generation 0 for every object (`crates/bridge-core/src/volume_bridge.rs`). So the
kernel's handle on a file opened before the takeover names whatever the successor numbered next. It breaks AC-1.6
("inode numbers are never reused within a volume") and D-4.

The same numbers reach every client that keeps them across a restart:
- NFS file handles, which carry `(volume, inode, generation 0)` to clients held across a restart (A-113, A-127);
- virtio-fs guests.

**Found:** 2026-10-22, while reproducing the open-count gap after a takeover (GAPS, A-147's siblings). A test of that
gap failed. The failure turned out to come from this reuse, not from the open count.

## Reproduction

`crates/cli/tests/cli.rs` `a_node_id_handed_out_before_a_takeover_never_names_another_file_after_it`, on the box:
1. a volume over a base directory holding `first.txt` and `second.txt`, mounted through FUSE;
2. `first.txt` opened and read; its inode number is 3659174697238530;
3. `SIGKILL` of the daemon; the anchor's successor takes the mount over;
4. `second.txt` looked up first;
5. the first file's cached pages dropped, then a read through its old descriptor.

It failed: "the successor issued the second file inode number 3659174697238530, which the dead daemon had handed the
kernel for the first file". The first file's descriptor read `the second file`.

## Root cause

`Volume::next_no` (`crates/vfs/src/volume.rs`) issues numbers from `next_counter`. That counter is durable only in a
publication (the recovery image and the delta both record it), and a successor restores it from the last one.

Numbers are issued without a publication:
- base entries are numbered at `LOOKUP` (`base.rs` `materialize`);
- `READDIRPLUS` numbers every new entry it lists;
- an `OPEN` adds nothing durable.

None of those requests runs the owner's barrier (`slates_bridge_fuse::bridge::needs_barrier`), so the kernel learns
numbers no publication recorded. Every number issued since the last publication is issued again by the successor,
to whatever it creates next.

## The fix (A-150)

Handing out an inode number is an effect a client keeps across a restart, so it follows D-18: a reply that carries a
number the committed publication does not reserve waits for the publication that does.
- **The ceiling.** Each publication records the counter plus the store's reserve (`Volume::recorded_counter`), never
  below the last ceiling. The live ceiling (`counter_ceiling`) rises to that value only when the publication commits:
  `mark_published`, in the same bracket as A-64's capture.
- **The barrier.** A number issued at or past the live ceiling is unreserved (`Volume::unreserved`). The FUSE reply,
  the NFS reply and a guest's chain then run the owner's barrier first (`numbers_unreserved`;
  `Bridge::numbers_unreserved` for the device).
- **The top-up.** The per-work hook (`relieve_deferred`) publishes before a unit of work once a volume's reserve falls
  under half (`Store::counters_low`, set by `next_no` and at a rebuild), so those barriers are rare.
- **The successor** restores the counter at the recorded ceiling. It skips at most one reserve of numbers and reuses
  none.
- **The reserve** is derived (`DaemonConfig::inode_reserve`): the largest listing reply over its smallest entries,
  times the top-up share. That is 262,144 on the box. Windows records none, since no transport there hands a client a
  number it keeps.

## What the first build missed

With the struct builders changed (`to_image`, the delta's `roots`), the reuse test still failed on the box. Temporary
logging showed why:
- the create's publication committed with `ceiling=262146`;
- the lookup issued counter 2 under it;
- yet the successor rebuilt from `image_next=2`.

The daemon publishes through the streamed encoders (`encode_publication`: `encode_image_into`, `encode_delta`), which
still wrote the live counter. The vfs model oracle compares streamed and built bytes after every step, but its store
had a reserve of zero, where the two counters coincide. Now:
- both encoders record the ceiling;
- the oracle runs with a nonzero reserve, and goes red when the delta encoder writes the live counter ("the streamed
  publication after Create");
- the reservation model publishes through the streamed path, and goes red when both encoders do ("the recovered volume
  issued 2, at or below 5").

## Exact edits

- `crates/vfs/src/volume.rs`:
  - `Store::inode_reserve`, `Store::counters_low` and `RESERVE_TOP_UP_SHARE`;
  - `Volume::counter_ceiling`, initialized to the counter at create and at clone, and to the recorded counter at a
    rebuild;
  - `recorded_counter` and `unreserved`;
  - `next_no(store)` sets `counters_low`.
- `crates/vfs/src/delta.rs`: `roots` and `encode_delta` record the ceiling; `mark_published` raises it.
- `crates/vfs/src/recover.rs`: `to_image` and `encode_image_into` record the ceiling; a rebuild sets `counters_low`.
- `crates/vfs/src/base.rs`, `crates/vfs/src/xattr.rs`: `next_no(store)`.
- `crates/bridge-core`: `Bridge::numbers_unreserved` (`VolumeBridge`, `ScopedBridge`).
- `crates/bridge-virtiofs/src/device.rs`: `awaits_barrier` also holds a chain while numbers are unreserved.
- `crates/server/src/config.rs` (`inode_reserve`, derived and logged), `daemon.rs` (set into the shard's store),
  `verbs.rs` (`relieve_deferred` tops up, `publish_shard` clears `counters_low`, `numbers_unreserved`,
  `COUNTERS_TOPPED_UP`), `fuse.rs` and `nfs.rs` (the reply's barrier).
- The tests:
  - `crates/vfs/tests/counter_reserve.rs` (three);
  - `crates/vfs/tests/model.rs` (`MODEL_INODE_RESERVE`);
  - `crates/cli/tests/cli.rs` (the reuse test, and the base-file test that A-150 turned into a refusal);
  - `crates/bridge-virtiofs/tests/admission.rs` and `device.rs` (their fixtures commit the publication a verb's
    barrier stands for);
  - the test bridges in `crates/bridge-fuse/tests`.
- Docs: `SLATES_DESIGN.md` A-150 and AC-1.6, GAPS.

## Verification

| | before | after |
|---|---|---|
| the CLI reuse test (the box) | the same number, 3659174697238530, for both files | numbers differ; the old descriptor's read is refused `ENOENT` |
| the hand repro with temporary logging (the box) | rebuild from `image_next=2` | rebuild from `image_next=262146`, `second.txt` at counter 262146 |
| `a_crash_never_lets_the_rebuilt_volume_issue_a_number_handed_out_before_it` | "issued 2, at or below 5" (old behaviour emulated) | green |
| the reservation model, both streamed encoders on the live counter | "issued 2, at or below 5" | green |
| the vfs model oracle with a reserve, the delta encoder on the live counter | "the streamed publication after Create" | green |

- **macOS:**
  - the vfs and bridge suites (650);
  - the server's `recovery` (39), `nfs_mount` (39), `nfs_hostile` (17), `virtiofs` (8) and `attach_forms` (9);
  - workspace clippy `-D warnings`, the doc gate, `cargo xtask check` and fmt;
  - the arm64 Windows cross-lint of `wire`, `mem`, `rt` and `ipc`.
- **The box (Linux 7.2.9):**
  - workspace clippy `-D warnings`;
  - the vfs and bridge suites (681);
  - the server's `fuse_mount` (3), `fuse_idle` (1), `nfs_mount` (39), `recovery` (41) and `virtiofs` (18);
  - `coherence_mount` (7) and the guest tests (3).

**The CLI suite on the box:** 53 of 53 in 3 of 8 full runs. Each of the other five failed one test under extreme
load from other sessions' builds (load average 80 to 139 on 32 cores). None reproduced alone: the failing test passed 5
of 5 alone at load 93, and 2 of 2 on macOS.
- **`a_fuse_mount_survives_its_daemons_kill_with_its_open_files_usable`, 3 of 8:** `fusermount3` refused the
  unmount, "Device or resource busy". Unexplained. The box's KDE session, which probes new mounts, is a suspect.
- **`the_anchor_supervises_a_daemon_the_verbs_answer_and_the_daemon_leaves_with_the_anchor`, 2 of 8,** with no daemon
  restart (its instance started once): a staged write, and once a `detach`, refused `NotFound`. A staging buffer
  expires one lease (`failover_slo_ns`, 10 s) after its last use and then answers `NotFound`, and the CLI does not
  stage again. The `detach` is suspected to be the attachment's own lease, not verified.

A-150 changes neither lease. A run of the same suite against the pre-A-150 tree under the same load is owed before
that is called proven (GAPS).

## After it landed: two misses CI found (2026-10-23)

CI on `0179be5f` failed three jobs, on two misses this record's verification did not cover:
- **Windows, both lanes:** `crates/server/src/verbs.rs` `numbers_unreserved` was dead code there. Its only callers,
  the FUSE and NFS replies, are Unix-only, so `-D warnings` refused it. It is `#[cfg(unix)]` now.
- **Ubuntu:** `crates/bridge-virtiofs/tests/device.rs`
  `a_message_based_setupmapping_is_refused_where_the_window_cannot_be_confined` created its file with no committed
  publication. So the file's number was unreserved, and the device held the chain for a barrier instead of refusing
  the map outright ("the map was refused outright, not parked"). Its fixture now commits the publication, as the
  others A-150 touched do.
  - It runs only where `/dev/udmabuf` is absent: CI's runners and Docker's VM, not the box (which has it) and not the
    Mac (Linux only). So no run here executed it.
  - Verified in a native arm64 Linux container without `/dev/udmabuf`: the device suite, 42 of 42.
