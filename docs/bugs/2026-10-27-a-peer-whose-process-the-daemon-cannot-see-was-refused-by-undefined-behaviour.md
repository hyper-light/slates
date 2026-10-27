# A peer whose process the daemon cannot see was refused, by undefined behaviour

- **Date:** found and fixed 2026-10-27, by the first run of the KIND lane's direct leg (`cargo xtask kind direct`).
- **Status:** fixed.
- **Subsystem:** the Linux rendezvous (§4.7 "Rendezvous", `crates/ipc/src/rendezvous.rs`), and the vhost-user seam's
  check of its front end (`crates/bridge-virtiofs/src/vhost_user.rs`).
- **Severity:** undefined behaviour in production code on every Linux accept, met by an ordinary placement, with a
  client refused as a result. A daemon refused every client in a pid namespace it cannot see, with an untyped close.

## Description

The KIND direct leg runs a probe as an ephemeral container of the daemon's pod. The container shares the pod's network
namespace, so it reaches the daemon's rendezvous, an abstract socket, and the pod's security context gives it the
daemon's uid. Its claim was closed unanswered, every time:

```
direct-map: could not run: connect to "default": channel: daemon unavailable at default: the daemon closed the rendezvous before answering the claim
```

The daemon's log said why:

```
slates-server: a client's claim was refused: SO_PEERCRED refused (code Some(4))
```

`getsockopt(SO_PEERCRED)` does not fail with `EINTR`.

## Root cause

Linux reports a socket peer's process id as the reader's pid namespace sees it, and 0 when the peer's process is not
visible from there (pid_namespaces(7)). The ephemeral container has a pid namespace of its own: a pod's containers share
one only with `shareProcessNamespace`, or an ephemeral container with `--target`. So the daemon read pid 0.

rustix 1.1.4 returns the credentials as `UCred { pid: Pid, uid: Uid, gid: Gid }`. `Pid` wraps a `NonZeroI32`.
`socket_peercred` fills the structure with `getsockopt` and calls `assume_init()` on it, so a pid of 0 is an invalid
value and its reading is undefined behaviour (rustix 1.1.5 declares it the same way). The compiler did not leave it
silent. `Result<UCred, Errno>` keeps its error discriminant in the pid's niche, so `Ok(UCred { pid: 0, .. })` reads back
as an `Err`. The error's `Errno` is taken from the uid's low 16 bits, which the linux_raw backend reads as a negated
errno:

| where | uid | low 16 bits | read as |
|---|---|---|---|
| the KIND pod (the image's `nonroot`) | 65532 | 0xFFFC | `code Some(4)` |
| adacachy's test user | 1000 | 0x03E8 | `code Some(-1000)` |

The rendezvous returned that error before reading the hello, and the socket closed with nothing sent. A refusal at the
uid check closes the same way, untyped.

**Why no test met it.** Every rendezvous test, and the Docker container's direct-map test, ran the daemon in the root
pid namespace, which sees every process's pid. The Docker test's container has a pid namespace of its own, but the
daemon on the host sees its child namespaces. Only a daemon in a container meets a client it cannot see: a pod's sidecar
or ephemeral container, or a client on the host of a daemon in a container.

## Impact

- **Kubernetes:** a daemon in a pod refused every client in another container of its pod, unless the pod shared its
  process namespace. A client on the node refused, the same way, a daemon running in a container.
- **vhost-user:** `VhostUserSeam::adopt` reads its front end's uid the same way. A VMM whose process the daemon cannot
  see, one jailed in a pid namespace of its own, would have been refused `PeerUnverified`, or read wrong.
- **What the pid is for.** Nothing on Linux depended on its value. A rendezvous client's liveness rides its control
  socket (`crates/server/src/peer.rs`); the pid is the probe only where no socket closes, which on Linux is never a
  rendezvous client. So a pid of 0 can be served as it is.

## The fix

- `crates/ipc/src/peer.rs` (new, Linux): `peer_credentials` reads `SO_PEERCRED` into a `libc::ucred` of plain integers.
  Its pid is 0 when the peer is not visible from here. It checks that the kernel wrote the whole structure. This is one
  `unsafe` block (an FFI call); slates-ipc's unsafe budget goes from 103 to 104, the site named in
  `unsafe-budget.toml`.
- `crates/ipc/src/rendezvous.rs`: the Linux accept reads the peer with it. `Accepted::pid` says what 0 means.
- `crates/bridge-virtiofs/src/vhost_user.rs`: `adopt` reads the front end's uid with it (slates-ipc joins the crate's
  Linux dependencies).

## Verification

- **The failing test first.** `crates/ipc/tests/rendezvous.rs`
  `a_client_whose_process_the_daemon_cannot_see_is_served` re-invokes the test binary as a listener inside a user and a
  pid namespace of its own: `unshare --user --map-current-user --pid --fork` (util-linux), the uid mapped to itself. It
  connects from outside, and the listener prints what it saw. It skips loudly where the host refuses those namespaces.
  - On adacachy (CachyOS, Linux 7.2.9, rustc 1.98.0, in a capped user unit):
    - with `rendezvous.rs` at HEAD it fails: `DaemonUnavailable { why: "the daemon closed the rendezvous before
      answering the claim" }`, the listener having seen `refused SO_PEERCRED refused (code Some(-1000))`;
    - with the fix the rendezvous suite passes 16 of 16, the new test with `accepted uid=1000 pid=0`.
  - Linux clippy of slates-ipc and slates-bridge-virtiofs (all targets, `-D warnings`) is clean, and bridge-virtiofs's
    tests pass, its seam's `adopt` among them.
- **The run that found it:** the KIND direct leg on this Mac's Docker Desktop (linux/arm64), recorded in
  `docs/wip/virtiofs.md` §8.

## Siblings

- No other read of a socket peer's credentials remains: a sweep for `socket_peercred`, `UCred`, `ScmCredentials` and
  `SO_PEERCRED` across `crates/`, `xtask/` and `vendor/` found these two sites only. rustix's `ScmCredentials`
  (`SO_PASSCRED`) builds the same `UCred` and would meet the same pid 0; slates receives none.
- **Open, reported:** the rendezvous closes a refused claim without telling the client why. Both an unreadable peer
  and another uid get the same untyped close, which the client reports as "the daemon closed the rendezvous before
  answering the claim". The capacity refusal is typed on the wire (`send_refusal`). These two are not.
- **Upstream:** rustix's `UCred::pid` cannot hold the kernel's 0. That is rustix's to change (an `Option<Pid>`, or a raw
  integer); slates no longer calls the wrapper.
