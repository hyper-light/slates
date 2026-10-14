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

- shards' guest filesystem is its **share-process virtio-fs**, a host-directory passthrough that does
  **not** negotiate DAX (`FUSE_MAP_ALIGNMENT` is not among the flags its server sets) and cannot be
  pointed at a RAM-only slates volume (R1: slates exposes no host directory).
- shards has **no vhost-user-fs frontend**, so it cannot attach slates' existing vhost-user-fs DAX
  backend as a second device.

So "a shards microVM applying a slates VFS **with DAX**" requires building a **vhost-user-fs frontend
in shards** (a virtio-fs PCI device that connects to slates' vhost-user socket and carries the DAX
cache window), then opting the volume into writable DAX and advertising it once the tamper-proof/fleet
registry gate is met (§4.6 A-9; see memory `dax-mapping-gate`). That is a shards-side build in a
separate, actively-developed project — out of scope to land unilaterally from here, but now a concrete,
scoped next step rather than a vague "needs a VMM." The slates side it would connect to (the
vhost-user-fs backend + the hardened DAX device window) is built and tested on branch `writable-dax`.

firecracker's DAX, by contrast, is **virtio-pmem** (it has no virtio-fs at all), a different device +
a flat-image representation — and it needs KVM, so it cannot run on this Mac regardless.
