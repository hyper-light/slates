# vhost-user: a message's descriptors were closed with an earlier message (found by the live QEMU guest)

**Date:** 2026-10-01. **Audit:** AUD-29-68. **Found by:** the first live run of QEMU 10.0.13's `vhost-user-fs-pci`
against the back end (`a_linux_guest_mounts_the_volume_through_qemu_over_vhost_user`).

## Description

QEMU refused to start the device:

```
qemu-system-aarch64: vhost_set_mem_table failed: Input/output error (5)
```

The back end ended the handshake with
`HandshakeRefused(Some(Protocol { reason: "a memory table whose descriptors do not match its regions" }))`.

## Root cause

`VhostUserSeam::receive` read as many bytes as the buffer held. QEMU sends `SET_PROTOCOL_FEATURES`, `SET_OWNER` and
`SET_MEM_TABLE` back to back without waiting, so one receive took several messages, and with them the memory
table's descriptor, which arrives with the table's first byte. The handler of the first message then closed the
descriptors it had not used (`handle` drops what a message carried), the table's descriptor among them. The test
front end waited for a reply after each step, so its messages were never read together and the tests passed.

## Impact

No real VMM could configure the device: every QEMU run failed at the memory table. Nothing was served wrongly.

## Exact edits

`crates/bridge-virtiofs/src/vhost_user.rs` `receive`: never reads past the current message. It takes the header
alone, then exactly the payload the header names, so a message's descriptors are received with it and with no
other.

## Proof

- `crates/server/tests/virtiofs.rs` `a_vhost_user_front_end_sending_back_to_back_has_each_descriptor_kept_with_its_message`
  queues the four messages before the daemon adopts the socket. It failed before the fix (ack 1) and passes after
  it (ack 0).
- `a_linux_guest_mounts_the_volume_through_qemu_over_vhost_user` (Linux arm64, QEMU 10.0.13 under software
  emulation, Debian kernel 6.12.111) now passes in 2.3 s. The guest mounts the tag, reads the host's file, writes
  its own, makes a directory, unmounts and powers off, and the host reads the guest's bytes over NFS. With the
  interrupt mutated out, the device faulted `NotifyRefused` and the guest hung until the 200 s bound.

## Siblings

The ipc rendezvous (`crates/ipc/src/rendezvous.rs`) receives one fixed-size handoff per connection, so it cannot read
two messages at once. No other receive in the tree takes descriptors over a stream.

## The live-guest recipe (local)

The image is `rust:1.98.0` plus Debian's `qemu-system-arm`, `busybox-static`, `linux-image-arm64` (6.12.111, with
virtio-fs, FUSE, virtio-pci and the PL011 console built in), `cpio` and `gzip`. Its initramfs holds `/bin/busybox`
and an `/init` that:

- mounts `proc`, `sysfs` and `devtmpfs`;
- runs `mount -t virtiofs slates /mnt`;
- prints `SLATES-HOST-SAYS: $(cat /mnt/from-host.txt)`;
- writes `/mnt/from-guest.txt` and makes `/mnt/guest-dir`;
- unmounts, prints `SLATES-GUEST-OK`, and runs `poweroff -f`.

The test runs as a non-root user with `SLATES_GUEST_QEMU=/usr/bin/qemu-system-aarch64`,
`SLATES_GUEST_KERNEL=/guest/vmlinuz` and `SLATES_GUEST_INITRD=/guest/initrd.gz`:

```
cargo test -p slates-server --test virtiofs a_linux_guest
```

QEMU's arguments are in `run_qemu`: `-machine virt,memory-backend=mem`, a 256M `memory-backend-memfd` with
`share=on`, `-nic none`, and `-device vhost-user-fs-pci,chardev=vfs,tag=slates`. Software emulation needs no
`/dev/kvm`.
