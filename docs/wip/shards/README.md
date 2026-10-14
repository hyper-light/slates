# slates ⇄ shards microVM integration (§4.6)

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
