# A slates VFS in a shards microVM — the native virtio-fs integration (§4.6)

A `shards` microVM mounts a slates volume by a virtio-fs tag, reaching slates' userspace backend directly over a
socket — no kernel NFS, no host mount, no 9p. This is the **native** route (shards runs microVMs on macOS via HVF
and on Linux via KVM); it supersedes the earlier NFS-passthrough attempt, which hit shards' agent-runtime capability
model (the guest's `mount` is refused without `CAP_SYS_ADMIN`).

## What's built, and proven

> **2026-10-24: the daemon now serves a real volume (A-161).** `Daemon::attach_shards_stream` attaches a volume (whole
> or one directory, for reading or writing within the principal's rights), makes an anonymous socket pair and serves
> one end in Shards' framing on the volume's owner shard, control asks included (refused in Shards' forms; a DAX
> descriptor ask `EOPNOTSUPP`). The other end is the attachment's capability. Proven by use against a real daemon
> (`crates/server/tests/shards.rs`). Over the wire the end reaches a client by Linux's control socket or macOS's
> encrypted socket route (`Client::attach_shards`), and `slates shards ID --fd N -- CMD` runs a spawner with it at
> descriptor `N`, so the recipe below no longer needs the one-file example, which goes. Not yet: a real boot against
> the daemon (Shards part 2). Until a DAX backing scoped to one attachment exists (part 3), launch a slates share with no
> DAX window.
>
> **The Shards half, checked 2026-10-24 (read-only, from this checkout's remote refs; no fetch):** `slates-fs-native`
> (08066b27) has been on origin since 2026-10-22; `origin/gpu` (1f82788c, 2026-10-24) still contains none of it.
>
> **Status checked 2026-10-21, and two things below were overstated.** (1) The slates half is on **main**, but
> **the daemon never calls `shards_fs::serve`** — no reference in `crates/server` or `crates/cli`. The only server
> is `examples/shards_fs_serve.rs`, which serves a hard-coded one-file RAM volume, so a real volume (its
> attachment, capability, scope and rights) cannot be served to a shards VM yet. (2) The shards half
> (`--slates-fs-fd`) is on shards branch `slates-fs-native` (`08066b27`, 2026-10-14), which is on **no remote**;
> shards' `origin/gpu` (`0aa00196`, 2026-10-21), the branch kiln vendors, contains none of it. "Proven" below
> means a real HVF VM **connected** to the example server; a guest kernel mounting and reading has never run.
> *(Corrected 2026-10-24: wrong. Shards' boot test, which passes only after the guest mounts `shards0` and reads
> `/hello` byte for byte, passed on 2026-10-14, 36.45 s, and 2026-10-20, 48.7 s, against the example; GAPS
> `8a-shards-boot`.)*

**slates side** (on main; updated 2026-10-24):
- `crates/bridge-fuse/src/shards_fs.rs` — the byte work: `split_frame` takes a frame (a little-endian `u32` length,
  then the FUSE request; the top bit marks Shards' control asks) from the bytes read, bounded before anything is
  allocated; `answer` dispatches it through the FUSE bridge and writes the reply frame; the control asks are refused in
  Shards' forms. `tests/shards_fs.rs` drives a guest's full mount and read through them on every platform.
- `crates/server/src/shards.rs` — the daemon serves an attachment on one end of an anonymous socket pair, on the
  volume's owner shard; `crates/server/tests/shards.rs` drives it as Shards' device would.
- The end reaches a client by Linux's control socket or macOS's encrypted socket route (`Client::attach_shards`), and
  `slates shards ID --fd N -- CMD` runs a spawner with it at descriptor `N` (`crates/cli/tests/cli.rs`).
- The one-file example server (`examples/shards_fs_serve.rs`) and its blocking `serve` were removed on 2026-10-24:
  the daemon serves the route now.

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

## The recipe

```sh
# 1. Run the daemon (under its anchor) and make a volume.
slates anchor &
id=$(slates volume create agent --bounded 1GiB --json | jq -r .id)

# 2. Boot a shards microVM whose share is that volume. `slates shards` attaches it in the Shards form and runs the
#    unsandboxed spawner with the stream end at descriptor 3, which hands it to the VM as `--slates-fs-fd 3`; the guest
#    mounts tag `shards0`. The attachment ends when the VM's end closes. Add `--read-only` or `--subtree DIR` to narrow
#    what the guest reaches.
slates shards "$id" --fd 3 -- shards run --kernel … --slates-fs-fd 3
```

Not yet run against a real VM: `slates shards` is proven with a test program that mounts and reads through the end it
is handed (`crates/cli/tests/cli.rs`), and Shards' `--slates-fs-fd` is on `slates-fs-native`, not `gpu` (part 2).

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

## Owed follow-ups (Ada's to direct)

0. **Done 2026-10-24 (A-161): the daemon serves a real volume, and `slates shards` hands the spawner its end.** What
   was owed, kept for the reasoning: **slates side, and the largest gap: the daemon serving a real volume.** A daemon verb that attaches a volume
   (or creates a session's copy-on-write overlay) for a guest, with a scope and rights, and returns a **connected
   descriptor** that the real daemon serves in shards' framing. The descriptor is the capability: whoever holds it
   reaches exactly that attachment. It must be a descriptor, not a path, because the macOS App Sandbox stops the VM
   process from opening a socket (shown on a real boot). **Isolation on this route is one attachment per session,
   never per uid:** every FUSE request carries a uid, but inside a microVM that is whatever the guest kernel says,
   and the guest can be root.
1. The **Agentfile side**, revised 2026-10-21. Split the **declaration** from the **binding**. The Agentfile
   (shards' syntax, shards' call) declares only where the agent mounts a slates volume and whether it may write —
   no socket path and no volume id, because an Agentfile is a signed artifact pushed to a registry, and a host
   socket path is per-run state that doesn't belong in a signed spec any more than a bind source does. Reusing
   shards' `VOLUME … FOR` (D109) may serve. The **binding** happens at launch: the runner asks the slates daemon
   for a descriptor for one attachment and passes it as `--slates-fs-fd`. This supersedes the earlier proposal,
   `SLATES ${SLATES_SOCK} shards0 /slates`, which put the socket in the Agentfile.
2. The **guest-kernel mount+read** proof (`SHARDS-TEST PASS`): passed against the one-file example on 2026-10-14 and
   2026-10-20 (GAPS `8a-shards-boot`); owed again against the daemon, through `slates shards` (Shards part 2), now that
   the example is gone.
3. The **Windows runtime** (HANDLE-passing + a named-pipe/`AF_UNIX` slates server) and, for DAX, a guest-physical
   **cache window** in shards' fs device.

---

## Superseded: the NFS path, and the shards-side DAX gap (recorded 2026-10-14)

The native virtio-fs route above replaced the NFS-passthrough attempt below. The findings are kept because they
were verified on this machine and they still name the shards-side contract: the capability refusal is why the NFS
route was abandoned, and the VMM reading is why DAX inside a shards microVM remains a shards feature. Nothing here
describes the current route — it is the record of the one that was measured and rejected. The NFS variant of the
`Agentfile` (and `demo.sh`'s forwarder recipe) belongs to this superseded route; the `Agentfile` in this directory is
the native one, and the NFS variant stays in git history on `ci-repair-2026-10-13`.

Applying a slates VFS inside a [shards](../../../../shards) microVM, and where DAX stands.

## Proven on this machine (macOS arm64, 2026-10-14)

**shards runs microVMs here, via Hypervisor.framework (HVF) — no KVM, no Linux needed.** shards'
`crates/vmm/build.rs` selects `hv="hvf"` for `(macos, aarch64)`; the installed `shards` binary
(`~/.local/bin/shards`, 0.1.0) booted a microVM and ran a command in one step:

```
$ shards run --rm alpine echo hello-from-shards-on-hvf
hello-from-shards-on-hvf        # exit 0
```

(An earlier claim in this session that the microVM work was "KVM-gated" was wrong for shards — that
holds only for **firecracker**, which is KVM-only and has no HVF backend, so it cannot run on this Mac.)

## Applying a slates VFS — the NFS path (works today, no DAX)

A shards microVM reaches a slates volume over slates' NFS transport. Two facts shape it:

1. **slates' NFS listener is loopback-only** by design (`crates/server/src/nfs.rs`: "one loopback
   listener"; loopback reachability is deliberately *not* consumer authority, §4.13/AUD-01). A guest
   reaches the host at the shards bridge **gateway**, not `127.0.0.1`, so a tiny host-side forwarder
   bridges `gateway:PORT → 127.0.0.1:slates-nfs-port`. `demo.sh` uses a python3 forwarder (socat is
   not installed here; `nc` is).
2. **`shards run` has no `-v`/`--mount`/vhost-user flag** — the guest mounts over the network itself.
   The guest self-discovers the host as its default route (`ip route … default`), so no address is
   baked in.

`demo.sh` wires it: run the `nfs_async` example (a real RAM-only volume with a seeded `hello.txt`),
forward its loopback port, then `shards run` an alpine microVM that mounts it. **Proven as far as
shards' security boundary (2026-10-14):** the microVM boots (HVF), the forwarder carries the guest's
request to slates' NFS listener, and the guest reaches the `mount` syscall — which shards refuses:

```
--- (microVM booted; forwarder on 0.0.0.0:FWD -> 127.0.0.1:slates-nfs) ---
mount: permission denied (are you root?)   # even with `shards run --user root`
```

The blocker is **shards' capability model, not KVM**: shards is a hardened agent runtime that drops
`CAP_SYS_ADMIN`, exposes no `--privileged`/`--cap-add` on `run`, and has no agentfile directive to grant
a container the capability (`AGENT`/`HARNESS`/`SKILL`/`MCP`/`NETWORK`/`CONNECT`/`ATTACH`/`VOLUME`/`EXPOSE`
are the directives; privilege is an internal agent-run notion, deliberately not a user knob). It also
mounts only the image rootfs and its own host-backed `VOLUME`s — there is no directive to mount an
external NFS/9p/vhost source. So **slates-in-a-shards-microVM needs a shards feature**: either a
mount-capability / external-volume directive (for the NFS path), or shards-init performing the mount at
boot from an agentfile field. `Agentfile` is the declarative form the demo mirrors; it, too, needs that
capability to mount.

## DAX inside a shards microVM — the honest gap

DAX is **not** available over NFS (NFS has no mapping path). slates' DAX is virtio-fs `SETUPMAPPING`
over a cache window, and its vhost-user-fs **backend** already serves a live guest under QEMU
(§4.6 AC-9.7, the green `live-guest` CI lane). The missing piece is on the **shards side**:

Verified by reading shards' VMM (`../shards/crates/vmm/src/devices/virtio/`, 2026-10-14), which decides the
exact size of the shards-side build:

- shards **has** a virtio-fs device (`fs/mod.rs`), alongside virtio-blk/net/vsock. But it forwards FUSE
  frames over a Unix socket in its **own length-prefixed framing** (`write_frame`/`read_frame`) to shards'
  **own passthrough FUSE server** (`fs/server.rs`, `fs/host.rs`) — a host-directory passthrough. It is
  *not* the vhost-user protocol, and it does **not** negotiate DAX: there is no `SETUPMAPPING`, no
  `FUSE_MAP_ALIGNMENT`, no cache region / shared-memory window anywhere in the device.
- slates' backend speaks the **vhost-user-fs** protocol (`crates/bridge-virtiofs`, its vhost-user module),
  a different wire protocol, and serves the DAX `SETUPMAPPING`/cache-window path (already proven against a
  live QEMU guest, §4.6 AC-9.7).

So two independent builds are needed on the shards side, and both are in shards' VMM, to shards' own bar
(real-VM E2E tests, sub-5 ms boot): **(1) protocol** — either shards' fs device learns the vhost-user
protocol to attach slates' backend socket, or the two agree on one FUSE-over-socket framing (a non-DAX
slates mount could be reached this way first); and **(2) DAX** — shards' fs device grows a guest-physical
**cache window** (a shared-memory region mapped into guest physical space via Hypervisor.framework on
macOS / KVM memory slots on Linux) plus `SETUPMAPPING`/`REMOVEMAPPING` handling that `MAP_FIXED`-overlays
the backend's block pages into it. That is a major cross-project VMM feature in shards' domain (its own
rules, its real-VM test bar), **not** a vague "needs a VMM": the slates side it connects to (the
vhost-user-fs backend + the hardened DAX device window + the daemon→device revoke channel) is built and
tested on branch `writable-dax`, and this names the precise shards contract. On **this** Mac a *live* run
is additionally blocked: firecracker needs `/dev/kvm` (absent even `--privileged` in Docker here, verified
2026-10-14), and shards' HVF path has no vhost-user-fs DAX yet — so the slates side is exercised through
its in-process device seam, not a live microVM.

firecracker's DAX, by contrast, is **virtio-pmem** (it has no virtio-fs at all), a different device +
a flat-image representation — and it needs KVM, so it cannot run on this Mac regardless.
