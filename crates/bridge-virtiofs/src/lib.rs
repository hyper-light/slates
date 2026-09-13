//! `slates-bridge-virtiofs` — the virtio-fs guest device (§4.6 "virtio-fs and OCI attachment
//! contract (A-9)", D-2, RQ-20; Phase 4, GAP-A9-5). A Linux guest mounts the tag this device
//! publishes (`mount -t virtiofs <tag> /mnt`) and its FUSE requests arrive as descriptor chains on
//! virtqueues in guest memory; the device turns each into one call on the shared operation layer
//! ([`slates_bridge_core::Bridge`]) through the FUSE codec (`slates-bridge-fuse`, the Linux ABI the
//! guest kernel speaks), writes the reply back into the guest's buffers, and publishes the used
//! element. Host processes, OCI containers and microVM guests therefore consume one VFS through one
//! operation layer (RQ-20: "guests use virtio-fs"; D-2: "all forms share the same semantics,
//! authorization and accounting").
//!
//! The device is **owned and sans-io**: nothing here maps memory, opens a device or spawns a
//! thread. Guest memory is a seam ([`memory::GuestMemory`]: bounded, checked reads and writes of
//! guest-physical ranges), the host VMM is a seam (the admission module's `VmmSeam`: in-process, as
//! Hecate's libkrun integration is, or an inherited descriptor), and the runtime integration is a
//! perpetual task on the volume's owning shard over `slates-rt` (D-9: the only runtime). The
//! simulated implementations of both seams live here (the `SimHost` pattern of `crates/vfs/src/host`)
//! so the whole cycle — a simulated guest driver building real §2.7 rings, the walk, the FUSE
//! dispatch onto a real scratch volume, the scatter of the reply, the used-ring publication — is
//! proven by use on every host, with no VM.
//!
//! The contract this crate enforces (§4.6, A-9): device admission authenticates the consumer before
//! creating a queue, mapping guest memory or publishing a tag; queue descriptors, scatter/gather
//! ranges, arithmetic and chained lengths are validated within derived caps *before access*
//! ([`virtqueue`]); in-flight requests, copy buffers and replies consume the attachment's credits,
//! and cancellation and revocation reclaim them under one owned terminal step. DAX is **not
//! advertised**: the baseline contract does not require it, and "a requested DAX capability cannot
//! be advertised until mapping isolation, pinning and teardown have been established for that VMM"
//! — the capability report says so with that reason (AC-4.12 "Do not advertise DAX without this
//! gate").
//!
//! Evidence: virtio 1.2 §2.7 "Split Virtqueues" and §5.11 "File System Device" (the wire formats,
//! cited per constant as `Format:` lines); the Linux FUSE ABI (`include/uapi/linux/fuse.h`, spoken by
//! the codec); `research/hecate-contract-review.md` §2 (the in-process VMM seam); `research/
//! os-filesystem-bridge.md` (the FUSE cost model). The design record for this crate, its derived caps
//! and the measurements behind them is `docs/wip/virtiofs.md`.
//!
//! Modules: [`memory`] (the guest-memory seam and its typed refusals), [`sim`] (the simulated guest
//! memory the tests own), [`virtqueue`] (the split-virtqueue machine: layout validation, the chain
//! walk with every check the contract names, the used ring), [`device`] (the FUSE-over-virtio
//! request cycle: gather, dispatch through the FUSE codec onto the shared `Bridge`, scatter,
//! publish; the hiprio queue; INIT/DESTROY; the derived caps; DAX not advertised), [`credit`] (the
//! attachment's request and byte credits, derived from the shard's admission limit and the §4.9
//! credit window, charged per chain before access), [`admission`] (the `VmmSeam`, the ordered
//! admission that authenticates the consumer first, the admitted device's service pass, revocation
//! and the owned terminal step), [`capability`] (the truthful transport report `attach` and
//! `status` carry, DAX not advertised).

pub mod admission;
pub mod capability;
pub mod credit;
pub mod device;
pub mod memory;
pub mod sim;
pub mod virtqueue;
