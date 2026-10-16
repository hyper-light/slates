# crosvm virtio-fs DAX A/B harness

The end-to-end, live proof of slates' **message-based DAX binding** (§4.6, A-9, A-135): a real
[crosvm](https://crosvm.dev) guest mounts a slates volume over vhost-user virtio-fs `-o dax=always`, reads a file
through an `mmap`, and gets the exact bytes back — which means the guest access went through
`FUSE_SETUPMAPPING → VHOST_USER_BACKEND_SHMEM_MAP` and slates mapped the file's content block into the guest's
DAX window, not through `FUSE_READ`.

This is a **manually-run harness, not a CI test**. It needs a Linux host with KVM and a `crosvm` binary. On a
non-Linux host the example prints a skip and exits; `cargo clippy --all-targets` compiles it everywhere. The
in-tree hermetic proof of the same park/ack path, with a stand-in front end and no VM, is
`a_message_based_setupmapping_parks_until_the_front_end_acks_then_answers_the_guest` in
`crates/bridge-virtiofs/tests/device.rs` — run that for the unit-level guarantee; run this for the live one.

## The authorized R1 deviation

crosvm's `--vhost-user` accepts only `socket=PATH` — a *filesystem* unix socket it connects to — with no
fd-passing and no abstract socket. So this harness, and only this harness, binds an on-disk socket (R1 item #3),
authorized by Ada for the A/B on 2026-10-16. The hermetic daemon never does this: production hands the
vhost-user fd in over the control channel (like `--warm`). The deviation is confined to `main.rs`; `examples/` is
exempt from the `cargo xtask structural` wall, and a file-level `#![allow(...)]` covers the socket cleanup and
the harness `expect`s.

## Pieces

| File | Role |
|---|---|
| `main.rs` | The slates side: starts a single-shard daemon, provisions a 1 MiB volume, binds the socket, hands the accepted connection's fd to `Daemon::attach_vhost_user_device_dax` (DAX) or `attach_vhost_user_device` (baseline), serves until the guest device ends. |
| `guest_init.c` | The guest side: a static `/init` that mounts `slates` at `/mnt`, writes a 32 KiB file, then reads it back — by `mmap` under DAX, by `read` otherwise — and verifies the bytes. Linux-only C (uses the 5-arg Linux `mount(2)` and `RB_POWER_OFF`); a macOS editor will flag those, it is never built on macOS. |
| `build_initramfs.sh` | Builds a bare initramfs (static `/init` from `guest_init.c`, a console node) into a work dir. |

Only a **single-block unsealed file** is DAX-mappable today (`Body::Open`; `inline_bytes` ≈ 128 B `< size ≤
chunk_bytes` ≈ 64 KiB on a 4 KiB page — see `file_block` in `crates/vfs/src/dax.rs`). The 32 KiB guest file sits
in that band. An inline or multi-block file is refused by the DAX gate and the guest falls back to `FUSE_READ`.

## Recorded run

- **Hardware:** `adacachy`, a bare-metal x86_64 KVM box (host page 4 KiB), kernel `7.2.9-cachyos` with
  `VIRTIO_FS`, `FUSE_FS`, `FUSE_DAX`, `DAX`, `VIRTIO_PCI` all `=y`; `/dev/kvm` world-rw.
- **crosvm:** built from source (`~/crosvm/crosvm`).
- **Date:** 2026-10-16.

```sh
# 0. Build the initramfs (once; needs a static gcc on the Linux box). WORK is any scratch dir.
WORK=/path/to/scratch
bash crates/client/examples/crosvm_dax_ab/build_initramfs.sh "$WORK"   # -> $WORK/initramfs.cpio.gz

# 1. Build the harness (release).
cargo build --release -p slates-client --example crosvm_dax_ab

# 2a. DAX arm: start the backend asking for a DAX window, then connect crosvm.
./target/release/examples/crosvm_dax_ab "$WORK/sl.sock" dax &
#   wait for `HARNESS-LISTENING <path>` on stdout, then:
crosvm run \
  --vhost-user type=fs,socket="$WORK/sl.sock" \
  --vhost-user-connect-timeout-ms 60000 \
  --initrd "$WORK/initramfs.cpio.gz" \
  -p "console=ttyS0 init=/init slates_dax=1" \
  --serial type=stdout,hardware=serial,console,stdin \
  --disable-sandbox -m 512 \
  /path/to/vmlinux

# 2b. Baseline arm: same, but no `dax` arg to the harness and drop `slates_dax=1` from the kernel cmdline.
```

## What a pass looks like

- **DAX arm** — guest console: `GUEST-BOOTED`, `GUEST-DAX-MODE`, `GUEST-MOUNTED`, `GUEST-WROTE`, **`GUEST-MMAP-OK`**,
  `SLATES-GUEST-OK`. The `GUEST-MMAP-OK` (a `memcmp == 0` of the mmap'd window against the written bytes) is the
  binding working: the read came from the DAX window slates mapped, not from `FUSE_READ`.
- **Baseline arm** — `GUEST-PLAIN-MODE`, …, **`GUEST-READ-OK`**, `SLATES-GUEST-OK`. Same file, served over
  `FUSE_READ` — the A/B's control.
- **Harness (both arms)** — `HARNESS-LISTENING`, `HARNESS-ACCEPTED`, `HARNESS-ADMITTED`, `HARNESS-ATTACHED`,
  `HARNESS-ENDED Ended(...)` when the guest powers off.

Run each arm in its own shell — a sequential pair in one `ssh` heredoc loses the second VM's console output.
