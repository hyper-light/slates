# NFS replies waited 40 ms for the client's delayed ACK

Date: 2026-10-05. Scope: the runtime's TCP streams (`crates/rt/src/tcp.rs`) behind the NFS loopback server
(§4.6), NFS over TLS and HTTP.

## Symptom

The hot-directory storm through Linux's own NFS client on loopback (`docs/wip/bench/hotdir/native.sh`) had tails
clustered at 40–50 ms: with 1 worker, create+write+close p99 43 ms and open+read+close p99 42 ms; with 16 workers,
create p99 690 ms. The daemon served each operation at p50 2 µs and p99 0.59–0.72 ms (`nfs.local_p*_ns`), yet a
mountstats OPEN round trip cost 1.46–6.3 ms.

## Root cause

No runtime stream set `TCP_NODELAY` (`grep -rni nodelay crates/rt/src crates/server/src crates/bridge-nfs/src`
returned nothing). With Nagle's algorithm (RFC 896) on, a small write made while an earlier segment is
unacknowledged is held until the ACK arrives. The server writes one batch of replies per write, so a reply that
leaves while the previous batch is unacknowledged waits. A client with nothing more to send delays that ACK, by
40 ms on Linux (`TCP_DELACK_MIN` = HZ/25). Confirmed by a test before the fix (below), not inferred from the
number alone.

## Impact

Every Linux NFS mount (local loopback, k8s nodes, containers on a Linux host) paid up to a delayed-ACK timer on
any reply that followed another before its ACK. Pipelined clients (several RPC slots in flight) hit it on most
rounds. The Docker Desktop runs did not show it, and macOS acknowledges at once on loopback here, so neither the
macOS suite nor the Docker Desktop benchmark could see it.

## Fix

- Failing test first: `a_reply_in_two_writes_does_not_wait_for_the_peers_delayed_acknowledgement`
  (`crates/rt/tests/tcp.rs`). A server answers each request in two writes, and the median of 32 rounds must stay
  under a quarter of the smallest delayed-ACK timer (10 ms). On Linux (rust:1.98.0 container) it failed with a
  median of 42 ms and every round after the first at 40.7–51 ms. On macOS it passed before the fix.
- `crates/rt/src/tcp.rs`: `prepare_stream` sets `TCP_NODELAY` with non-blocking and close-on-exec on every
  accepted and adopted stream, and `connect` sets it on every connected stream. A refusal to set it is a typed
  `RtError`. The test now passes on Linux and macOS.
- Re-measured: with 1 worker, create p99 43 → 0.86 ms and open+read p99 42 → 0.21 ms; with 16 workers, create
  p99 690 → 13.3 ms (docs/wip/BENCHMARKS.md, "The same storm through Linux's own NFS client").

## Sibling sweep

Every production TCP socket in the tree is a `slates_rt::tcp` stream. `std::net::TcpStream` appears only in
tests, which the lint wall reserves. The UDP fleet transport has no Nagle. No other site is owed.
