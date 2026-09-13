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
