# The inherited-descriptor VMM binding was not built (AUD-29-68, the binding leg)

**Date:** 2026-10-01. **Audit:** AUD-29-68 ("Complete the owned in-process or inherited-descriptor binding and its
durable authority/lifetime records, then exercise a real guest. Do not introduce a disk socket, standalone runtime,
disk image or privilege as an implicit attachment dependency"). **Design:** §4.6 A-9, D-2, RQ-20;
`docs/wip/virtiofs.md` §8 item 1.

## Description

A request for the inherited-descriptor guest form was refused `AttachmentUnsupported{InheritedDescriptor,
BindingNotBuilt}`, and the in-process seam had only the tests' pipe-backed implementations. No VMM could attach this
export: no vhost-user back end existed, so QEMU, cloud-hypervisor or any vhost-user-fs front end had nothing to
speak to.

## Root cause

The `VmmSeam` trait modelled both forms (`docs/wip/virtiofs.md` §6), but only the simulated seams implemented it.
Guest memory had no implementation over a real shared mapping.

## Impact

Guests could not consume the volume through a real VMM. Nothing was served wrongly; the form was refused.

## Exact edits

- `crates/bridge-virtiofs/src/vhost_user.rs` (new, Linux): the vhost-user back end.
  - It adopts a connected stream socket (a `socketpair` end, never a socket on disk).
  - The consumer is the peer's `SO_PEERCRED` uid, read before any message.
  - It speaks the protocol's request set for a vhost-user-fs device:
    - features (`VIRTIO_F_VERSION_1`, protocol features) and protocol features (multiqueue, reply-ack);
    - the owner, the memory table, and per queue its size, addresses, base, kick, call, error descriptor and
      enable;
    - `GET_VRING_BASE`.
  - It maps each memory region through `slates_mem::SharedObject`. A region's object must be sealed against
    shrinking and already as long as the region, so no access can meet a truncated page. Regions must not overlap.
  - It translates ring addresses from the front end's address space to guest-physical ones.
  - It presents a `VmmSeam` whose doorbell is one epoll descriptor over the socket and every kick eventfd, and
    whose notification is the call eventfd. Its memory fences each ordering edge (AUD-29-72's owed native
    fences).
  - Everything not offered is a typed protocol refusal that ends the device: an unoffered feature, an unknown
    request, an oversized message, a queue past the device's, a kick without a descriptor, or memory replaced
    under configured rings.
  - A ring resumed at a non-zero index is refused at admission.
  - Stopping a ring answers its position (the used index, which equals the next available index because the
    device completes each request before taking the next) and ends the device through its terminal step.
- `crates/bridge-virtiofs/src/{admission,capability}.rs`: the inherited-descriptor form is admitted and reported
  supported on Linux, and stays `BindingNotBuilt` elsewhere (macOS guests use the in-process form).
- `crates/server/src/virtiofs.rs`:
  - `Daemon::attach_vhost_user_device(volume, tag, view, socket, on_end)` adopts the socket on the owning shard
    and negotiates within the daemon's failover budget (`slates_rt::futures::within`);
  - the device is then admitted and served by the same path as the in-process form, with the transport passed
    through;
  - a failed handshake is `GuestDeviceOutcome::HandshakeRefused`, with nothing admitted.

## Proof

Both binding tests are in `crates/server/tests/virtiofs.rs` and run on Linux (arm64, Docker rust:1.98.0, non-root).

- `a_vhost_user_front_end_drives_a_guest_whose_file_the_host_reads_back` drives a test front end that speaks the
  real protocol bytes over a socketpair:
  - it negotiates, sends a sealed memfd as the guest's RAM by `SCM_RIGHTS`, and configures both queues with
    eventfds, with every reply-ack reporting success;
  - the guest, writing its rings into a second mapping of that memfd, CREATEs, WRITEs and RELEASEs a file, each
    kicked and its interrupt awaited on the call eventfd;
  - `GET_VRING_BASE` answers 3, and the device ends `DoorbellHungUp` with its references swept;
  - the host reads the bytes back over NFS.
  - With the interrupt mutated out, it failed waiting for it (`an interrupt in time`).
- `a_vhost_user_front_end_that_offers_unsealed_memory_or_leaves_is_refused`:
  - an unsealed memory object is refused (its ack reports failure, `HandshakeRefused(Memory)`);
  - a front end that closes first ends `HandshakeRefused(Closed)`.
  - With the seal check mutated out, the unsealed table was accepted and the test failed.
- The suites now expect the form served on Linux and refused elsewhere (`the_host_report_names_where_the_binding_is_built`,
  the server's report and ring-refusal tests). Green on macOS and Linux, with clippy on both.

## Not done here (AUD-29-68 stays open)

- **A live guest.** The test front end is the oracle's other leg, not a VMM. QEMU's `vhost-user-fs-pci` with
  `-chardev socket,fd=N` speaks the same protocol over an inherited descriptor, and a run of it against this back
  end with a Linux guest that mounts the tag is the next leg. Locally it needs QEMU in a Linux container (software
  emulation; this Mac's Docker has no `/dev/kvm`). In CI it needs QEMU installed on the runner, which is non-Rust
  tooling in CI (banned item 13) and needs Ada's authorization.
- **The durable guest attachment record** (`Consumer::Guest`, a guest form), so a guest's view can advance and a
  restart can account for it.
- The request codes are recorded from QEMU's `docs/interop/vhost-user.rst` and are checked by the live run only
  when it happens.
