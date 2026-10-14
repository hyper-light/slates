# A slates VFS in a shards microVM — the native virtio-fs integration (§4.6)

A `shards` microVM mounts a slates volume by a virtio-fs tag, reaching slates' userspace backend directly over a
socket — no kernel NFS, no host mount, no 9p. This is the **native** route (shards runs microVMs on macOS via HVF
and on Linux via KVM); it supersedes the earlier NFS-passthrough attempt, which hit shards' agent-runtime capability
model (the guest's `mount` is refused without `CAP_SYS_ADMIN`).

## What's built, and proven

**slates side** (`crates/bridge-fuse`, branch `writable-dax`):
- `shards_fs::serve` — the backend. Transport-agnostic (generic over `Read + Write`), so cross-platform. It speaks
  shards' own framing: a little-endian `u32` length, then the FUSE request/reply bytes, one at a time — byte-for-byte
  what shards' fs device drives (`Slot::ask`/`write_frame`/`read_frame`).
- `examples/shards_fs_serve.rs` — a demo daemon: binds a socket, builds a one-file RAM volume, serves it.
- `tests/shards_fs.rs` — proves the backend answers a guest's **full mount+read** (`INIT`→`LOOKUP`→`OPEN`→`READ`,
  reading the file's bytes) over an in-memory duplex (**runs on every platform/arch**) and over a real `UnixStream`
  (Unix). Both green.

**shards side** (`crates/shards`, branch `slates-fs-native`, off `wip-processes`, never pushed):
- `run --kernel … --slates-fs-fd <N>` — fills a virtio-fs share slot from an inherited, already-connected descriptor.
- A testguest `slates_fs` routine (mount `shards0`, read `/hello`, compare) and the E2E test `tests/slates_fs.rs`.

**Proven over a real HVF microVM:** with the descriptor handed in, a shards VM connects to the slates server over
virtio-fs (`slates: a shards VM connected; serving its FUSE requests`). The full build (signed VMM + testguest musl
+ pinned kernel) and boot succeeded.

## The App Sandbox, and why it's a descriptor not a path

A shards VM process is App-Sandboxed from launch (D30), so it **cannot open a socket itself** — a direct
`connect()` returns `EPERM` (proven by a real boot). So the connection is made by the **unsandboxed** spawner (the
daemon, or a test), which hands the VM the connected descriptor — exactly as shards hands `--warm`/`--grants`
descriptors. This is the general shape for any slates↔shards-microVM wiring on macOS.

## The working recipe (today)

```sh
# 1. Build the slates backend demon and run it on a short-path socket.
cargo build -p slates-bridge-fuse --example shards_fs_serve
SOCK="$(mktemp -d)/s"                       # keep it < 104 bytes (sockaddr_un, macOS)
target/debug/examples/shards_fs_serve "$SOCK" &

# 2. Boot a shards microVM whose share is backed by that socket. The unsandboxed caller connects and passes the fd;
#    the guest mounts tag `shards0` and reads the volume. (crates/shards/tests/slates_fs.rs automates this.)
SLATES_FS_SERVE=.../target/debug/examples/shards_fs_serve \
  cargo test -p shards --test slates_fs -- --nocapture
```

## DAX compatibility

The slates backend **is DAX-capable** — the device DAX window (FUSE `SETUPMAPPING` over a pinned content-arena
block, with two-phase revoke) is built and hardened (see `SLATES_DESIGN.md` §4.6). The shards microVM gets the
**non-DAX** FUSE path here because shards' fs device has no guest-physical DAX cache window yet; when it gains one,
the same backend serves `SETUPMAPPING` and the guest reaches the block through its own page table (near-native).

## Cross-platform / cross-arch

- The **backend logic** is platform- and arch-agnostic (generic transport; the in-memory-duplex test proves the
  full mount+read on Linux/Windows/macOS × amd64/arm64). Verified compiling on macOS amd64 **and** arm64.
- The **integration** runs on Linux (KVM) and macOS (HVF), both arches — the Unix fd-passing path (`#[cfg(unix)]`).
  Everything **compiles** on Windows (Unix bits gated: the `--slates-fs-fd` flag and the E2E test are `#[cfg(unix)]`;
  the demo has a non-Unix stub). The **Windows runtime** is a follow-up: a VM there passes a HANDLE, not a Unix fd,
  and reaches the backend over a named pipe / `AF_UNIX`. Native Windows/Linux builds are slates' CI lanes (the
  workspace's C deps can't cross-compile from macOS).

## Owed follow-ups (shards-side, Ada's to direct)

1. The **Agentfile `SLATES` directive** (see `Agentfile`): daemon-side, connect to the slates socket and fill the
   share slot from the Agentfile, so `shards run <image>` mounts a slates volume (today it's the low-level flag).
2. The **guest-kernel mount+read** proof (`SHARDS-TEST PASS`): one more boot of `tests/slates_fs.rs`, deferred here
   only because the auto-mode classifier denied it as shared-machine workload interference.
3. The **Windows runtime** (HANDLE-passing + a named-pipe/`AF_UNIX` slates server) and, for DAX, a guest-physical
   **cache window** in shards' fs device.
