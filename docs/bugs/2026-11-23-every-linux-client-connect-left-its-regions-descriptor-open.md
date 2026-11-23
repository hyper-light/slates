# Every Linux client connect left its region's descriptor open (2026-11-23, §4.7 "Rendezvous")

Date: 2026-11-23
Area: `crates/ipc/src/rendezvous.rs` (Linux `decode_handoff`, the client's side of a claim), reaching
`crates/mem/src/shared.rs` (Linux `open_object`).
Regressed from: docs/bugs/2026-09-14-segment-handoff-descriptor-owned-twice.md.

## Description

Found by use: the nemesis of `crates/cli/tests/live_chaos.rs` on adacachy2. Two of ten runs panicked at
`live_chaos.rs:162`, where `run()` spawns the CLI: `Os { code: 24, kind: TooManyOpenFiles }`. The test process
had used up its 65,536 descriptors. It runs the nemesis's clients in process (`Client::connect`), and its
write streams connect again after every refused attach (`Stream::attach`) and every refused write.

## Measured

These counts are the test process's own descriptors, read from `/proc/<pid>/fd` every 2 s
(`nemesis-fds.sh`, `nemesis-fds2.sh`), over five runs of seed 14946995733817154046 on adacachy2:

- The count started at 10 and grew to 18,950, 10,062, 9,406 and 9,347. The fifth run died early at 577.
- Growth came in bursts of 2,000 to 3,000 in 2 s, and otherwise at about 50 a second.
- A census of their link targets, taken at 4,812 descriptors, digits folded: each of the twelve most common was
  `/memfd:slates-cr-<key>-<n> (deleted)`, 3,260 descriptors between them, all under one daemon start's key. These are
  the daemon's client regions, named `slates-cr-{key}-{number}` (`crates/server/src/daemon.rs`), a fresh number for
  every connect. A second run's census at 5,725 showed the same. None was mapped: `/proc/<pid>/maps` held 56 anonymous
  maps and no memfd.

So the process held a descriptor for every region it had ever been handed, including regions whose clients were long
gone.

## Root cause

The claim's handoff carries three descriptors: the region, the completion eventfd and the shard's kick.
`decode_handoff` received them as `OwnedFd`s. It then turned the region's descriptor into a bare number
(`into_raw_fd`) and opened the region from that number (`ClientRegion::open(&Handoff::Descriptor(raw), len)`).

On Linux, opening a `Handoff::Descriptor` duplicates the number and owns only the duplicate. That is the rule
2026-09-14's bug set, and it is right for that bug's case: a handed-off number is inherited state of the process. The
daemon attaches the anchor's segment more than once, and each attach must not own the number.

A descriptor received over the socket is different. It belongs to this one connection, and once it was a bare number
nothing owned it. The region's mapping closed its duplicate when the connection went, and the received descriptor stayed
open until the process exited.

Each leaked descriptor also keeps its memory object alive, so every region a client had connected through stayed
allocated as well.

## Fix

`crates/ipc/src/rendezvous.rs` `decode_handoff`: the received `OwnedFd` goes to `ClientRegion::from_descriptor`. The
region owns it and closes it with its mapping. macOS already takes this path (GAP-A9-9).

Test: `a_client_that_connects_and_leaves_holds_no_descriptor_of_its_region` in `crates/ipc/tests/rendezvous.rs`. It
connects eight times in one process, each claim served by a daemon end in the same process under a region name of the
test's own. It counts this process's descriptors naming those regions while each connection lives, and again after the
last has gone.

- Red on main: `counted while each lived: [3, 4, 5, 6, 7, 8, 9, 10]`, and 8 after (expected 0). One live connection
  is 3: the daemon end's own, the received one and its duplicate. Each earlier connection left one more.
- Green with the fix: all 24 rendezvous tests pass. The client crate's `client` (39), `reap` (1) and `reap_map` (4)
  tests pass, and `clippy -p slates-ipc --all-targets` is clean.

## Impact

Every Linux client process leaked one descriptor, and one region's memory, per connect: SDK processes, `slates`
commands, and the CLI and chaos test harnesses. That is unbounded growth (CLAUDE.md §2 item 8) wherever a process
connects more than once. A short-lived command lost nothing it would not have lost at exit. A long-lived process
reached `EMFILE`, and before that its next `memfd_create`, `eventfd` or `accept` failed (A-837). Reconnects made it
worse: the client reconnects after a daemon restart, and the harness after every refused attach.

macOS and Windows were not affected. A macOS region arrives as a Mach fileport, turned into an owned descriptor
(`mach_claim.rs`). A Windows region opens by name.

## Siblings (the sweep, every spelling)

Each construction of `Handoff::Descriptor`, each `into_raw_fd` or `as_raw_fd` handed to a callee that duplicates, and
each route that carries descriptors or handles across processes:

- Leaking, not fixed here (reported):
  - `crates/anchor/src/segment.rs` `handoff_env`. It calls `SharedObject::handoff` for the segment and the content
    object. That makes a duplicate without close-on-exec and returns only its number, and the caller formats the
    number into a child's environment and never closes it.
    - `crates/anchor/src/supervise.rs` `Supervisor::start` calls it at every daemon start. The anchor keeps two more
      descriptors per restart, and every later child of the anchor inherits all of them.
    - `crates/server/src/daemon.rs` calls it once per shard and once for the control loop. That costs 2 × (shards + 1)
      descriptors per daemon start, all inheritable by anything the daemon spawns, and the segment holds the issuer
      secret.
  - `crates/ipc/src/rendezvous.rs`, the daemon's side of a claim. `region.handoff()` makes the duplicate. It is adopted
    as an `OwnedFd` only after the kick's `dup` or `eventfd`, so if either fails the `?` returns first and the
    duplicate leaks. That happens exactly under exhaustion (`EMFILE`), when a lost descriptor costs the most. Adopting
    the number on the line that receives it closes the window.
- Not leaking:
  - Numbers inherited from the environment and never owned, the 2026-09-14 rule, one per process for its life:
    `crates/server/src/daemon.rs` (the segment's handoff), `crates/api/src/issuer.rs`,
    `crates/api/src/role/daemon.rs`, and `crates/anchor/src/segment.rs` (`attach_from_env`, `content_handoff_in`).
  - Borrowed numbers handed to a duplicating open while the caller keeps its owner:
    - `crates/api/src/issuer.rs`: the issuer route's claimed `object`, which closes at the end of its arm.
    - `crates/bridge-virtiofs/src/vhost_user.rs`: the memory table's received descriptors, dropped once the table is
      installed.
  - Adopted once, at start: `crates/server/src/fleet.rs` `inherited_descriptor`, `crates/server/src/daemon.rs`
    `held_listener`, `crates/ipc/src/delivery.rs` `take_named`, `crates/ipc/src/mach_claim.rs` (a fileport made a
    descriptor).
  - Process-lifetime by design: `crates/cli/src/signal.rs` (the relay's write end), `crates/machine/src/descriptors.rs`
    (the spare).
  - Handed to the caller by contract: `crates/ipc/src/endpoint.rs` `enable_async_completion_dup`. The Node SDK's
    `net.Socket` adopts one per channel and destroys it at a reconnect (`crates/sdk-node/async.mjs` `_ensureReader`).
    `crates/ipc/tests/completion_memory.rs` holds the contract.
  - `crates/anchor/src/held.rs` `hand_in_process`: a copy left for an in-process daemon to adopt (tests).
  - The macOS socket route (A-173). The client's `route::claim` receives owned descriptors and closes them on every
    refusal (`claimed_of`). `claim_map` and `claim_offer` in `crates/client/src/client.rs` move them into
    `PeerView::map`, `PeerWriteView::map` or a stream end, each keeping its descriptor. The server's
    `crates/server/src/shards_route.rs` keeps `OwnedFd`s and borrows a raw number only to register readiness.
  - Linux map deliveries (`recv_delivery`): owned descriptors, moved into the same views.
  - The Windows handle route (A-175, `crates/ipc/src/handle_route.rs`). The daemon duplicates into the client's
    table. The client adopts the value once (`adopt`) into an `OwnedHandle` that its view keeps. A duplicate whose
    reply the client never reads stays in the client's table. That path was not observed here.
