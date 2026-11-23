# A hand-off left an inheritable duplicate at every call, and a daemon's hooks held the anchor's segment (2026-11-23, §2.6, §4.7, §4.13)

Date: 2026-11-23
Area: Linux only.
- `crates/mem/src/shared.rs`: `handoff`.
- `crates/anchor/src/supervise.rs`: the spawn and the order of a start's record.
- `crates/api/src/role/daemon.rs`: the daemon's start.
- `crates/ipc/src/rendezvous.rs`: the claim's hand-off, `completion_dup`.
Regressed from: docs/bugs/2026-09-14-segment-handoff-descriptor-owned-twice.md.
Found by: the sweep of docs/bugs/2026-11-23-every-linux-client-connect-left-its-regions-descriptor-open.md, then by
the tests below.

## Description

On Linux, `SharedObject::handoff` duplicated the object's descriptor, cleared close-on-exec on the duplicate, and
returned only its number. Its doc said "the caller closes nothing (the duplicate lives in the child)". But the
duplicate lived in the caller too, and nothing there owned it:

- **The anchor.** Its supervisor calls `AnchorSegment::handoff_env` at every daemon start, for the segment and for the
  content object. So every start left two more inheritable descriptors in the anchor, and each later child of the
  anchor inherited every one of them.
- **The daemon.** It calls `handoff_env` for every shard and for its control loop, so it held 2 × (shards + 1)
  inheritable descriptors of the anchor's objects.
- **Everything the daemon inherited stayed inheritable**: the segment's and the content object's numbers, the listeners
  the anchor kept for it, and its hold table.
- **Two plain duplicates.** The daemon made a plain `dup`, which has no close-on-exec, of every connected client's
  completion eventfd and of the shard's kick.
- **The claim.** Its hand-off made a duplicate of the region's descriptor and adopted it only after the kick was made.
  So a kick refused `EMFILE` returned first, and the duplicate stayed open for the daemon's life.

Linux's spawn hands a child every descriptor not marked close-on-exec. `posix_spawn` has no default to close, and
`confined_spawn` names only the three standard streams, so a hook, which is a user's code, held all of the above.

macOS hands the objects by name, and its confined spawn closes every descriptor it does not name
(`POSIX_SPAWN_CLOEXEC_DEFAULT`). Windows spawns with one explicit handle list (A-198). Neither had this path.

## Measured (red, adacachy2, 2026-11-23)

- **The hook.** `a_hook_holds_no_descriptor_of_its_daemon` (`crates/cli/tests/svn_interop.rs`) ran a real anchor and
  daemon, and a start-commit hook that became `ls -l /proc/self/fd/`. Past its three streams and the listing's own
  directory, the hook held 17 descriptors:
  - 4 of `/memfd:slates-seg-…`, the anchor's segment, which holds the issuer secret;
  - 4 of `/memfd:slates-con-…`, the content object, which holds every volume's bytes;
  - a listening socket;
  - a client's completion eventfd;
  - seven `perf_event` descriptors that the runner had leaked into the daemon's own environment.
- **The anchor.** `an_anchor_started_again_and_again_hands_each_daemon_its_objects_once` (`crates/anchor/tests/anchor.rs`)
  killed a recovered child 6 times. After the 7 starts, the test process held 8 descriptors of each object, where it
  held 1 before the first. Its children held 2, 4, 6, 8, 10 and 12 at their starts, against 2 each.
- **The claim.** `a_claim_refused_for_want_of_a_descriptor_leaves_none_of_its_region` (`crates/ipc/tests/rendezvous.rs`)
  capped the descriptor limit at one past the lowest free number as the claim was prepared. The claim was refused,
  and one descriptor of its region remained (`refused=1 remaining=1`).

## Root cause

2026-09-14's fix made an open of a handed-off number duplicate it, because an inherited number is the process's own
state. It also made the hand-off itself a duplicate, so that a child would inherit one. The duplicate was cleared of
close-on-exec for that one child, but it lived on in the parent, where no value owned it and every later child of the
process inherited it. The daemon's own descriptors carried the same fault: what it inherited it never marked, and its
plain `dup`s were made without the flag.

