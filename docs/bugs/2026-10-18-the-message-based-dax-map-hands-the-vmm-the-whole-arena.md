# The message-based DAX map hands the VMM a descriptor for the whole arena

- **Date:** 2026-10-18
- **Area:** `crates/bridge-virtiofs` (`vhost_user::BackendReqChannel::send_map`, the `DaxPlacer::Message`
  form), `crates/vfs/src/dax.rs` (`Store::dax_backing`), `crates/mem/src/arena.rs` (`block_backing`)
- **Status:** found by measurement; **NOT patched** — the fix is a design call (options below), and the
  path is unreachable in production today
- **Class:** confinement / over-broad capability handed across a trust boundary (CWE-668)

## Description

DAX has two delivery forms and they differ in who performs the mapping:

1. **fd-sharing** (`DaxPlacer::Local`, `DaxCacheRegion`): the VMM hands the backend its cache-window fd
   and **the backend** `MAP_FIXED`-overlays the content block itself. The VMM never receives a
   descriptor for slates' memory. Confined by construction.
2. **message-based** (`DaxPlacer::Message`, `VHOST_USER_BACKEND_SHMEM_MAP`): the backend is
   out-of-process and cannot reach the guest BAR, so it **sends the content block's descriptor** over
   the back-end-request socket and the **frontend** maps it.

In form 2 the descriptor sent is not the block's. It is the arena's whole shared object:

- `Store::dax_backing` returns `self.content.arena().block_backing(mapping.block)`.
- `block_backing` returns `(region.backing_fd(), region.backing_base() + extent.offset(), extent.len())`
  and says so plainly in its own comment: "`backing_fd` is the whole object".
- `BackendReqChannel::send_map(shmid, shm_offset, fd, fd_offset, len, writable)` passes that `fd` to
  `self.send(&header, &body, Some(fd))`, i.e. over `SCM_RIGHTS`.

`fd_offset` and `len` scope the **request**. They do not scope the **descriptor**. A frontend that maps
the received fd outside `[fd_offset, fd_offset + len)` reaches whatever else lives in that object —
which is every volume's content in that store.

## The measurement

A store with two tenants' volumes in one shared-object arena, tenant A's file mapped read-only, then
`fstat` on the descriptor `dax_backing` yields against the length it authorizes (macOS arm64,
16 KiB pages, 2026-10-18):

```
object_size=1048576  authorized_offset=0  authorized_len=16384
ratio = object/authorized = 64.0x
```

The authorized window is one page; the descriptor covers the whole 1 MiB region, and tenant B's blocks
are inside it. The test store is deliberately tiny — in a real shard the object is the content arena
for the whole shard, so the ratio is far worse, not better.

## Impact

Bounded, and not live:

- **DAX is unadvertised.** `admission::unsupported` refuses any request with `dax` set,
  unconditionally (`UnsupportedReason::DaxNotEstablished`). No production guest can establish a window
  at all, so nothing reaches this path.
- **The message form is harness-only.** It is reached through
  `server::virtiofs::attach_vhost_user_device_dax`, whose own doc says "For the A/B harness only — the
  hermetic daemon's attach path …".

What it would mean if advertised: in a multi-tenant host each guest has its own VMM process, and the
message form would hand VMM-A a descriptor covering other tenants' content in the same shard. Tenant
isolation would then rest on every frontend honouring `fd_offset`/`len` — on the VMM's good behaviour,
not on what slates handed it. That is the wrong side of the trust boundary for a project whose gate
exists precisely because the guest is untrusted. The fd-sharing form does not have this property.

## Why it is not patched here

The fix is architectural and is Ada's call. The options, with what each costs:

1. **A backing object per tenant** (per volume, or per attachment): the descriptor's whole extent is
   then that tenant's, and `fd_offset`/`len` stop being load-bearing for isolation. Cleanest, and it
   keeps zero-copy. Costs: cuts against A-64's one-image-per-shard content-in-place layout, and
   multiplies objects and fds by tenant count.
2. **A per-mapping `memfd` holding only the block's bytes**: confined by construction, but it is a copy
   per mapping — the "view" arm that the 2026-10-17 backing A/B already measured and rejected (one byte
   of extra RAM per byte simultaneously mapped, slope 1.0), and it loses writable zero-copy entirely.
3. **Keep form 2 for trusted frontends only** and advertise DAX only over fd-sharing: no code change,
   but it makes the crosvm/QEMU route — the one chosen on 2026-10-16 — unavailable for multi-tenant
   hosts, which is most of the point of it.

Option 1 is the only one that keeps both zero-copy and confinement; it is also the largest change, and
it touches a decided layout (A-64), so it is not a change to make unilaterally.

## Owed

- Ada's call between the three options, **before** the advertisement gate at
  `admission.rs`'s `DaxNotEstablished` is flipped.
- No test is committed asserting the current behaviour: a test that pins a weakness in place is worse
  than the record of it. When the option is chosen, the test to write is the inverse of the
  measurement above — the descriptor's extent must equal the authorized window's.
