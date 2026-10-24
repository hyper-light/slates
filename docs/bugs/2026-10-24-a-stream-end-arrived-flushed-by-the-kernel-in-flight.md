# A Shards stream end arrived flushed by the kernel while in flight (macOS)

**Status:** fixed in the change that adds the macOS socket route (A-161, Shards part 1 step 3), before it shipped.

## Description

On macOS the daemon hands a client the end of a Shards stream on its socket route: it sends the end by `SCM_RIGHTS`
with its confirmation, then closes its own copy and the route connection. Under load the client sometimes received an
end whose reads answered end of file at once, while its writes still reached the daemon:

- `hostile_claims_get_no_descriptor_and_the_honest_claim_succeeds_once` (`crates/server/tests/shards.rs`) failed 4 of
  20 runs, then 5 of 16, at load 35-47 on the Mac (2026-10-24). The client's `INIT` reached the daemon, which read it
  and failed to answer (`EPIPE`, the test having given up); the client's first read had returned 0 bytes 12 µs after
  its write.
- The descriptor was the same socket at the claim and at the end of file (`fstat`'s device and inode unchanged), with
  no pending error (`SO_ERROR` none).
- In every failing run the received end's receive buffer read `SO_RCVBUF` 0 at the claim; in 25 passing runs it read
  8,192, the kernel's default.

## Root cause

XNU collects descriptors in flight in Unix-socket messages (`unp_gc` in `bsd/kern/uipc_usrreq.c`). A socket whose every
reference sits in a message, with nothing outside one, is a candidate; one the collector does not find reachable is
flushed (`sorflush`), which empties its receive buffer and zeroes its high-water mark, so reads answer end of file and
writes still go out. The daemon closed its own copy of the end right after sending it, so until the client's
`recvmsg` installed the end in its descriptor table, the message held the only reference. A collection running in that
window, which any Unix-socket close in the system can trigger while descriptors are in flight, flushed the end.

The evidence is the measurement above. Third-party reports name the same mechanism and the same zeroed receive buffer:
k3sm-io/darwin-net#143 ("refuse a handed-over socket the kernel flushed in flight", about 1 in 3,000 handovers under
load) and yas-run/yas#74 (the sweep treats a socket whose `fg_count == fg_msgcount` and that is unmarked as garbage).
No Apple source or advisory was found.

## Impact

A client's Shards stream end could be dead on arrival, about one claim in five under load, and up to 40 of 48 claims
with another thread opening and closing Unix sockets. The guest's share would fail at its first request with nothing
to say why. Nothing shipped: the route was being built. The Linux route, the control socket, also closes the daemon's
copy after the send (`shards::deliver_handoff`). Measured since (2026-10-24): not affected under the same kind of
stress. `every_end_delivered_arrives_live_while_the_kernel_collects_descriptors_in_flight` delivered 48 of 48 ends
live in each of 10 runs on Linux 7.2.9 while another thread dropped 8,192–12,976 unreachable cycles of descriptors in
flight, each release scheduling Linux's collector.

## The fix

- **The daemon keeps its copy until the client has its own** (`crates/server/src/shards_route.rs`): after the
  confirmation and the end are sent, the exchange waits, within its deadline, for the client to close the route
  connection, which the client does once its `recvmsg` has returned, and only then closes its copy. The end always has
  a reference outside the message, so the collector never takes it for garbage.
- **The client refuses an end that arrives flushed** (`crates/ipc/src/route.rs`, `claim`): a received end whose
  receive buffer reads zero is refused `RouteRefusal::Flushed`, so a regression is a typed refusal, never a share that
  dies at its first request.

## Test, failing first

`every_end_handed_over_arrives_live_while_sockets_churn` (`crates/server/tests/shards.rs`) claims 48 ends while another
thread opens and closes Unix socket pairs, and requires every end's receive buffer to be the kernel's default and its
`INIT` answered. Before the fix: 32, 17, 40, 26 and 28 of 48 ends arrived flushed in five runs.
