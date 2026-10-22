# The virtio-fs guest device (§4.6 A-9, D-2, RQ-20; GAP-A9-5): design and measurement record

> Status (2026-09-13). The owned FUSE-over-virtio device exists as `crates/bridge-virtiofs` and is
> wired into the daemon (`crates/server/src/virtiofs.rs`): a sans-io split virtqueue over a bounded
> guest-memory seam, the FUSE-over-virtio request cycle through the FUSE codec onto the shared
> `Bridge`, device admission that authenticates the consumer before any queue, mapping or tag with
> per-chain credits and an owned terminal step, and the device loop as a perpetual task on the
> volume's owning shard over `slates-rt`, reached by the same cross-shard spawn the NFS transport
> uses. Proven by use with a simulated guest driver writing real §2.7 rings: byte-identical with
> direct FUSE dispatch over 15 requests; a guest's file on a daemon-provisioned volume read back
> over the daemon's NFS port; T-4.14's malformed chains, overflow lengths and adjacent-page ranges
> refused before access; revoke while requests are pending refused before access and reclaimed.
> DAX is not advertised. **Not yet:** a real VMM binding (libkrun in-process; vhost-user for the
> inherited-descriptor form — the trait models both, the in-process form is served, the other is
> refused typed `AttachmentUnsupported{InheritedDescriptor, BindingNotBuilt}`), the guest form's
> durable `AttachmentRecord` and the transport report on the `attach`/`status` wire, a live Linux
> guest (AC-9.7: the conformance evidence names the simulated driver, never a live guest), and the
> OCI namespace handoff (§8 below). This document is the assistant-owned record; the numbered
> requirements live in `SLATES_DESIGN.md` §4.6 and the ledger in `GAPS.md`.

## 1. The requirement

§4.6 "virtio-fs and OCI attachment contract (A-9)" (design lines ~1399–1427), D-2 and RQ-20 (Ada,
2026-09-05: host processes, OCI containers and Linux microVM guests consume the same VFS; guests
use virtio-fs). The sentences this crate is built to, each with where it is enforced:

| Contract sentence | Enforced by |
|---|---|
| "The guest transport is FUSE-over-virtio served by an owned device integrated with the custom executor." | `device.rs` (the cycle), `serve.rs` (the loop on `slates-rt`), `crates/server/src/virtiofs.rs` (on the owning shard). No foreign runtime, no thread. |
| "The seam accepts guest-memory/queue capabilities and completion notification from the host VMM through an in-process interface or an inherited descriptor." | `admission::VmmSeam` — one trait, two forms (`GuestTransport::{InProcess, InheritedDescriptor}`), a `Doorbell::{InProcess, Descriptor(fd)}` and `notify_used`. |
| "Device admission authenticates the consumer before creating a queue, mapping guest memory or publishing a tag." | `admission::admit`: `seam.consumer()` first; rights from the access list only then; queues read, memory mapped, queues validated after; the tag published last. Asserted from the simulated seam's recorded call order (`tests/admission.rs`). |
| "Queue descriptors, scatter/gather ranges, arithmetic and chained lengths are validated within derived caps before access." | `virtqueue::Virtqueue::peek` — every check in §4 below, before any buffer byte; a refusal faults the queue (`DEVICE_NEEDS_RESET` semantics). Proven by the simulated memory's access log. |
| "In-flight requests, mapped bytes, copy buffers and replies consume the attachment's credits; cancellation and revocation reclaim them under an owned terminal step." | `credit::CreditLedger` charged per chain after the walk and before access, released at the used-element publish; `AdmittedDevice::revoke` + `reclaim` (sweep → revoke → drain → credits whole → seam released last). |
| "Requesting an unsupported form returns `AttachmentUnsupported{transport, reason}`." | `AdmissionError::AttachmentUnsupported { transport, reason: UnsupportedReason }` for DAX, the notification queue, and the unbuilt inherited-descriptor binding — before the seam is touched. |
| "Capabilities ... must be reported by `attach` and `status`: supported transport, target-path constraints, read/write policy, sharing/cache semantics, residency boundary and conformance evidence." | `capability::TransportCapability` (`AdmittedDevice::capability`, `host_capability`, `server::virtiofs::guest_transport_capabilities`). The wire-level surfacing on `attach`/`status` is owed (§8). |
| "The baseline contract does not require DAX; a requested DAX capability cannot be advertised until mapping isolation, pinning and teardown have been established for that VMM." | The codec's INIT negotiation never keeps `FUSE_MAP_ALIGNMENT` (tested: a guest offering it gets a reply without it); the report says `dax.advertised = false` with the contract's sentence as the reason; a DAX request is refused typed. |
| "No disk socket, image construction, target mkdir or privilege escalation is implicit." | The crate holds no `std::fs`, no `std::net`, no `unsafe`; the seam owns every descriptor; R10 holds by construction. |