## Fix

- **The memory crate** (`crates/mem/src/shared.rs`). On Linux `handoff` names the object's own descriptor, which is
  close-on-exec and open while the object lives. Nothing is duplicated until an open, which owns its duplicate.
- **The anchor** (`crates/anchor`).
  - `Supervisor::spawn_child` clears close-on-exec in the child alone, between its fork and its exec, on the segment's
    and the content object's numbers (`hand_on_in_child`, `CommandExt::pre_exec`; `AnchorSegment::handed_descriptors`).
    The anchor itself keeps one descriptor of each object.
  - The start is recorded before the spawn and the pid after it (`Supervision::record_pid`). Recorded after the spawn,
    the record wiped a quick child's first beat and serving mark, and showed it the last start's restart count. Main's
    own anchor tests failed this way in 1 run of 6. The forked spawn widened the window, since another thread's fork
    holds the spawn's exec pipe, and they failed in 4 of 6.
- **The daemon** (`crates/api/src/role/daemon.rs`). Its first step at its start marks every descriptor past its
  standard streams close-on-exec (`slates_ipc::confined_spawn::close_inherited_on_exec`), and a daemon that cannot do
  so does not start.
- **IPC** (`crates/ipc`).
  - The claim sends the region's own descriptor, borrowed, so nothing is duplicated and nothing is left behind by a
    refusal.
  - The kick, the completion eventfd's copy for the daemon's end, and the client's completion copies are all made with
    `fcntl_dupfd_cloexec`: `rendezvous.rs`, `completion.rs` and `endpoint.rs`.

## Tests (green, adacachy2)

- The three reds pass: the hook lists its three streams and its directory alone; the anchor holds `[1, 1]` after 7
  starts, and each child holds 2; the claim leaves `remaining=0`.
- With the record order fixed, the anchor's file passes 6 of 6 runs (36 tests). Main's base code passes 5 of 6 in the
  same loop.
- These pass:
  - the rendezvous tests (26);
  - the ipc unit tests (57);
  - the memory crate's hand-off tests (6);
  - the client's `client` (39, 3 of 3 runs), `reap` and `reap_map`;
  - the DAV hook test beside the new one;
  - the anchor's unit tests (43).
- `clippy -D warnings` on mem, ipc, anchor and api (all targets), and on the CLI's tests, is clean, and so is
  `cargo xtask check` (the unsafe budget is +1 for the anchor, ±0 for ipc).

## Impact

Every child of a Linux daemon could read the anchor's segment, and with it the issuer secret, the authority under which
the issuer's verbs prove a human's grant (§4.13, R10). That includes a repository's hooks, which are code the operator
configures and slates does not own.

The same children could read the content object, which holds every volume's bytes on the host. They could also accept
connections on the listeners the anchor held for the daemon, and signal every connected client's completion eventfd.

The anchor kept two descriptors per daemon start, without bound over a crash loop, and every child it started
inherited every one made before. A claim refused at exhaustion leaked a descriptor exactly where one counts most.

## Siblings

- **Open (GAPS), the Windows edge.** A map reply whose operation was abandoned is dropped on arrival
  (`Client::buffer`, `unawaited_dropped`). The section handle the daemon duplicated into the client's table then stays
  there, and the daemon keeps the copy for the client until its release or the client's reap. Closing the handle is a
  line in the drop branch (`handle_route::adopt_section`, dropped). Giving the copy back needs a release request from
  that path, and neither can be exercised off Windows, so the edge is recorded, not changed.
- **Not changed.**
  - The anchor's held listeners and its hold table stay inheritable in the anchor, whose only child is the daemon, and
    the daemon marks them close-on-exec at its start.
  - Every other creator of a descriptor in shipped code passes the flag as it creates it: sockets
    (`SOCK_CLOEXEC`), accepted connections (`accept_with`), eventfds, memfds and pipes. The plain `dup`s left are in
    tests.
