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

4. **A per-mapping `udmabuf` over the arena memfd's authorized range** — *found 2026-10-19, and missing from
   the three above.* `UDMABUF_CREATE` takes `{memfd, offset, size}` and returns a **new descriptor covering
   only that range**, without copying: it pins the memfd's existing pages rather than duplicating them. The
   descriptor is confined by construction, zero-copy survives, and **A-64 is untouched** — the arena stays one
   image per shard and only the fd handed to the front end changes.

   Measured on Fedora 6.19 (`CONFIG_UDMABUF=y`) as **uid 1000, no `CAP_SYS_ADMIN`, not in `kvm`**: a udmabuf
   made at offset 64 KiB for 128 KiB of a 1 MiB memfd maps exactly that window (byte 0 is the marker written
   inside it), and mapping the whole arena's length through the same fd is **refused**. That is the property
   options 1-3 were competing for.

   Its costs, and what is unproven:
   - **64 MB per buffer** by default (`/sys/module/udmabuf/parameters/size_limit_mb`), an admin tunable; a
     larger window needs the host raised or the mapping split.
   - **Access is deployment-dependent.** `/dev/udmabuf` is `root:kvm 0660` here, reached through an explicit
     ACL (`user:adalundhe:rw-`), not group membership. A daemon that cannot open it must **refuse DAX** rather
     than fall back to the wide descriptor. It is not a privilege slates *requires* (R10) — it is the same
     device a VMM already uses, and DAX only matters where one is running — but DAX availability would follow
     host configuration.
   - **It pins pages** for the mapping's life, unswappable and unmovable, which interacts with L2's moving
     target (§4.6) and has to be weighed against it.
   - **The hand-off is proven; only crosvm's own handler is not.** The mechanism `SHMEM_MAP` performs —
     receive a descriptor over `SCM_RIGHTS`, `mmap` it — was run across a real process boundary on the same
     host. A 4 MiB arena memfd filled with `'N'` (a neighbour's bytes) and a 256 KiB window at offset 1 MiB
     filled with `'W'`; the udmabuf for that window passed to a forked process holding **nothing else**:

     ```
     FRONTEND mapped 262144 bytes: first='W' last='W'   (the authorized window, not the neighbour's bytes)
     FRONTEND whole 4194304-byte arena through it: REFUSED
     FRONTEND at an offset past the window:        REFUSED
     ```

     So the confinement survives the descriptor crossing to another process, which is the property row 1
     exists for.

     And the rest of the shape a VMM needs was run too, on the same host — a VMM reserves the guest's DAX
     region up front and `MAP_FIXED`es each window into a slot of it, which is a path a plain `mmap` test
     would not have exercised:

     ```
     SHMEM_MAP-style MAP_FIXED into the reserved region: OK, slot byte0='W' last='W'
     SHMEM_UNMAP-style restore of the hole:              OK
     writable udmabuf mapping: OK, and the arena now reads 'Z' at the window (the write reached the memfd)
     ```

     The last line is the one that separates this from option 2: **writable zero-copy survives**. A write
     through the guest's window lands in the arena's own pages, which is what the per-mapping copy gave up.
     What is left untested is crosvm's `SHMEM_MAP` handler specifically — it mmaps what it is handed, and
     every primitive it uses is now shown to work on a udmabuf — and the harness for that already exists
     (`crates/client/examples/crosvm_dax_ab/`).

Of the four, **1 and 4 keep both zero-copy and confinement**; 4 is much the smaller change and the only one
that leaves A-64 alone, which was the whole reason 1 was hard. Neither is a change to make unilaterally — 4
still has an untested VMM leg and a pinning cost — but the choice is no longer "reverse A-64, or give
something up".

## Owed

- Ada's call between the **four** options, **before** the advertisement gate at
  `admission.rs`'s `DaxNotEstablished` is flipped.
- No test is committed asserting the current behaviour: a test that pins a weakness in place is worse
  than the record of it. When the option is chosen, the test to write is the inverse of the
  measurement above — the descriptor's extent must equal the authorized window's.