## 2. What is built

```
crates/bridge-virtiofs/
  src/memory.rs      the guest-memory seam: GuestAddr, GuestRange (end proven at construction),
                     GuestMemory { check, read, write }, typed refusals, little-endian field reads
  src/sim.rs         SimGuestMemory: Vec<u8> regions, an access log and counters (the SimHost pattern)
  src/virtqueue.rs   the split virtqueue (virtio 1.2 §2.7): layout validation, peek/advance/pop,
                     push_used (element before index), interrupts_wanted, the closed refusal taxonomy
  src/device.rs      the FUSE-over-virtio cycle (§5.11): gather → dispatch → scatter → publish; the
                     hiprio queue; INIT/DESTROY observed; the derived caps; faults
  src/credit.rs      AttachmentCredits (derived), CreditLedger, ChainAdmission, Unlimited
  src/admission.rs   VmmSeam, admit (the contract's order), AdmittedDevice::{service, revoke, reclaim}
  src/capability.rs  the truthful transport report; DAX_NOT_ADVERTISED_REASON
  src/serve.rs       (Unix) serve_loop on slates-rt: doorbell readiness → drain → passes; the
                     per-shard revoke registry; ServeEnd
  tests/common/      the simulated guest driver (real rings, chains, submit/reap/reply) and the
                     simulated VMM seam (records the order it is called in)
  tests/virtqueue.rs tests/device.rs tests/admission.rs tests/serve.rs
crates/server/src/virtiofs.rs   Daemon::attach_guest_device, spawn_on_owner, ShardBridge,
                                guest_transport_capabilities; DaemonConfig::guest_credits
crates/server/tests/virtiofs.rs the daemon end to end (a guest's file read back over NFS)
```

Commits, in order (branch `agent/virtiofs`): `0641df2` the virtqueue machine; `d42d5ff` the
request cycle + the codec siblings; `874fe51` a literal-marker fix; `0bbb02c` admission and
credits; `35e86ff` the runtime loop; `2404108` the daemon wiring.

The pattern by canonical example (for CLAUDE.md §6, if the integrator wants a line): *a device
edge built without the device present*: `crates/bridge-virtiofs/tests/common/mod.rs` — the
driver half of a hardware protocol written as a simulator over the same byte layouts the real
driver writes, so the device half is proven by use with no VM; the differential oracle's other
leg is the transport-free path (`slates_bridge_fuse::bridge::dispatch`). Gotcha: the two legs
must share a deterministic clock (`StepClock`) or the timestamps in the replies differ.

## 3. Derived caps and credits

