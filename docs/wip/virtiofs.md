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
   | bare metal | the host mount | slates' own: a map of a copy of the file, kept equal to it, handed to a program (A-171: sealed, proven on Linux; A-173: read alone over the socket route, proven on macOS; A-175: a read-only section handle duplicated into the program, proven on Windows), read alone or read and write, its stores compared back into the file at the program's sync (A-186, proven on all three) |
   | OCI (Docker, Kubernetes on runc) | the host mount bound into the container (A-28, proven 2026-10-19) | a process in the container maps the file, through the bound mount or by A-135's direct map: the DAX matrix below (A-164) |
   | Shards | the host mount shared with `VOLUME … FOR`; or the daemon's own stream, handed out by `slates shards` (A-161) | over the mount, Shards' DAX window (D189, `crates/vmm/src/devices/virtio/fs/mod.rs` on `gpu`, read 2026-10-21) maps the host file; over the stream, it maps a per-attachment copy of the file that slates keeps equal to the volume (A-163, a real guest 3 of 3 on this Mac, 2026-10-24) |
   | QEMU, crosvm, Kubernetes on VM runtimes (Kata) | the host mount shared by virtio-fs (virtiofsd or the VMM's own device) | the sharer's DAX window maps the host file |
   | Firecracker | **no folder sharing at all** — no virtio-fs (its README, read 2026-10-21) — so a volume reaches it as a disk-image file: `slates image build` writes a snapshot as ext2 into an image volume, the guest's drive is that file through a slates mount, and `slates image merge` reads the guest's image back into a clone (A-160; built and proven by a real Firecracker v1.17.0 run, 2026-10-24) | pmem maps the image file with DAX (A-165; proven by real runs on the box, 2026-10-24) |

   **The DAX matrix (A-164, 2026-10-24).** What DAX means in each environment is A-164's: a consumer reading and
   writing a file's bytes in place, through mapped memory, coherent with every other path. Each cell below is one of:
   - **proven:** the run or gated test is named;
   - **owed:** the goal that owns it is named (legend after the tables);
   - **impossible:** the platform's own source is cited (kiln research 01a17143717a7556a52f40f1d5a0e1b2);
   - **none:** the environment does not run on that platform.

   *Bare metal: a process on the host maps the file.*

   | platform | mount route (the host kernel's cache) | socket route (no copy per read, A-171) |
   |---|---|---|
   | Linux x86_64 | proven: `crates/cli/tests/cli.rs` `a_shared_map_through_a_mount_writes_back_and_follows_another_paths_change` and, read alone, `a_read_only_map_through_a_mount_follows_another_paths_change` (16 replacements followed as memory alone, fedora, 2026-10-25), and the generated histories of `crates/bridge-fuse/tests/coherence_mount.rs` (CI's Linux lane) | proven (A-171, 2026-10-25, fedora): `crates/client/tests/client.rs` asserts the zero-copy path for one chunk, many chunks and every size from empty to 1 MiB (`a_same_kernel_client_maps_a_file_with_no_copy`, `a_map_read_returns_the_whole_file_at_every_size`), judges the handed descriptor as a hostile peer would (`a_direct_maps_descriptor_reads_one_file_and_changes_nothing`) and sees every change within the map as memory alone (`a_direct_map_follows_every_change_within_its_length_as_memory_alone`), red-checked four ways. The copy is the file's own sealed memfd, so the escape of `docs/bugs/2026-10-18-a-read-handed-the-peer-the-whole-content-arena.md` cannot recur through it. Before A-171 this cell said "proven" while every read was a copy (corrected 2026-10-25). **Read and write too (A-186, 2026-10-28, adacachy, Linux 7.2.9):** a client maps a file read and write under its own write attachment, and its stores reach the file at its sync, its release and before its reap; the client binary's 8 writable-map tests (exact bytes written, another path's write kept, nothing past the end, a detach refused while held, the file followed across a rename, the authority check, a hostile peer's resize and seal refused, a store made during a sync never undone) and `reap_map`'s killed writer, red-checked eleven ways (A-186). |
   | Linux arm64 | proven (G-arm64-host, 2026-10-26): a process's read-only and shared maps through the FUSE mount follow another path's change (`a_read_only_map_through_a_mount_follows_another_paths_change`, `a_shared_map_through_a_mount_writes_back_and_follows_another_paths_change`) in CI's native arm64 lane, `ubuntu-24.04-arm`, run 40262946431. Red-checked on arm64 (2026-10-27, the same container as the next cell): with `FUSE_NOTIFY_INVAL_INODE` never sent both fail, the read-only map keeping its first fill through all 16 replacements | proven (G-arm64-host, 2026-10-26): every direct-map client test, 10 of 10, in the same lane: zero-copy at every size, following every change, the seal against a hostile peer, the hand-off and release cases. Red-checked on arm64 (2026-10-27, slates 7318e744, linux/arm64 natively in Docker Desktop on Apple silicon, Linux 6.12.76): green 10 of 10; with every map read by copy (`map_read` answering `MapUnavailable`) 9 of 10 fail their zero-copy assertions **Read and write too (A-186, 2026-10-28):** the 8 writable-map client tests in the same lane, all passing (159,669 stores over 200 syncs, none undone; run 40816824486). |
   | macOS arm64 | proven for read-only maps (G-mount step 1, 2026-10-25, this Mac): `crates/cli/tests/cli.rs` `a_read_only_map_through_a_mount_follows_another_paths_change` maps a file read alone through the NFS mount and follows 16 replacements made through another attachment, each after a `stat` (an NFS v3 client revalidates only when asked, and Apple's then takes the changed file's pages out of memory, `nfs_vinvalbuf2`'s `UBC_INVALIDATE`); red-checked with the server's file times frozen in a scratch copy (all 16 missed, the map still the first fill); CI's macOS lane runs it in the CLI flow. Writable shared maps: coherent at sync points (`a_shared_map_through_a_mount_writes_back_and_follows_another_paths_change` and its generated sequence pass in CI's macOS lane: a store reaches the volume at `msync`, another path's change reaches the map after a `stat`); a page dirtied and not synced while another path writes it is written back whole, stale bytes included (measured 2026-10-28, diagnostics run 40821794698: the other path's 16,384 bytes lost, 3 of 3), which Apple's client cannot avoid by either version (GAPS); the socket route's writable map keeps that case | proven (A-173, 2026-10-25, this Mac): the same tests over the daemon's socket route, where the client claims, with the reply's secret, a read-only descriptor of the file's copy (a shared-memory object whose name went at once); the zero-copy path asserted for one chunk, many chunks and every size from empty to 1 MiB, every change within the map seen as memory alone, and the handed descriptor judged as a hostile peer would (`a_direct_maps_descriptor_reads_one_file_and_changes_nothing`, macOS: read alone, exactly the mapped length, every way to write through it or resize it refused by XNU), red-checked with the read-write descriptor handed in its place. **Read and write too (A-186, 2026-10-28, this Mac):** the same writable-map tests and the killed writer over the socket route, the descriptor claimed read and write (XNU refuses its resize `EINVAL`). |
   | Windows x86_64 | impossible through WinFsp, measured (G-mount step 2, A-177): `slates mount VOLUME S:` serves the volume from the daemon and tells Windows of every other path's change by `FspFileSystemNotify`, so a cached read through a handle follows an SDK write in 20.7 ms (`crates/cli/tests/winfsp.rs` `a_cached_read_through_a_winfsp_mount_shows_another_paths_change`, red-checked with empty rounds: stale for 10 s, runs 40249528145 and 40250871529). But a mapped view (`CreateFileMappingW`, `MapViewOfFile`) keeps its pages: WinFsp purges with `CcPurgeCacheSection` (f8), which "will not purge mapped files" (f9), only `CcCoherencyFlushAndPurgeCache` invalidates user-mapped views (f9), so the view stays stale (f10). CI's Windows runner, Windows Server 2025, WinFsp 2.1.25156, run 40252840969: the view held its first byte for 10 s, and so did a read through another handle while it stood (GAPS) | proven (A-175, 2026-10-25, jupiter, Windows 10.0.26200): the same tests by handle duplication, where the daemon duplicates a read-only handle of the file's copy (a section whose security descriptor grants no one any right) into the client's process; the zero-copy path asserted for one chunk, many chunks and every size to 1 MiB, every change within the map seen as memory alone, and the handle judged as a hostile holder would use it (`a_direct_maps_handle_reads_one_file_and_changes_nothing`: a duplicate that writes, one that may rewrite the descriptor and a writable view refused `ERROR_ACCESS_DENIED`), red-checked with a section under the default descriptor; CI's two x86_64 Windows lanes run them. **Read and write too (A-186, 2026-10-28, jupiter):** the same writable-map tests, one at a time, with a read-write handle whose section's descriptor refuses any wider duplicate; red-checked three ways on jupiter (a wider handed handle and an unconfined section each refused at the hand-off; with the judge passed too, the hostile test catches the escalation). |
   | Windows arm64 | impossible through WinFsp, measured on arm64 too (2026-10-27): on GitHub's `windows-11-arm` (Windows 10.0.26200 on Cobalt 100, WinFsp 2.1.25156 with its arm64 driver and library, `winfsp-a64.sys` and `winfsp-a64.dll`), the same tests as on x86_64, diagnostics run 40570394717: a cached read through a handle followed an SDK write in 20.9 ms, while a mapped view held its first byte for 10 s, and so did a read through another handle while the view stood; red-checked in the same run with the mount handing Windows empty notification rounds (the cached read stale for 10 s). CI's `windows-arm64` lane runs them since | proven (2026-10-27): every direct-map client test, 20 of 20, in CI's `windows-arm64` lane on `windows-11-arm` (run 40525669353, slates 02562ac8): zero-copy at every size, every change within the map seen as memory alone, the handle judged as a hostile holder would use it. Red-checked on the same image (slates 7318e744, a diagnostics branch): green 20 of 20 and, with every map read by copy, 9 of 20 failing at their zero-copy assertions (run 40570140348); a section under the default descriptor was refused by the hand-off's own judge (`handle_route::confined_section`), so every map fell back to a copy and the same 9 failed (same run); with that judge passed too, only the hostile holder's test failed, its duplicate that writes granted (`Ok(())`, not `Err(5)`, `ERROR_ACCESS_DENIED`; run 40572762430) **Read and write too (A-186, 2026-10-28):** the 8 writable-map client tests and the writable section test on the same image, all passing (run 40816824486). |

   *VMs: the guest maps through its device's DAX window.*

   | VMM, platform | mount route | socket route (slates' own device) |
   |---|---|---|
   | crosvm, Linux x86_64 | proven: `crates/cli/tests/guest_dax.rs` (4 of 4 on the box, gated) | proven: `crates/cli/tests/guest_vhost.rs` (3 of 3 on the box, gated; G-vhost step 3) |
   | crosvm, Linux arm64 | impossible: crosvm offers DAX on x86_64 only (GAPS, 2026-10-22) | impossible: the same window |
   | QEMU, and Kata on QEMU, any platform | impossible: upstream `hw/virtio/vhost-user-fs.c` has no window property | impossible: the window is the front end's to offer |
   | Cloud Hypervisor, and Kata on it | impossible: "it is not available in Cloud Hypervisor" (its `docs/fs.md`) | impossible: the same |
   | Apple's Virtualization framework, macOS | impossible, measured (2026-10-27): a Linux guest of the framework's own virtio-fs device (`VZVirtioFileSystemDeviceConfiguration`, this Mac, macOS 26.4.1; Debian's 6.12.113 arm64 kernel, virtio-fs and FUSE DAX built in) logs `virtio_fs_setup_dax: No cache capability`: the device exposes no cache window, and `mount -t virtiofs -o dax=always` is refused "dax can't be enabled as filesystem device does not support it" (`arm64-kvm-on-a-mac.md`'s launcher with `VZRUN_SHARE`) | impossible: it offers no vhost-user device |
   | Hyper-V and WSL2, Windows | impossible, measured (2026-10-27): WSL 3.0.3.0 on CI's `windows-2025` (its kernel 6.18.54.1-microsoft-standard-WSL2) mounts Windows' drives over 9P, `C:\ /mnt/c 9p rw,…,aname=drvfs;path=C:\;…,trans=fd` (diagnostics run 40585743122): a message protocol over a Hyper-V socket, with no shared-memory window a guest could map a file through | impossible: no vhost-user device |

   *microVMs (Firecracker, which runs on Linux KVM only).*

   | platform | mount route (pmem over the image file, A-160) | socket route |
   |---|---|---|
   | Linux x86_64 | proven: `crates/cli/tests/guest_image.rs`, its three pmem tests (the box, gated; A-165) | impossible: Firecracker has no virtio-fs or vhost-user-fs device |
   | Linux arm64 | proven (G-arm64-guest, 2026-10-27): `crates/cli/tests/guest_image.rs`, 4 of 4 (its three pmem tests and the drive's), in 18.44 s, on an arm64 Linux host with KVM: a VM on Apple's Virtualization framework with nested virtualization on, on this Mac (Apple M5 Max, macOS 26.4.1; Debian's 6.12.113 arm64 kernel booted at EL2, "Hyp nVHE mode initialized"; Firecracker v1.17.0 for aarch64; the recipe is `arm64-kvm-on-a-mac.md`). The host's write showed in the guest's DAX map in 47 µs, and in 133 µs after the daemon was killed under the live map and taken over. Red-checked with the guest mounting `dax=never`: the three pmem tests fail at their own check, `Mapped { dax: false }: DAX is what this test is for`, the drive's passing. CI has no arm64 host with KVM: GitHub's `ubuntu-24.04-arm` boots without hypervisor mode (`kvm [1]: HYP mode not available`; diagnostics run 40570705949), and Docker Desktop's VM has no `/dev/kvm` | impossible: the same |
   | macOS, Windows | none: Firecracker runs on Linux KVM only | none |

   *Shards microVMs.*

   | platform | mount route (Shards' own window over a host mount) | socket route (the daemon's stream, A-161, A-163) |
   |---|---|---|
   | macOS arm64 | proven (G-shards-mount, 2026-10-26): a microVM under a Shards daemon gets a `slates mount` (NFS over loopback) as an ordinary volume with its own DAX window (`-v MOUNT:/v`, a 64 MiB share window, D189); the guest's kernel maps an 8 MiB file into the window (`dax /v/big yes`) and reads it whole through it, and a byte rewritten through another slates attachment reached a fresh open and read in the guest after 790.8, 24.4 and 728.1 ms, 3 of 3 on this Mac (HVF) against slates `b9014531`; red-checked with the window off (`dax /v/big no`). The test is Shards' `a_microvm_maps_a_slates_mount_file_through_its_own_dax_window` (`crates/shards/tests/slates_fs.rs`), a patch on Shards' `gpu` 3a659450 with its owner for review. Writable shared maps: as the bare-metal macOS mount cell, the window maps the host's NFS pages, coherent at sync points; a page dirtied and not synced while another path writes it is written back whole (measured 2026-10-28 on the host's own map, GAPS) | proven: Shards' `a_microvm_maps_a_slates_file_with_dax_and_each_side_sees_the_others_writes`, 3 of 3 twice (GAPS, 2026-10-24) |
   | Linux x86_64 | proven (G-shards-linux, 2026-10-26): a microVM under a Shards daemon gets a `slates mount` (FUSE) as an ordinary volume with its own DAX window (`-v MOUNT:/v`, D189); the guest maps an 8 MiB file into the window (`dax /v/big yes`) and reads it whole, and a byte rewritten through another slates attachment reached a fresh read in the guest after 29.0, 26.8 and 27.0 ms, 3 of 3 on adacachy (CachyOS, Linux 7.2.9, KVM) against slates `be7fa221`; red-checked with the window off (`dax /v/big no`). Shards' `a_microvm_maps_a_slates_mount_file_through_its_own_dax_window`, on Shards' `gpu` as `d717197a` | proven (G-shards-linux, 2026-10-26): Shards' `a_microvm_maps_a_slates_file_with_dax_and_each_side_sees_the_others_writes` over the daemon's stream (A-161, A-163), 3 of 3 on adacachy (`SHARDS-TEST PASS`; 28.7, 26.6 and 7.6 s); red-checked with the window off (`SHARDS-TEST FAIL /mnt/big is not DAX`) |
   | Linux arm64 | proven (G-arm64-guest, 2026-10-27): Shards' `a_microvm_maps_a_slates_mount_file_through_its_own_dax_window` on the microVM row's arm64 KVM host (nested on this Mac, `arm64-kvm-on-a-mac.md`): Shards' `gpu` 8597f56, its tests cross-built as static musl as its own `linux_tests()` builds them, its pinned `Image-6.18.48-aarch64` guest kernel (sha256 checked); slates 9799eb77. The guest maps `big` into Shards' own window over a `slates mount` (FUSE) (`dax /v/big yes`), reads it whole, and a fresh open and read saw the host's new byte after 19.99 ms. Red-checked with the share window off: "the guest maps the slates file into Shards' own window", `dax /v/big no` | proven (G-arm64-guest, 2026-10-27): Shards' `a_microvm_maps_a_slates_file_with_dax_and_each_side_sees_the_others_writes` over the daemon's stream on the same host (`SLATES-DAX STORED`, `SHARDS-TEST PASS`), with Shards' own window-off test passing beside it (`without_a_dax_window_the_guest_finds_no_dax_and_its_map_check_fails`, the red-check by construction); the suite 4 of 4 |
   | Windows x86_64 | impossible: on Windows the host mount is WinFsp's, and a mapped view of a WinFsp file keeps its pages after another path's change (A-177, measured on x86_64, run 40252840969, and on arm64, run 40570394717); a Shards window over the mount would map such a view. Shards also shares no host folder on Windows: its window serves a share another server serves (D38), and it has no share server there | proven (G-shards-windows, 2026-10-27): Shards' `a_microvm_maps_a_slates_file_with_dax_and_each_side_sees_the_others_writes` over the daemon's named-pipe stream (A-179, A-181) with Shards' WHP DAX window (its `gpu` bc032060: a view of slates' section placed in a placeholder per range, stage 2 mapped once and left standing; slates' sections, A-180): 3 of 3 in each of runs 40503725694, 40505101140 and 40521211082 on GitHub's windows-2025, slates f71282e1 (`slates-fs.yml`). Each guest maps `big`, stores through the map and fsyncs (slates' first read held the store 51 to 95 ms after the guest's word), sees another attachment's change in its same map, and reads zeros past the end of a cut. Red-checked with the window off in run 40521211082 (`SHARDS-TEST FAIL /mnt/big is not DAX`) |
   | Windows arm64 | none: Shards has no hypervisor backend on Windows arm64 (its `crates/vmm/build.rs` on `gpu` 930a1158 selects the Windows Hypervisor Platform for Windows x86_64 only, read 2026-10-27), and GitHub's `windows-11-arm` runner has the Windows Hypervisor Platform disabled (Hyper-V enabled; diagnostics run 40570705949) | none: the same |

   *Docker and OCI containers.*

   | platform | mount route (the host mount bound in) | socket route (A-135 from inside the container) |
   |---|---|---|
   | Linux x86_64 | proven (G-mount step 3, 2026-10-25): a program in the container maps a file read alone through the host's FUSE mount bound in and follows another path's replacement as memory alone. `crates/cli/tests/cli.rs` `a_container_maps_a_file_through_the_bound_mount_and_follows_another_paths_change` (Docker, the OCI attachment's own bind of a `--shared` mount; Docker 29.8.2 with runc inside a privileged Fedora 44 container, Linux 7.2.9, 1.01 s) and `a_rootless_container_maps_a_file_through_the_mount_and_follows_another_paths_change` (rootless podman, no `allow_other`; fedora, 0.99 s); each red-checked with `FUSE_NOTIFY_INVAL_INODE` never sent (the map kept the first fill); the probe is the test binary itself, run through its own loader in `alpine:3.20`; CI's Linux lane runs both | proven (G-container-direct, 2026-10-26): a process in a Docker container maps a slates file by the direct map with no copy: `crates/cli/tests/cli.rs` `a_container_maps_a_slates_file_by_the_direct_map_with_no_copy` runs the test binary in the container (its own loader and libraries bound read-only) as a client of the host's daemon, which `map_read`s a 4 MiB file: zero-copy, every byte, the daemon counting one hand-off (`dax.local_map`) and no fallback, 3 of 3 on adacachy (Docker 29.9.0, Linux 7.2.9); red-checked with the hand-off forced to a copy (`zero_copy=false`). The container shares the daemon's kernel and uid and the host's network namespace (`--network host`), since the rendezvous is an abstract socket, which Linux scopes to its network namespace (unix(7)) **Read and write too (A-186, 2026-10-28, adacachy):** `a_container_maps_a_slates_file_read_and_write_by_the_direct_map`: the container's process maps the file read and write under its own write attachment, stores 43 bytes across the first page's end, syncs and releases (`synced=43 released=0`), and the host reads the store in place, the daemon counting one hand-off and two write-backs; red-checked with the sync comparing nothing (`synced=0`). Both container tests run a one-shard daemon: a copy is handed only to a client of the volume's shard, and with two shards the container's client landed there or not by how many clients came before it. **On a bridge network (A-187, 2026-10-29):** `a_container_on_a_bridge_network_maps_a_slates_file_read_and_write_through_the_socket_file` in CI's Linux lane: the daemon at the socket file `/dev/shm/slates-a187-bridge-37476/cli-bridge-37476`, a container on Docker's bridge network with that directory bound in read-only writing through its map, `synced=43 released=0`, after its control found no daemon at an abstract instance (`direct-reach: unavailable`; run 40844136550, job 123347393500); red-checked on arm64, the arm64 row's. |
   | Linux arm64 | proven (G-arm64-host, 2026-10-26): Docker's bound FUSE mount and rootless podman's, each a program in the container whose map follows another path's change (`a_container_maps_a_file_through_the_bound_mount_and_follows_another_paths_change`, `a_rootless_container_maps_a_file_through_the_mount_and_follows_another_paths_change`), in CI's native arm64 lane (Docker 28.0.4 linux/arm64), run 40262946431 | proven (G-arm64-host, 2026-10-26): `a_container_maps_a_slates_file_by_the_direct_map_with_no_copy` in the same lane: `zero_copy=true whole=true len=4194304` **On a bridge network (A-187, 2026-10-29):** `a_container_on_a_bridge_network_maps_a_slates_file_read_and_write_through_the_socket_file` in the same lane: the daemon's instance is a socket file in a directory of the user's alone (`/dev/shm/slates-a187-bridge-5606/cli-bridge-5606`), bound read-only into a container on Docker's bridge network, whose process maps the file read and write: `synced=43 released=0`, the daemon counting one hand-off and two write-backs; its control, a container on a bridge network asking a daemon at an abstract instance, found none (`direct-reach: unavailable`; run 40844136550). Red-checked with the socket file read as an abstract name: the container's claim found no daemon (diagnostics run 40847130881). |
   | macOS arm64 (Docker Desktop) | impossible, measured (2026-10-27): Docker Desktop 29.3.1's VM mounts the Mac's folders over the framework's virtio-fs device (`virtiofs0 /run/host_virtiofs/Users`, no `dax` option, read from its first process's mount table) and binds them into containers through its own `fakeowner` layer (`/run/host_mark/Users /s fakeowner`); that device has no cache window (the VMs table's framework row) | impossible: the container runs in a VM, not on the daemon's kernel |
   | Windows (Docker Desktop on WSL2) | impossible: its containers run in WSL2's VM and reach Windows' folders through WSL2's drive mounts, which are 9P (the VMs table's WSL2 row, measured); Docker Desktop itself was not on the runner, so its own binding of them rests on its documentation, not a run | impossible: the container runs in a VM |
   | Windows process-isolated containers | impossible (G-mount step 4, 2026-10-26): Docker's engine refuses to bind a folder of the WinFsp drive into a process-isolated container — CI's Windows runner (Windows Server 2025, Docker 29.7.2, `nanoserver:ltsc2025`), run 40257213499: `invalid mount config for type "bind": bind source path does not exist: E:\share`, exit 125, while an NTFS folder bound into the same kind of container listed whole. The engine is a service in a logon session of its own, and "each logon session receives its own set of drive letters" (Microsoft Learn, "Services and Redirected Drives"), so it never sees the drive an unprivileged mount defines; a drive every session sees needs the file system run as LocalSystem, which slates' daemon never is (R10). Past that, Microsoft's container team says the bind filter refuses a custom file system's volume ("Do not attach the filter to the volume at this time", microsoft/Windows-Containers#335, closed by design). `crates/cli/tests/winfsp.rs` `a_winfsp_folder_bound_into_a_process_isolated_container` asserts the refusal and prints Windows' answer | proven with the daemon in the container (2026-10-27): on CI's `windows-2025` (Windows Server 2025, build 26100), `nanoserver:ltsc2025` run `--isolation=process` as `ContainerUser`, a daemon started in the container and two client processes beside it, one writing a 4 MiB file and one mapping it by the direct map (the read-only section handle duplicated into its process, A-175): `zero_copy=true whole=true len=4194304`, the daemon counting one hand-off (diagnostics run 40582122662); red-checked in the same run with every map read by copy (`zero_copy=false`). A daemon on the host cannot be reached from the container: the client's connect is refused "the rendezvous object is not there (no daemon has created it)", the container's server silo having an object namespace of its own, while the host's own client of that daemon maps zero-copy (same run). The binaries must link the C runtime statically (`+crt-static`): built as the workspace builds them they do not start in Nano Server, exit `0xC0000135` (`STATUS_DLL_NOT_FOUND`: no `vcruntime140.dll`), run 40580524733 **Read and write too (A-186, 2026-10-28):** the same container, a third client mapping the file read and write (a read-write section handle duplicated into its process), storing 37 bytes, syncing, releasing and reading it back: `differing=37 synced=37 released=0 whole=true`, the daemon counting two hand-offs, two write-backs and 37 bytes written (diagnostics run 40816955827, slates 4552995b); red-checked in the same run with a write-back comparing nothing (`synced=0 whole=false`). |

   *Kubernetes.*

   | runtime, platform | mount route | socket route |
   |---|---|---|
   | pods on runc, Linux x86_64 (the KIND lane) | proven (G-mount step 5, 2026-10-26): a pod maps a file read alone and shared through kubelet's mount of the export, NFS over RPC-with-TLS (the lane's PersistentVolume, `cargo xtask kind export --require-kernel-tls`), and follows another path's change after a stat and a read through its descriptor, 4.2 s after the change (adacachy, Linux 7.2.9, kind v0.33.0, Kubernetes 1.36.4; the mapper leg, `crates/guest-probe/src/pod.rs`). On Linux's NFS client a stat alone marks the cached pages invalid without dropping them (`nfs_getattr`), and the map follows once a read revalidates the mapping (`nfs_revalidate_mapping` runs only on a read, splice, mmap or lock); the probe prints both facts each run. Red-checked with the export answering a frozen change attribute for regular files: the map kept the old bytes for 120 s. CI's KIND lane runs it (run 40253147189: 3.2 s after the change) | proven (2026-10-27): CI's KIND lane runs the direct leg (`cargo xtask kind direct`, the arm64 row's leg) on `ubuntu-24.04`: `zero_copy=true whole=true len=4194304`, one hand-off, no fallback (run 40580068439, job 122456767232, slates 9799eb77). Its red-check is the arm64 row's, the same code. **A pod in a network namespace of its own (A-187, 2026-10-29):** the direct leg's step 6 in the same lane. A pod of its own on pod 0's node, as the daemon's user under the restricted profile's limits, first found no daemon at the abstract socket (`direct-reach: unavailable`, the pod at 10.244.1.3, pod 0 at 10.244.1.2); with the chart binding the rendezvous at a socket file in the node's directory, mounted read-only, pods of their own wrote the 4 MiB file and wrote through it by a writable direct map: `differing=37 synced=37 released=0 whole=true` (pods at 10.244.1.5 and 10.244.1.6, pod 0 at 10.244.1.4; run 40844136550, job 123347393577). Red-checked on arm64 **Read and write too (A-186, 2026-10-28):** the leg's third probe, in the same lane: `differing=37 synced=37 released=0 whole=true` (run 40816824486). |
   | pods on runc, Linux arm64 | proven (2026-10-27): the export leg (`cargo xtask kind export --require-kernel-tls`) on GitHub's `ubuntu-24.04-arm` (Linux 7.0.0-1017-azure, Docker 28.0.4 linux/arm64, kind 0.33): kubelet mounts the export over RPC-with-TLS, a second mount reads what the first wrote, and a pod's read-only map follows another path's change 3.00 s after it, after a stat and a read, as on x86_64 (diagnostics run 40580252064). Red-checked with the export answering a frozen change attribute for regular files: the map kept the old bytes for 120 s (run 40582373759). Docker Desktop's kernel lacks the TLS record layer (6.12.76-linuxkit: no `/sys/module/tls`), so the leg runs on the runner | proven (2026-10-27): `cargo xtask kind direct` (`xtask/src/kind_direct.rs`) on Docker Desktop on this Mac (linux/arm64, Linux 6.12.76-linuxkit; kind 0.33 with `kindest/node:v1.36.4`; the chart with one replica). In pod 0, one ephemeral container writes a 4 MiB file as one client and another maps it by the direct map (`kubectl debug --profile=restricted`: every capability dropped, no privilege escalation, the runtime's default seccomp, its own mount and pid namespaces; the pod's network namespace and uid). Result: `zero_copy=true whole=true len=4194304`, the daemon counting one hand-off, no fallback and nothing unconfined; the same on GitHub's `ubuntu-24.04-arm` (run 40580252064). Red-checked with every map read by copy (`map_read` answering `MapUnavailable`): `zero_copy=false`, no hand-off. Its first run found the daemon refusing every client it cannot see by pid (`docs/bugs/2026-10-27-a-peer-whose-process-the-daemon-cannot-see-was-refused-by-undefined-behaviour.md`). **A pod in a network namespace of its own (A-187, 2026-10-29):** the direct leg's step 6 on GitHub's `ubuntu-24.04-arm`, as the x86_64 row's: `direct-reach: unavailable` from a pod of its own at the abstract socket, then `wrote 4194304` and `differing=37 synced=37 released=0 whole=true` from pods of their own through the socket file (diagnostics run 40847130881, job 123357266951). Red-checked in the same run with the socket file read as an abstract name: steps 1 to 5 passed and step 6's pod of its own was refused, "daemon unavailable at /run/slates/default" (job 123357266949). A namespace enforcing Pod Security's restricted profile, which forbids hostPath, mounts the directory through a claim bound to an administrator's `local` PersistentVolume: the leg's step 7, built, its first run owed to CI **Read and write too (A-186, 2026-10-28):** the direct leg's third probe maps the file read and write in pod 0, stores 37 bytes across the first page's end, syncs, releases and reads the file back: `differing=37 synced=37 released=0 whole=true`, the daemon counting one more hand-off and two write-backs, none refused (GitHub's `ubuntu-24.04-arm`, diagnostics run 40816955827, slates 4552995b); red-checked in the same run with a write-back comparing nothing (`synced=0 whole=false`). |
   | Kata (VM pods), any platform | impossible: QEMU and Cloud Hypervisor offer no window (above) | impossible: the same |

   The goals, in kiln (agent 01a14885):
   - G-pmem `01a1714948f072bca869043c7b0e74f4`;
   - G-vhost `01a1714948f072bca869043d7693e23d`;
   - G-direct `01a1714948f072bca869043e164295f2`;
   - G-mount `01a1714948f072bca869043ffdf02ff9`;
   - G-shards-mount `01a1714948f072bca8690440193d7e16`;
   - G-shards-linux `01a1714e342e7243885a3dceaa6ed646`;
   - G-shards-windows `01a1714e342e7243885a3dcfa07cfe5e`;
   - G-container-direct `01a1714e342e7243885a3dd09bc0bb19`;
   - G-arm64-host `01a1714e342e7243885a3dd1d0da43a0`;
   - G-arm64-guest `01a16633113d7017b533c990c3b36511`;
   - G-nfs-writable `01a16633113d7017b533c99240a0981e`.

   A cell marked unconfirmed rests on documentation that says nothing, or on search summaries; it is re-judged the
   first time a primary source or a run says otherwise.

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

   Two lifecycle tests run beside it. A daemon killed under the guest's live map is taken over and the history goes on
   (`a_daemon_killed_under_a_guests_live_map_is_taken_over_and_the_guest_goes_on`); its first run found A-147, a store
   made after a re-fetch lost by the restored mount, red 3 of 3 before the fix. A guest killed with unsynced stores
   leaves them in the host's cache, where they are written back, and a second guest maps the same bytes
   (`a_guest_killed_with_unsynced_stores_leaves_them_to_land_and_a_new_guest_reads_them`).

   **A stated limit of DAX under KVM, measured 2026-10-22: a guest that touches the cut range of a file shrunk while
   it maps the file hangs, whatever directory is shared.** The guest probe mapped an 8-page file with DAX and read it
   whole. The file was then truncated to 2 pages. The guest still read the 2 pages, but its read of page 5 never
   returned. The result was the same in all three placements of the truncate:
   - through a second slates mount;
   - through the shared mount itself;
   - on a plain btrfs directory shared in slates' place, with no slates running.

   KVM's own counters (`/sys/kernel/debug/kvm/<pid>-<fd>/`) over 2 s of each hang:
   - faults taken: 56 227 to 65 582;
   - faults injected into the guest: 55 195 to 67 445;
   - faults fixed: 0 to 5;
   - the vCPU thread was running throughout.

   On the slates mounts, the FUSE connection had no request outstanding (`/sys/fs/fuse/connections/<n>/waiting` read
   0). The daemon used 220 ticks per 2 s during the hang, against 200 when idle before the guest started.

   The mechanism is KVM's. The host's fault of a page past the file's end fails. KVM faults asynchronously, so it
   ignores the failure, tells the guest the page is ready, and the guest retries forever. Vivek Goyal reported this
   very case, virtio-fs DAX under a host truncate, in "[RFC PATCH 0/4] kvm,x86,async_pf: Add capability to return page
   fault error" (LKML, 2020-03-31). His later "kvm,x86: Exit to user space in case page fault error" (v4, reviewed into
   October 2020) would have exited to the VMM instead of looping. On the box's 7.2.9 kernel the loop is unchanged.

   The guest cannot step around it. Its page tables still point into the window, so its access never asks its kernel
   for the size. crosvm's device (1c8d17a85) has nothing to tell it with: one high-priority queue plus the request
   queues, and no notification feature.

   slates cannot see a sharer's maps, and POSIX lets an open file be truncated, so slates refuses nothing here. On the
   mount route with DAX, a file a guest maps must not be shrunk through any other path while the guest may touch the
   cut range. A fix belongs to KVM (report the failed fault) or to a VMM that tells the guest.

   Reproduced by hand: the probe commands `map f 8`, `read 0 32768`, a host `truncate` to 8192, `read 0 8192`, then
   `read 20480 4096`, with the counters read twice 2 s apart while the last read hangs.

   **crosvm offers DAX on x86_64 only.** Its device enables the window under `cfg!(target_arch = "x86_64")`, with the
   TODO "Remove cfg! once DAX is supported on ARM" (b/176129399), read in `devices/device_virtio_fs/src/lib.rs` at
   1c8d17a85. So on an arm64 host the mount route under crosvm serves the guest through its own page cache, without
   DAX.

   **Firecracker's disk-image form (A-160; built and proven 2026-10-24).** crosvm's experimental `--pmem-ext2` is prior
   art: it builds an ext2 file system in memory from a host directory and presents it to the guest on a pmem device
   (`crosvm run --help` and its `ext2` crate, read on the box 2026-10-22). slates' form is a session like any other:
   - `slates image build ID SNAPSHOT NAME --capacity SIZE` makes a volume whose root holds `image.ext2`, the
     snapshot's tree as ext2 revision 1 (4 KiB blocks, 256-byte inodes, `filetype`, `sparse_super`, `large_file`). The
     image volume pins the snapshot.
   - The guest's drive is that file read through a slates mount (FUSE on Linux), writable, its flushes written through
     (`cache_type: Writeback`).
   - `slates image merge IMAGE NAME` reads the image at a snapshot of the image volume and makes a new clone of the
     build's snapshot hold its tree. A node the build wrote keeps its inode, so a renamed file is renamed and an
     unchanged file's chunks stay shared. The merge is not an inferred one (D-27): nothing else writes the clone.
   - Every unit of the build and the merge is bounded: none passed 18.5 µs on the box at 100,000 nodes or entries
     (BENCHMARKS, 2026-10-24).

   Proven by `crates/cli/tests/guest_image.rs` on the box (gated): Firecracker v1.17.0, the box's 7.2.9 kernel, an
   initramfs held in a slates volume, the guest reading every node as the volume holds it, then patching, renaming,
   removing, writing, making a directory and flushing; the merge held the clone to exactly the guest's tree. Its exit
   on the guest's restart is a race between the keyboard-controller reset and the kernel's triple-fault fallback, which
   the test takes in either form (GAPS). Shards' own DAX window over the macOS mount is proven since 2026-10-26 (the
   Shards rows above).

   **Firecracker's pmem form (A-165; built and proven 2026-10-24).** Firecracker (1.14 and later) can attach the same
   image file as a virtio-pmem device: it maps the file shared as a range of the guest's physical memory, and carries
   out a guest's flush as an `msync` of it. The guest mounts the device as ext4 with `dax=always`, after loading the
   nine modules a distribution kernel builds virtio-pmem and nd_pmem as, which the test carries in the initramfs.
   - What the three pmem tests in `crates/cli/tests/guest_image.rs` prove on the box:
     - the guest's unsynced store is in the image file, read through the slates mount;
     - a write through the mount shows in the guest's map in 8–13 µs;
     - the guest's sync reaches the volume;
     - a daemon killed under the live map is taken over with nothing lost;
     - a killed Firecracker's unsynced stores reach the volume. The merge refuses that image as `NotClean` until
       `e2fsck -fy` has checked it, whose one repair is a `lost+found` the merge then takes.
   - Every check is red-checked.

   **What has to be proven, per target, by real runs** (the goal's bar: adversarial and chaos testing at each
   step): the guest or container reads and writes the volume; DAX works where the platform has it, which needs a
   slates mount's files to accept a shared `mmap` from the sharer; changes on either side reach the other; a
   session's writes stay inside its own volume or clone; and nothing breaks when the daemon or the guest goes away.
   Firecracker's piece, a snapshot presented as a disk-image file and merged back, is built (above).

   **slates' direct connection** (the VM's virtio-fs device talking to slates itself: `attach_vhost_user_device`
   in-process, and since 2026-10-24 the daemon's Shards stream handed out by `slates shards`, A-161) removes the
   host's filesystem layer. In-process, DAX maps slates' own memory with no copy. Over the Shards stream it maps a copy
   of each file, one per attachment, that slates keeps equal to the volume by writing every change through before it
   returns (A-163), and since 2026-10-25 slates' own device does the same for an out-of-process VMM over vhost-user
   (A-168): a VM's window holds what it mapped for as long as it holds it, so nothing else can keep it right. A cut
   through another path zeroes the cut range in the copy, whose size never changes, so a guest touching it reads zeros
   rather than the hang the mount route meets below, as a real crosvm guest showed (`crates/cli/tests/guest_vhost.rs`,
   2026-10-25). A descriptor authorizes its whole object, so slates' own content, every volume's, cannot be handed
   to another process. It is not needed for any target to
   work: the mount serves every one. Two designs for it were written and withdrawn on 2026-10-21: handing slates a
   socket from the launcher (which had no answer on macOS), then a slates-owned listener (Ada asked why another
   socket was needed when NFS has one, and why a mount would not do — it does). **Superseded on 2026-10-22:** Ada's
   goal update asks for DAX on every platform and environment by both routes, a host mount with the host's DAX and
   the socket route (socket files allowed when what crosses them is encrypted end to end and at rest), so the
   direct connection is built beside the mount, not after it (A-161 and its Shards parts). A consumer may still
   choose either route.

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
