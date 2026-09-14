# The epoll driver re-adds an already-registered socket, so the second await on any socket fails with EEXIST

Date: 2026-09-14
Area: `crates/rt/src/epoll.rs` (`register_readable`, `register_writable`)
Severity: every socket on the epoll driver — the Linux fallback wherever io_uring is refused, which is
every container under the runtimes' default seccomp profile (Docker, containerd/Kubernetes
`RuntimeDefault`) — can be awaited **once**; the second await ends its loop. The fleet's serve sockets
died after their first datagram on every pod of the KIND lane, so no mesh could ever form on
Kubernetes; the NFS mount server's per-connection reads and writes would have died the same way.

## Symptom

The KIND lane's first fleet install (2026-09-14 13:33 CDT, three pods on kind v0.33.0 under
`seccompProfile: RuntimeDefault`): every pod `fleet_peers_probed: 0` forever, `status` counting
`fleet.serve: 2` on each — both planes' receive loops ended — with the cluster's DNS serving every
per-pod name (checked from a throwaway pod, and with the daemon's exact query bytes answered by
CoreDNS with the `A` record). With the loop's error logged once:

```
slates-server: fleet: a serve socket's receive loop ended:
  Io(DriverRefused { call: "epoll_ctl(ADD readable)", code: Some(17) })
```

`17` is `EEXIST`.

## Root cause

`EpollDriver::register_readable` registers the socket with `epoll_ctl(EPOLL_CTL_ADD, EPOLLIN |
EPOLLONESHOT)` on every call. A one-shot registration is *disabled* after it fires, not removed: the
descriptor stays in the epoll interest list, and the next `EPOLL_CTL_ADD` of the same descriptor is
refused `EEXIST` (epoll_ctl(2): "the supplied file descriptor is already registered"). The readiness
future (`crate::readiness::readable`) registers on every await, so the second await on any socket —
the demultiplexer's second datagram, a TCP stream's second read — fails, and the loop that awaited
it ends. kqueue's `EV_ADD | EV_ONESHOT` re-adds an existing event harmlessly, so macOS never showed
it; the Linux CI runners have io_uring, whose `poll_add` is per-await, so the epoll path was never
exercised with a second await until the lane ran the daemon under a container's seccomp profile.

## Fix

`epoll_ctl(ADD)`, and on `EEXIST` re-arm the existing registration with `epoll_ctl(MOD)` carrying the
same one-shot interest and the new waker word — the documented epoll discipline for one-shot
interest (epoll(7) "EPOLLONESHOT … the user must call epoll_ctl with EPOLL_CTL_MOD to rearm"). One
helper for both interests; the borrow of the caller's descriptor is unchanged.

Test first, in the `rust:1.98` container (io_uring refused, epoll driver):
`a_second_receive_on_the_same_socket_registers_readiness_again` (`crates/rt/tests/udp.rs`) — a
receiver awaits two datagrams on one socket, each sent after it blocked — fails before the fix with
`DriverRefused { call: "epoll_ctl(ADD readable)", code: Some(17) }` and passes after; on kqueue and
io_uring it passes either way (no registration is retained). The lane's formation is the by-use gate.

## Siblings

- `register_writable` had the same shape (a TCP stream whose send buffer filled twice); fixed by the
  same helper.
- The AFD reactor on Windows (`crates/rt/src/afd.rs`) issues a fresh `IOCTL_AFD_POLL` per await —
  not affected. kqueue and io_uring — not affected.
- The rt `udp` test ran on Linux CI under io_uring only; the epoll driver had no CI coverage of a
  second await. The KIND lane now runs the daemon under `RuntimeDefault` seccomp, so the epoll path
  is exercised by every lane run.