Every number below is a `Format:` constant (a specification's value, cited), a `Derived`
(formula + anchors, logged), or a `Shape:` (a test's fixture size). None is a tuning literal.

| Quantity | Formula | Anchors | Where | Value |
|---|---|---|---|---|
| chain descriptor cap | the queue size (§2.7.5.2: a chain is never longer than the queue) | virtio 1.2 §2.7.5.2 | `device::chain_caps` | = queue size |
| readable bytes per request | `fuse_in_header + fuse_write_in + max_write` (the largest request a guest kernel sends is one full write) | `fuse.in_header_len` 40, `fuse.write_in_len` 40, `fuse.max_write` 256 KiB (`slates_bridge_fuse::init::MAX_WRITE`) | `device::readable_cap` | 262 224 bytes |
| writable bytes per request | `fuse_out_header + FUSE_DEFAULT_MAX_PAGES_PER_REQ × largest guest base page` (the largest READ reply a guest kernel asks for when the daemon leaves `max_pages` at the default) | `fuse.out_header_len` 16, `FUSE_DEFAULT_MAX_PAGES_PER_REQ` 32 (`fs/fuse/fuse_i.h`), the largest Linux base page 64 KiB (arm64/ppc64) | `device::writable_cap` | 2 097 168 bytes |
| request queues offered | one per device task: the volume's one owning shard serves the device (D-7); a second queue would be polled by the same task and add no parallelism | D-7 | `DeviceConfig::new` | 1 (+ the hiprio queue) |
| request credit | the owning shard's admission limit, Little's law: request rate × p99 service time (`requests_in_flight_per_shard`); one attachment may use its shard's whole in-flight budget, which the daemon splits among attachments | `rt.requests_in_flight_per_shard` | `AttachmentCredits::derive`, `DaemonConfig::guest_credits` | 20 with the daemon's assumed rate/service (`4e6 × 5e3 / 1e9`), re-derived when measured |
| byte credit | `clamp(bandwidth × rtt, one frame, bandwidth × class latency budget)` — the §4.9 credit window (`slates_wire::credit::window_bytes`) with bandwidth = memcpy bytes/ns at the largest measured copy, rtt = wake p99 (a kick's round trip through the driver), frame = readable cap + writable cap, budget = the provisioning latency budget | `memcpy`, `wake.p99_ns`, the two caps, `rt.latency_budget_ns` | same | machine-derived; logged at boot as `guest_bytes_credit` |
| devices per shard (loop registry bound) | the shard's admission limit (`clients_per_shard`) | `rt.requests_in_flight_per_shard` | `serve::register(bound)` | = the admission limit |
| sim access log | 65 536 entries, twice the largest queue's descriptor count | Shape | `sim.rs` | — |

The device's per-pass work is bounded by the request credit (the batch), and each pass yields
between rounds (§4.3 "bounded work everywhere").

## 4. The checks the walk makes, and their refusals

`Virtqueue::new` (configuration): queue size a power of two in `1..=32768` (§2.7); each ring at
its alignment (descriptor table 16, available ring 2, used ring 4; §2.7) and wholly inside guest
memory (`RingMisaligned`, `RingOutsideGuestMemory`); the three rings disjoint (`RingsOverlap`);
the descriptor cap in `1..=size` (`ChainCapInvalid`).

`Virtqueue::peek` (per chain, before any buffer byte): the available index at most `size` ahead
(`AvailableIndexAhead`); the head inside the table (`HeadOutOfRange`); no descriptor visited twice
(`DescriptorLoop`, a visited bitset); the chain within the descriptor cap (`ChainTooLong`); no
`VIRTQ_DESC_F_INDIRECT` (`IndirectNotNegotiated` — the feature is not offered, so the flag is a
§2.7.5.3.1 driver violation); `addr + len` not overflowing (`LengthOverflow`); the buffer inside
one mapped region, never straddling an edge or a hole (`BufferOutsideGuestMemory` — T-4.14's
"unauthorized adjacent-page ranges"); the buffer off the queue's own rings (`BufferOverlapsRing`);
every device-readable descriptor before every device-writable one (§2.7.4.2,
`ReadableAfterWritable`); the readable and writable totals within the caps
(`ReadableBytesOverCap`, `WritableBytesOverCap`); `next` inside the table (`NextOutOfRange`).
A refusal faults the queue: every later `peek` repeats it (`chains_refused` counts once).

`Device::serve_chain` (after the walk, before access): readable ≥ `fuse_in_header`
(`RequestTooShort`); on a request queue writable ≥ `fuse_out_header` (`ReplyBufferTooSmall`);
the attachment's credits (`CreditRefused`). A reply the posted buffers cannot hold is answered
`EIO` in the room there is (`replies_truncated` counts), never left waiting.

Wire formats, each a `Format:` constant with its citation: virtio 1.2 §2.7.5 (descriptor: addr
le64, len le32, flags le16, next le16; `NEXT=1`, `WRITE=2`, `INDIRECT=4`), §2.7.6 (available
ring: flags, idx, ring[size], used_event; `NO_INTERRUPT=1`), §2.7.8 (used ring: flags, idx,
ring[size] of {id le32, len le32}, avail_event; §2.7.8.2 the element before the index), §5.11.1
(device id 26), §5.11.2 (queue 0 hiprio, request queues follow), §5.11.4 (`tag[36]` NUL-padded,
`num_request_queues`), §5.11.6 (`virtio_fs_req`: in-header + in-payload readable, out-header +
out-payload writable; `FUSE_FORGET`/`FUSE_BATCH_FORGET`/`FUSE_INTERRUPT` on the hiprio queue);
`include/uapi/linux/fuse.h` (`fuse_write_in` 40 bytes, `FUSE_BATCH_FORGET` 42,
`FUSE_MAP_ALIGNMENT` 1<<26); `fs/fuse/fuse_i.h` (`FUSE_DEFAULT_MAX_PAGES_PER_REQ` 32);
`fs/fuse/dev.c` (an `ENOSYS` reply to an interrupt sets `no_interrupt`). **The virtio section
numbers are quoted from memory of virtio 1.2 and must be verified against the published
specification before they are cited outside this tree; the field layouts and flag values are the
ones every Linux driver writes and the tests exercise.**

## 5. Tests and evidence (macOS 26.4 / Darwin 25.4.0, Apple Silicon, rustc 1.98.0, 2026-09-13; the
box shared with two other builds — timings are not benchmarks)

- `cargo test -p slates-bridge-virtiofs` → 34 pass, 0 fail: 14 `virtqueue` (every refusal above,
  the access log proving no buffer access, the used element before the index, ring wrap), 7
  `device` (the differential oracle: INIT, LOOKUP miss, CREATE, a 3 000-byte WRITE gathered from
  three descriptors, GETATTR, a 2 000-byte READ scattered over three, OPENDIR/READDIR/RELEASEDIR,
  RELEASE, UNLINK, a hiprio FORGET, the reclaimed inode, STATFS, DESTROY — 15/15 byte-identical
  with `dispatch` on a second identical volume; DAX not advertised; the batch bound; malformed
  requests faulting; the EIO answer; configuration validation; the tag), 7 `admission` (the call
  order; unsupported forms untouched; credits charged/released; a chain beyond the byte credit
  refused before access; T-4.14 revoke-and-reclaim; the truthful report; the host report), 3
  `serve` (three kicks served through the kqueue driver and the loop ended on hangup; revoke by
  message wakes the loop; no-doorbell ends at once), 3 unit.
- `cargo test -p slates-server --test virtiofs` → 2 pass in 1.16 s: a guest's CREATE/WRITE/RELEASE
  through real virtqueues on a daemon-provisioned volume, read back over the daemon's NFS port
  byte for byte; a consumer off the access list refused `EPERM`.
- `cargo test -p slates-bridge-fuse` → 32 pass (2 new: BATCH_FORGET dispatched with no reply and
  bounded by the body; DESTROY served with the sweep).
- Failing-first: `cargo test -p slates-bridge-virtiofs --test virtqueue` red with 3 unresolved
  imports before piece 1; `--test admission` red with 6 before piece 3; the two codec tests red
  (`BATCH_FORGET` answered 16 bytes of `ENOSYS`; `DESTROY` `-ENOSYS`) before the codec fix.
- Gates: `cargo fmt --check`; `cargo clippy -p slates-server -p slates-bridge-virtiofs -p slates-rt
  -p slates-bridge-fuse --all-targets -- -D warnings` clean; `cargo xtask check` structural /
  literals / unsafe ok (bridge-virtiofs budget 0, no `unsafe`; rt 49/49 unchanged).
- Cross-lint from macOS for the Linux target: the plain command dies in `zstd-sys`'s build
  script (pulled by `slates-vfs → slates-archive`, so every bridge crate's Linux cross-lint has
  it; no cross C toolchain is installed). With Apple clang standing in for the build script's C
  compiler — environment variables only, no install —
  `CC_x86_64_unknown_linux_gnu=clang CFLAGS_x86_64_unknown_linux_gnu="--target=x86_64-unknown-linux-gnu -nostdlibinc -isystem $(xcrun --show-sdk-path)/usr/include" cargo clippy -p slates-bridge-virtiofs --target x86_64-unknown-linux-gnu --all-targets -- -D warnings`
  → `Finished`, no diagnostics. The crate has no `target_os = "linux"` branch (only `cfg(unix)`,
  exercised natively here); the CI `gates` matrix lints it natively on ubuntu as part of
  `--workspace`.

## 6. Decisions and what was tried

- **The seam owns the doorbell.** `Doorbell::Descriptor(i32)` carries the number only; the loop
  awaits its readiness through the shard's driver (`slates_rt::readiness::readable`, made public
  for exactly this) and asks the seam to drain (`VmmSeam::drain_doorbell`). The alternative — a
  `BorrowedFd` in the doorbell and the loop reading it — needs `BorrowedFd::borrow_raw` (unsafe)
  or a lifetime on `Doorbell`; the crate stays at zero `unsafe`.
- **`peek`/`advance` instead of "un-pop".** A chain is admitted against the credits after the
  walk validates it and before it is consumed; a refusal leaves the ring position untouched and
  faults the device. The alternative (pop, then push back) would re-walk or trust a cached chain.
- **Credit backpressure is not modelled** because this device completes each request before it
  takes the next: a credit refusal can only mean one chain alone exceeds the attachment's credit,
  which is a hostile or misconfigured driver, so it faults rather than skips. An asynchronous
  completion path (a cross-shard bridge call) would add backpressure then — and the ledger's
  `charge`/`release` already carry the in-flight counts it needs.
- **Indirect descriptors are refused, not implemented**: `VIRTIO_F_INDIRECT_DESC` is not offered;
  the Linux virtiofs driver does not need it (its chains are direct); implementing it fully
  (a second table, its own bounds and loop checks) is a later leg if a VMM negotiates it.
- **A refusal faults the queue rather than skipping the chain**: the specification's
  `DEVICE_NEEDS_RESET` (§2.1.2). Skipping would let a hostile driver probe the device's checks one
  chain at a time.
- **The rights are a function of the authenticated principal** (`admit(..., rights)`), read from
  the volume's access list only after the seam has said who the consumer is: §4.13's order.
- **`DESTROY` and `BATCH_FORGET` fixed in the codec, not worked around in the device**
  (`docs/bugs/2026-09-13-fuse-destroy-and-batch-forget-unserved.md`): the `/dev/fuse` transport
  had the same gaps.
- **The reply buffer is zero-filled per request** (`Vec::resize` to the posted writable size:
  up to 2 MiB under the cap, typically 128 KiB for a READ). Measured-later refinement: keep the
  buffer sized to the largest reply seen and write the used length only; the FUSE codec already
  writes only `n` bytes.

## 7. Siblings found

- `bridge-fuse`: `FUSE_DESTROY` fell to `ENOSYS` and never swept the attachment's references;
  `FUSE_BATCH_FORGET` was not in the opcode set (answered `ENOSYS`). Fixed failing-first (the bug
  record above). The `/dev/fuse` transport benefits identically.
- `bridge-fuse::init::MAX_WRITE` is now `pub`: it is the anchor of the device's readable cap, and
  the two must be the same number.
- `slates-rt`: `readiness::readable` is public; the module doc names the doorbell use.

## 8. Owed, with what each needs

> **2026-10-01 (AUD-29-68):** item 1's inherited-descriptor half is built — `src/vhost_user.rs`, the vhost-user back
> end, Linux, proven by a test front end over a socketpair with a sealed memfd and eventfds
> (`crates/server/tests/virtiofs.rs`). Still owed from item 1: the in-process libkrun seam and a live guest
> (QEMU `vhost-user-fs-pci,chardev=c` with `-chardev socket,id=c,fd=N`; since run locally, 2026-10-01).
> Item 2's durable record: built 2026-10-01 (`Consumer::Guest`, `AttachForm::GuestTag`). The transport report on
> the `attach`/`status` wire predates it.

1. **A real VMM binding.** In-process (Hecate's libkrun, the reference): a `VmmSeam` over
   libkrun's device-backend interface — guest memory as the mapping libkrun hands the backend,
   the kick as its ioeventfd, the call as its irqfd; the memory implementation must read the ring
   indices with acquire/release ordering (§2.7.13–14) through `slates_mem::SharedObject`'s atomic
   views, since the guest writes them from another thread. Inherited descriptor (vhost-user): the
   same seam over `VHOST_USER_SET_MEM_TABLE` regions mapped from the inherited descriptors and the
   `SET_VRING_KICK/CALL` eventfds. Both need a Linux host with KVM (or macOS with HVF via libkrun)
   to prove live; the `SimVmm`/`PipeVmm` seams are the oracle they must agree with.
2. **The guest form on the wire.** A durable `AttachmentRecord` with a guest consumer/form
   (`Consumer::Guest`, `AttachForm::GuestTag { tag }` — append-only `Wire` enum variants), and
   `attach`/`status` carrying `TransportCapability` (`StatusReport`/`DaemonReport` gain a field;
   the CLI's `--json` prints it). Today the report exists in-process
   (`guest_transport_capabilities`, `AdmittedDevice::capability`); `Refusal::AttachmentUnsupported`
   on the wire is part of the same change.
3. **Live-guest conformance (AC-9.7).** `Conformance::LiveGuest` may only appear after a Linux
   guest has mounted the tag and run the §6 workloads; the report says `SimulatedGuestDriver`
   until then, by construction.
4. **The OCI namespace handoff (the container side of GAP-A9-5).** What it needs: a host runtime
   (runc/crun/youki) is handed the established host attachment's mount (the FUSE mount on Linux,
   the NFS mount on macOS) as a bind mount into the container's mount namespace — no new daemon
   privilege; the launcher (`slates exec`, §4.6) already does `CLONE_NEWUSER|CLONE_NEWNS` + bind,
   so the OCI form is an OCI hook or a `mounts[]` entry naming the host path, plus the
   capability report line for "OCI on host" (target path = the container's mountpoint, sharing =
   the host mount's). Inside a guest, containers consume the guest's virtio-fs mount, which needs
   item 1. Nothing of this touches this crate.
5. **`FUSE_INTERRUPT` handling beyond `ENOSYS`** only if a request can ever be in flight across
   passes (item 1's asynchronous completion); today `ENOSYS` is the ABI's correct "no interrupts".
6. **The notification queue** (`VIRTIO_FS_F_NOTIFICATION`) and **indirect descriptors**, if a VMM
   negotiates them; refused typed today.

7. **How a VM or container reaches a slates volume in production: as a mount (decided 2026-10-21, Ada).** A
   slates volume is mounted on the host as an ordinary folder (`slates mount`: NFS on macOS, FUSE on Linux), and each
   platform shares that folder in with its own mechanism, including its own DAX where it has one. slates needs no
   new transport for this, and every platform it targets is covered:

   | target | how it reaches the volume | DAX |
   |---|---|---|
   | bare metal | the host mount | slates' own: a read-only map of slates' memory handed to a program (A-135, built) |
   | OCI (Docker, Kubernetes on runc) | the host mount bound into the container (A-28, proven 2026-10-19) | not applicable: no VM, the host kernel serves reads |
   | Shards | the host mount shared with `VOLUME … FOR` | Shards' DAX window (D189, `crates/vmm/src/devices/virtio/fs/mod.rs` on `gpu`, read 2026-10-21) maps the host file |
   | QEMU, crosvm, Kubernetes on VM runtimes (Kata) | the host mount shared by virtio-fs (virtiofsd or the VMM's own device) | the sharer's DAX window maps the host file |
   | Firecracker | **no folder sharing at all** — no virtio-fs (its README, read 2026-10-21) | only a file-backed block or pmem device; a volume must be presented as a disk-image file |

   **What the mount route costs, stated so it is weighed honestly:** every uncached operation crosses two
   filesystems on the host instead of one; DAX maps the host kernel's cached copy of a file rather than slates'
   own memory, so the host holds the data twice and fills its cache by one copy from slates; and a change slates
   makes on the host reaches the guest after two caches expire, not one. None of that is a correctness loss. It
   is measured against each consumer's bar (kiln's: a build in the volume at most 1.5x the host).

   **Proven so far (2026-10-21), on real mounts:** a shared map through a slates mount reads the volume, and a write
   through it, `msync`ed, reaches the volume — read back through slates itself, bypassing the mount — on Linux (FUSE,
   arm64 container) and macOS (NFS). A change made through any other path reaches the map **as memory alone on Linux**,
   which is what DAX through a mount needs; that took a fix, found by this proof
   (`docs/bugs/2026-10-21-a-fuse-mount-hears-of-another-paths-change-only-on-its-next-request.md`), and holds over a
   generated sequence of sixteen replacements: the map read the new bytes 52–193 µs after the writer's reply, median
   82 µs (51 replacements, arm64 container). That is *after* the reply, not before it: a reader of the mount in that
   window sees the old bytes, which AC-3.3 as written does not allow (open in the ledger, with what can and cannot be
   made to wait). With three writers at once — a shared map through one mount (`msync`ed), writes at offsets through a
   second mount of the same volume, and whole-file replacements through the SDK, 24 generated writes over four pages —
   the map, the second mount and slates' own read agree after every write, on Linux (the map as memory alone, so each
   FUSE mount is told of the other's writes) and on macOS (after a `stat`)
   (`a_shared_map_a_second_mount_and_the_sdk_agree_with_the_volume_through_a_generated_history`). That history
   `msync`s each map write before the next writer acts. Without the sync it first failed on both platforms: a store
   left dirty in a mount's map, which is what a guest's DAX stores are on the host, was written back over a later write
   through another path, and the other write's bytes on that page reverted, including bytes the map never touched.
   **Fixed on Linux (A-139, A-140):** a cache write-back is applied except over bytes another path changed after the
   kernel's copy of them was fetched. The history with the map's stores left unsynced for generated stretches now agrees
   at every sync point (`unsynced_map_stores_a_second_mount_and_the_sdk_agree_at_every_sync_point`). A long-lived map
   is judged too, which is what DAX keeps. The mount keeps each byte's state as it consumes the journal, so a map whose
   store outlived 2,000 unrelated writes, or one faulted in a page at a time past the journal's capacity, still syncs
   (`a_long_lived_maps_write_back_survives_unrelated_activity_past_the_journal`,
   `a_map_fetched_a_page_at_a_time_under_activity_keeps_its_write_backs`). **macOS stays open:** NFS v3 has no
   write-back mark and no callback, so the server cannot tell a stale page from a new write. That is Ada's call
   (`docs/bugs/2026-10-21-a-dirty-shared-map-page-overwrites-a-later-write-through-another-path.md`). **On macOS it does not and cannot, over NFS v3:** the client cannot be
   told of a change, and a memory-only reader refreshes only when something makes the client revalidate (seen both
   never in 8 s and at 0.88 s); a `stat` of the file — what a guest's attribute request becomes on the host — makes it
   follow at once. So DAX through a mount on macOS sees host-side changes on the guest's next attribute refresh, not
   sooner.

   **A real VM's DAX window over the mount, on Linux (2026-10-22, by hand on the box).** crosvm's own virtio-fs device
   shared a `slates mount` folder into a guest running the box's own kernel (7.2.9), mounted `dax=always`, with a second
   mount of the same volume on the host as the other path. The sharer runs unprivileged only with crosvm's sandbox on:
   with `--disable-sandbox`, crosvm jails the device by pivoting into the folder without a user namespace, which needs
   root (`jail/src/helpers.rs` `create_base_minijail`). What the run showed:
   - The guest read the host's file and wrote one the host read back. Its inode reported DAX (`STATX_ATTR_DAX`), and
     its shared map read the volume's bytes.
   - A write through the second mount reached the guest's map as memory alone, and the guest's synced stores reached
     the volume, each at its own page.
   - **Unsynced guest stores on a page another path then wrote:** with the other path writing half the page, each side
     kept its own half; writing the whole page, the later acknowledged write won. The guest's map showed the same before
     it synced. This is the revert bug A-139/A-140 fixed, now through a real guest.
   - **The daemon killed under the guest's live maps:** the anchor restarted it and both mounts were taken over. The
     guest's next sync succeeded, its first fault afterwards read the volume, and a later write through the second mount
     reached its map as memory alone.
   - **The VM killed with an unsynced store:** the host's page cache still held the dirty page, and it landed in the
     volume.

   Every page agreed at the end through the second mount and through slates itself. The run also found that every
   change dropped the whole file from the kernel's cache, and so from the guest's map: each one-byte change to a 128 MiB
   file made the guest's next scan take 356–382 ms against 0.2–0.3 ms warm. Fixed by A-146, which drops only the bytes a
   change touched.

   **The run is now a gated test** (`crates/cli/tests/guest_dax.rs`,
   `a_guest_maps_a_slates_mount_with_dax_and_holds_the_latest_write_to_every_byte`). The guest's `/init` is the static
   `slates-guest-probe` (`crates/guest-probe`), which answers a line protocol over the serial console. The test writes
   the initramfs into a second slates volume, so nothing touches disk. Over three fixed-seed files it runs a generated
   history: the guest's unsynced stores and its syncs, and another mount's writes, each waited for in the guest's map as
   memory alone. At each check, every byte of the volume, read through slates itself, and of the guest's map must hold
   the latest write to it from either side. Green 4 of 4 on the box, 3.0–4.0 s each. With the write-back rule disabled
   it fails: "the other path's write of 130 at [28033, 28033 + 4735) did not reach the guest's map at 28033:
   TimedOut { seen: 15 }", a guest byte having reverted it. It needs crosvm, a DAX-capable guest kernel and the probe
   named in the environment, and skips loudly without them, so CI's lanes skip it.

   Still owed: Shards' DAX window over the macOS mount, and Firecracker's disk-image form. crosvm has prior art for the
   latter: its experimental `--pmem-ext2` builds an ext2 file system in memory from a host directory and presents it to
   the guest on a pmem device (`crosvm run --help` and its `ext2` crate, read on the box 2026-10-22).

   **What has to be proven, per target, by real runs** (the goal's bar: adversarial and chaos testing at each
   step): the guest or container reads and writes the volume; DAX works where the platform has it, which needs a
   slates mount's files to accept a shared `mmap` from the sharer; changes on either side reach the other; a
   session's writes stay inside its own volume or clone; and nothing breaks when the daemon or the guest goes away.
   Firecracker needs a new piece — a volume, or a snapshot of one, presented as a disk-image file.

   **slates' direct connection** (the VM's virtio-fs device talking to slates itself, built in-process as
   `attach_vhost_user_device` and `shards_fs::serve`) is kept as a later optimization: it removes the host's
   filesystem layer and lets DAX map slates' own memory with no copy. It is not needed for any target to work.
   Two designs for it were written and withdrawn on 2026-10-21: handing slates a socket from the launcher (which
   had no answer on macOS), then a slates-owned listener (Ada asked why another socket was needed when NFS has
   one, and why a mount would not do — it does).

## 9. For the integrator (docs this branch does not touch)

`docs/wip/GAPS.md`, Bridges (4.6) row — append to the status cell:

> **The virtio-fs guest device is built and wired (2026-09-13, `crates/bridge-virtiofs`,
> `crates/server/src/virtiofs.rs`; GAP-A9-5's device half): a sans-io split virtqueue over a
> bounded guest-memory seam with every §4.6 A-9 check made before access (chain length, loops,
> indirect refused, overflow, buffers outside or straddling guest memory or aliasing the rings,
> readable-before-writable, derived byte caps), the FUSE-over-virtio cycle through the FUSE codec
> onto the shared `Bridge` (byte-identical with direct dispatch over 15 requests; the hiprio
> queue; INIT/DESTROY; DAX never advertised), admission that authenticates the consumer before any
> queue, mapping or tag with per-chain credits derived from the shard's admission limit and the
> §4.9 window and an owned terminal step (T-4.14: revoke with requests pending → refusal before
> access, references swept, credits whole), and the device loop as a perpetual task on the
> volume's owning shard over `slates-rt` woken by the seam's doorbell; the daemon attaches a guest
> device to a provisioned volume on its owner shard and a guest's file reads back over the NFS
> port byte for byte (`crates/server/tests/virtiofs.rs`). The `VmmSeam` models the in-process and
> inherited-descriptor forms; the in-process form is served, the inherited-descriptor binding is
> refused typed `AttachmentUnsupported{InheritedDescriptor, BindingNotBuilt}`. Owed: the real
> libkrun/vhost-user bindings, the guest form's durable attachment record and the transport report
> on the `attach`/`status` wire, a live guest for AC-9.7, and the OCI namespace handoff (the
> container half). Record: `docs/wip/virtiofs.md`. Codec siblings fixed on the way: `FUSE_DESTROY`
> now sweeps and `FUSE_BATCH_FORGET` is served (`docs/bugs/2026-09-13-fuse-destroy-and-batch-forget-unserved.md`).**

`docs/wip/GAPS.md`, GAP-A9-5 row — replace the "Gap and source finding" cell's text with:

> Device half **built** (2026-09-13): the owned FUSE-over-virtio device with device admission,
> per-attachment credits, refusal-before-access on malformed chains (T-4.14) and the owning-shard
> loop, wired into the daemon; DAX not advertised. Still open: a real VMM binding (libkrun
> in-process, vhost-user inherited descriptor — the seam is built, the bindings are not), the
> guest form on the wire (`AttachmentRecord`, `attach`/`status` capability report,
> `AttachmentUnsupported` as a `Refusal`), a live guest for AC-9.7, and the OCI namespace handoff;
> native macOS/Windows per the Bridges row.

and its "Closure gate" cell: `AC-4.11–4.12/T-4.13–4.14 (device leg proven with the simulated
driver; live guest and OCI leg open); Phase 4.`

`docs/wip/SLATES_DESIGN.md` §4.6, a status blockquote to add after the A-9 contract paragraphs
(before "Writeback and snapshot barrier"):

> **Status (virtio-fs, 2026-09-13).** The owned FUSE-over-virtio device exists
> (`crates/bridge-virtiofs`) and the daemon serves it on the volume's owning shard
> (`crates/server/src/virtiofs.rs`): the split virtqueue is walked sans-io over a bounded
> guest-memory seam with every check above made before any buffer access and a refusal faulting
> the queue; the request cycle gathers the chain into the FUSE codec's `dispatch` onto the shared
> `Bridge` and scatters the reply, byte-identical with direct dispatch; admission asks the seam for
> the consumer before it reads the queues, maps the memory or publishes the tag, reads the
> consumer's rights from the volume's access list only then, and charges every chain against
> credits derived from the shard's admission limit and the §4.9 window; revocation is refused
> before access and reclaimed under one owned terminal step; the loop is a task on `slates-rt`
> woken by the seam's doorbell. DAX is not advertised and a DAX request is refused
> `AttachmentUnsupported`. The seam models the in-process and inherited-descriptor forms; only the
> in-process form is served (the libkrun and vhost-user bindings are owed), the guest form is not
> yet on the `attach`/`status` wire, and the conformance evidence is the simulated guest driver
> until a live Linux guest runs (AC-9.7). Record: `docs/wip/virtiofs.md`.
