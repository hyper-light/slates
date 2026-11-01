# An arm64 Linux host with KVM, on an Apple M-series Mac (2026-10-27)

The DAX matrix's arm64 guest cells (`virtiofs.md` §8: Firecracker and Shards on Linux arm64) need an arm64 Linux host
with KVM. Neither CI nor this kiln has one as a machine:

- **GitHub's `ubuntu-24.04-arm`** is a Hyper-V guest on Cobalt 100 (Neoverse-N2) that boots without hypervisor mode:
  `kvm [1]: HYP mode not available`, and no `/dev/kvm` (Linux 6.17.0-1022-azure, diagnostics run 40570705949).
- **Docker Desktop's VM** on this kiln's Macs has no `/dev/kvm` (`docker run --device /dev/kvm` refused, 2026-10-27).

Apple's Virtualization framework offers nested virtualization on M3 and later Macs since macOS 15
(`VZGenericPlatformConfiguration.isNestedVirtualizationSupported`, true on this Mac: Apple M5 Max, macOS 26.4.1). A
Linux guest with it on boots at EL2 and runs KVM, so it is a real arm64 KVM host: hardware virtualization, nested. This
file is the recipe. Nothing of it is in slates or its CI's main workflows: a 67-line Swift launcher, an image built with
Docker (on this Mac until 2026-10-28; on CI's arm64 runner from a diagnostics branch since 2026-11-01, the last section),
and a boot. Each run is a command on the record below.

## What the VM showed

Debian trixie's arm64 kernel (6.12.113+deb13-arm64), 2 vCPUs, 2 GiB, a busybox initramfs:

```
CPU: All CPU(s) started at EL2
kvm [1]: nv: 554 coarse grained trap handlers
kvm [1]: IPA Size Limit: 40 bits
kvm [1]: GIC system register CPU interface enabled
kvm [1]: Hyp nVHE mode initialized successfully
/dev/kvm present (10, 232)
```

## The launcher (`vzrun`)

`vzrun KERNEL INITRD CMDLINE CPUS MEMORY_MIB`: one Linux VM, nested virtualization on, its console (`hvc0`) on stdin and
stdout; it exits when the guest stops. Built and signed with the virtualization entitlement, ad hoc:

```
swiftc -O vzrun.swift -o vzrun
codesign --force --sign - --entitlements vzrun.entitlements vzrun   # com.apple.security.virtualization = true
```

```swift
// A minimal Linux VM on Apple's Virtualization framework with nested virtualization on, so the guest boots at EL2 and
// has KVM: the arm64 Linux host the DAX matrix's arm64 guest cells need (G-arm64-guest), on an M3-or-later Mac.
// Test infrastructure outside the slates tree; not part of slates or its CI.
//
// vzrun KERNEL INITRD CMDLINE CPUS MEMORY_MIB
// The guest's console (hvc0) is this process's stdin and stdout; the process exits when the guest stops.
import Foundation
import Virtualization

let args = CommandLine.arguments
guard args.count == 6, let cpus = Int(args[4]), let mib = UInt64(args[5]) else {
  FileHandle.standardError.write("usage: vzrun KERNEL INITRD CMDLINE CPUS MEMORY_MIB\n".data(using: .utf8)!)
  exit(2)
}

guard VZGenericPlatformConfiguration.isNestedVirtualizationSupported else {
  FileHandle.standardError.write("vzrun: nested virtualization is not supported on this Mac\n".data(using: .utf8)!)
  exit(2)
}

let platform = VZGenericPlatformConfiguration()
platform.isNestedVirtualizationEnabled = true

let boot = VZLinuxBootLoader(kernelURL: URL(fileURLWithPath: args[1]))
boot.initialRamdiskURL = URL(fileURLWithPath: args[2])
boot.commandLine = args[3]

let console = VZVirtioConsoleDeviceSerialPortConfiguration()
console.attachment = VZFileHandleSerialPortAttachment(
  fileHandleForReading: FileHandle.standardInput, fileHandleForWriting: FileHandle.standardOutput)

let config = VZVirtualMachineConfiguration()
config.platform = platform
config.bootLoader = boot
config.cpuCount = cpus
config.memorySize = mib * 1024 * 1024
config.serialPorts = [console]
config.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]

do {
  try config.validate()
} catch {
  FileHandle.standardError.write("vzrun: invalid configuration: \(error)\n".data(using: .utf8)!)
  exit(2)
}

final class Watcher: NSObject, VZVirtualMachineDelegate {
  func guestDidStop(_ virtualMachine: VZVirtualMachine) {
    FileHandle.standardError.write("vzrun: the guest stopped\n".data(using: .utf8)!)
    exit(0)
  }
  func virtualMachine(_ virtualMachine: VZVirtualMachine, didStopWithError error: Error) {
    FileHandle.standardError.write("vzrun: the guest stopped with an error: \(error)\n".data(using: .utf8)!)
    exit(1)
  }
}

let vm = VZVirtualMachine(configuration: config)
let watcher = Watcher()
vm.delegate = watcher
vm.start { result in
  if case let .failure(error) = result {
    FileHandle.standardError.write("vzrun: start failed: \(error)\n".data(using: .utf8)!)
    exit(1)
  }
}
RunLoop.main.run()
```

## The host's image (the Firecracker image test)

Built natively on the Mac (Docker Desktop, linux/arm64) with the slates tree as the context and this folder as the
`l1` build context: `docker build --platform linux/arm64 --build-context l1=<this folder> -t slates-vz-l1 -f Dockerfile
<slates tree>`. Debian's arm64 `vmlinuz` is the uncompressed `Image` already, which both the Virtualization framework and
Firecracker boot. Firecracker v1.17.0 for aarch64 is checked against its release's sha256 (`e351ebe4…`).

```dockerfile
# The arm64 KVM host for the DAX matrix's arm64 guest cells (G-arm64-guest), run as a nested-virtualization VM on an
# Apple M-series Mac (vzrun). Test infrastructure outside the slates tree. Built from the slates tree as context.
FROM rust:1.98.1 AS build
WORKDIR /work
COPY . .
ENV RUSTUP_TOOLCHAIN=1.98.1 CARGO_BUILD_JOBS=8
RUN --mount=type=cache,id=vz-l1-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=vz-l1-target,target=/work/target,sharing=locked \
    cargo build --release --locked -p slates-cli --bin slates \
 && cargo test --release --locked -p slates-cli --test guest_image --no-run --message-format=json > /tmp/test.json \
 && T=$(grep -o '"executable":"[^"]*guest_image-[^"]*"' /tmp/test.json | head -1 | cut -d'"' -f4) \
 && echo "test binary $T" && mkdir -p /out/work/target/release /out/opt/slates \
 && cp target/release/slates /out/work/target/release/slates \
 && cp "$T" /out/opt/slates/guest_image \
 && rustup target add aarch64-unknown-linux-musl \
 && RUSTFLAGS="-C target-feature=+crt-static" cargo build --release --locked -p slates-guest-probe --bin slates-guest-probe --target aarch64-unknown-linux-musl \
 && cp target/aarch64-unknown-linux-musl/release/slates-guest-probe /out/opt/slates/slates-guest-probe

FROM debian:trixie-slim AS fetch
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends curl ca-certificates >/dev/null
WORKDIR /fc
RUN curl -fsSLO https://github.com/firecracker-microvm/firecracker/releases/download/v1.17.0/firecracker-v1.17.0-aarch64.tgz \
 && curl -fsSLO https://github.com/firecracker-microvm/firecracker/releases/download/v1.17.0/firecracker-v1.17.0-aarch64.tgz.sha256.txt \
 && cat firecracker-v1.17.0-aarch64.tgz.sha256.txt && sha256sum -c firecracker-v1.17.0-aarch64.tgz.sha256.txt \
 && tar -xzf firecracker-v1.17.0-aarch64.tgz && cp release-v1.17.0-aarch64/firecracker-v1.17.0-aarch64 /fc/firecracker

FROM debian:trixie-slim
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends linux-image-arm64 e2fsprogs fuse3 kmod \
      busybox-static procps coreutils >/dev/null 2>&1 && rm -rf /var/lib/apt/lists/*
COPY --from=build /out/ /
COPY --from=fetch /fc/firecracker /opt/firecracker/firecracker
COPY --from=l1 init /init
RUN chmod 755 /init
```

`/init`, the VM's first process (`l1.run=image` or `l1.run=shards` on the kernel's command line picks the suite):

```sh
#!/bin/sh
# The L1 VM's init: a real arm64 Linux with KVM (nested virtualization on an Apple M-series Mac). The kernel argument
# `l1.run=` picks what it runs: `image`, slates' Firecracker image test (the arm64 microVM cell), or `shards`, Shards'
# slates_fs tests (the arm64 Shards cells). Prints, then powers off.
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev
exec </dev/console >/dev/console 2>&1
mkdir -p /dev/pts /dev/shm
mount -t devpts devpts /dev/pts
mount -t tmpfs tmpfs /dev/shm
mount -t tmpfs tmpfs /run
mount -t tmpfs tmpfs /tmp
# The loopback interface, which a fresh network namespace starts with down (Shards' tests serve an image locally).
busybox ip link set lo up
K=$(ls /lib/modules)
RUN=$(sed -n 's/.*l1\.run=\([a-z]*\).*/\1/p' /proc/cmdline)
echo "L1: $(uname -a)"
echo "L1: $(dmesg | grep -m1 'started at EL')"
ls -l /dev/kvm /dev/fuse
export HOME=/root PATH=/usr/sbin:/usr/bin:/sbin:/bin SLATES_TEST_CLI=1
mkdir -p /root
cd /root
case "$RUN" in
  shards)
    export SLATES_BIN=/work/target/release/slates SHARDS_TEST_GUESTS=/opt/shards-guests SHARDS_REQUIRE_VMS=1
    T=$(cat /opt/shards-test-binary)
    echo "L1: shards test binary $T"
    "$T" --test-threads=1 --nocapture 2>&1
    echo "L1: test exit $?"
    ;;
  *)
    echo "L1: firecracker $(/opt/firecracker/firecracker --version | head -1)"
    export SLATES_GUEST_FIRECRACKER=/opt/firecracker/firecracker SLATES_GUEST_VMLINUX=/boot/vmlinuz-$K \
      SLATES_GUEST_PROBE=/opt/slates/slates-guest-probe SLATES_GUEST_MODULES=/lib/modules/$K
    /opt/slates/guest_image --test-threads=1 --nocapture 2>&1
    echo "L1: test exit $?"
    ;;
esac
busybox poweroff -f
```

The image is exported and made an initramfs. `docker export` writes `dev/console` as an empty regular file, which the
kernel then opens as init's console, so every line init prints goes into RAM: it is left out.

```
C=$(docker create --platform linux/arm64 slates-vz-l1); docker export $C > rootfs.tar; docker rm $C
bsdtar -cf - --format newc --exclude dev/console @rootfs.tar | gzip -1 > l1-initrd.gz
tar -xf rootfs.tar boot/vmlinuz-6.12.113+deb13-arm64          # the kernel the VM boots
vzrun vmlinuz-6.12.113+deb13-arm64 l1-initrd.gz "console=hvc0 rdinit=/init loglevel=4" 6 16384 < /dev/null
```

The Shards variant adds Shards' cross-built tests on top of that image, its `shards` and `shardsrc` build contexts being
the Shards clone's `target/` and the clone itself (a `.dockerignore` there leaving out `target/`). Shards' tests are built
on the Mac with Shards' own guest toolchain: `CC_aarch64_unknown_linux_musl=scripts/zig-cc`,
`AR_aarch64_unknown_linux_musl=scripts/zig-ar`, `ZIG_TARGET=aarch64-linux-musl`,
`CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld`, then `cargo build -p shards-testguest --profile guest --target
aarch64-unknown-linux-musl --target-dir target/guest` and, with `CARGO_PROFILE_TEST_RELEASE_DEBUG=false`, `cargo test -p
shards --profile test-release --no-run --message-format=json-render-diagnostics --target aarch64-unknown-linux-musl
--target-dir target/l1 --test slates_fs`, whose JSON names the paths to place.

```dockerfile
# The arm64 KVM host's Shards variant: Shards' slates_fs tests, cross-built as static musl on the Mac as Shards' own
# linux_tests() builds them, placed at the absolute paths their build baked in (CARGO_BIN_EXE_shards, OUT_DIR, the
# carried helpers), with the test guest under SHARDS_TEST_GUESTS and the pinned kernel in Shards' artifact cache.
FROM slates-vz-l1
COPY --from=shardsrc . /Users/adalundhe/.cache/slates-scratch/shards-arm64/
COPY --from=shards l1/aarch64-unknown-linux-musl/test-release/deps/slates_fs-3309d29f56a295f2 /Users/adalundhe/.cache/slates-scratch/shards-arm64/target/l1/aarch64-unknown-linux-musl/test-release/deps/slates_fs-3309d29f56a295f2
COPY --from=shards l1/aarch64-unknown-linux-musl/test-release/shards /Users/adalundhe/.cache/slates-scratch/shards-arm64/target/l1/aarch64-unknown-linux-musl/test-release/shards
COPY --from=shards l1/aarch64-unknown-linux-musl/test-release/build/shards-3e4a7afdb878dd44/out/ /Users/adalundhe/.cache/slates-scratch/shards-arm64/target/l1/aarch64-unknown-linux-musl/test-release/build/shards-3e4a7afdb878dd44/out/
COPY --from=shards guest/aarch64-unknown-linux-musl/guest/shards-testguest /opt/shards-guests/guest/shards-testguest
COPY --from=shards artifacts/Image-6.18.48-aarch64-e39058b2b627 /Users/adalundhe/.cache/slates-scratch/shards-arm64/target/artifacts/Image-6.18.48-aarch64-e39058b2b627
COPY --from=l1 init /init
RUN chmod 755 /init && echo /Users/adalundhe/.cache/slates-scratch/shards-arm64/target/l1/aarch64-unknown-linux-musl/test-release/deps/slates_fs-3309d29f56a295f2 > /opt/shards-test-binary
```

## Results

- **Firecracker's pmem DAX (2026-10-27, slates 9799eb77 with the test's module changes):** the image test 4 of 4 in
  18.44 s, the host's write in the guest's DAX map in 47 µs, and in 133 µs after a takeover under the live map.
  Red-checked with the guest mounting `dax=never` (its own image, built with a separate cargo cache, `Compiling
  slates-cli` seen): the three pmem tests fail at "Mapped { dax: false }: DAX is what this test is for", the drive's
  passes. `virtiofs.md` §8, the microVM row for Linux arm64.
- **Shards on Linux arm64 (2026-10-27, Shards `gpu` 8597f56, slates 9799eb77):** Shards' `slates_fs` suite, cross-built as
  static musl on the Mac as Shards' own `linux_tests()` builds it (zig for its C, `rust-lld`), placed in the image at the
  absolute paths its build baked in (`CARGO_BIN_EXE_shards`, the build script's `OUT_DIR` with `shards-init` and the
  carried helpers), its test guest under `SHARDS_TEST_GUESTS` and its pinned kernel in its artifact cache (`l1.run=shards`):
  4 of 4, both DAX routes, in 3.12 s. The mount route's own red-check, the share window off, fails at `dax /v/big no`.
  The first run failed one test at `shards pull` of an image the test serves locally: init had left the loopback
  interface down, which it now brings up.

## What the first runs found

- **The image test assumed a kernel that builds its drive and file system in.** CachyOS's does; Debian's arm64 kernel
  builds `virtio_blk`, `ext4` and the nvdimm stack as modules. The test now carries every module a form needs that
  `modules.builtin` does not list (`crates/cli/tests/guest_image.rs`, `PMEM_MODULES`, `DRIVE_MODULES`).
- **Soft dependencies.** ext4 and jbd2 ask the crypto API for `crc32c` when they load (`softdep ext4 pre:
  crypto-crc32c`), which `modprobe` honours and a guest without `modprobe` does not: "EXT4-fs (pmem0): Cannot load crc32c
  driver", all four tests failing. The test now carries a module's `modules.softdep` `pre:` modules too, resolved
  through `modules.alias` (`crypto-crc32c` is `crc32c_generic`).

## The framework's virtio-fs device has no DAX window (2026-10-27)

With `VZRUN_SHARE=DIR`, the launcher adds a `VZVirtioFileSystemDeviceConfiguration` sharing `DIR` read-only, tag
`share`. A busybox guest of Debian's arm64 kernel (virtio-fs and FUSE DAX built in) then logged, on this Mac:

```
virtiofs virtio1: virtio_fs_setup_dax: No cache capability
virtio-fs: dax can't be enabled as filesystem device does not support it.
mount -t virtiofs -o dax=always share /mnt   -> exit 255
mount -t virtiofs share /mnt                 -> 0, the Mac's file read whole
```

So no guest of Apple's framework maps a shared file through a window, and Docker Desktop's containers, whose VM shares
the Mac's folders through the same device (`virtiofs0 /run/host_virtiofs/Users`), cannot either (`virtiofs.md` §8).

## Built on CI's arm64 runner, with no Docker on the Mac (2026-11-01, Step 7 of the VFS and DAX plan)

Ada quit Docker Desktop on 2026-10-28, so the image above can no longer be built on this Mac. It is now built on CI's
native arm64 runner (`ubuntu-24.04-arm`), whose Docker is the runner's, from a diagnostics branch that carries the
Dockerfile, the init and the workflow below as `ci/s7-l1/` and `.github/workflows/diag-s7-l1.yml` (never main). The Mac
downloads the artifact, checks its digests against the run's log, and boots it:

```
gh run download <run> -n s7-l1 -D <dir>      # l1-initrd.gz and l1-kernel; shasum -a 256 each against the run's log
vzrun l1-kernel l1-initrd.gz "console=hvc0 rdinit=/init loglevel=4 l1.run=image" 6 16384 < /dev/null   # or vhost, red
```

What it adds to the image above: crosvm for arm64 at 1c8d17a85, the x86_64 box's, built with its own toolchain (1.88.0)
and without its default features (its jail's build script compiles its seccomp policies with `python3`); Cloud
Hypervisor v54.0's static aarch64 binary, its digest pinned; Firecracker v1.17.0 as before; guest_vhost's test binary
beside guest_image's; and copies of both built with their guests mounting `dax=never`, the red-checks. The initramfs is
made with the runner's own `cpio`, `dev/console` left out as before.

**crosvm cannot sandbox on an initramfs.** The first run failed each crosvm test before the guest booted:
`libminijail[1]: pivot_root: Invalid argument`, then "failed to create a PCI root hub". crosvm's sandbox pivots each
jailed device into `/var/empty`, and pivot_root(2) refuses when the current root is the kernel's rootfs ("EINVAL The
current root is on the rootfs (initial ramfs) mount"). The init now copies the tree onto a tmpfs and `switch_root`s into
it before anything runs; on the box crosvm runs on a real root and needs none of it.

```dockerfile
# The arm64 KVM host for the DAX matrix's Linux arm64 cells (Step 7 of the VFS and DAX plan), booted as a
# nested-virtualization VM on an Apple M-series Mac (vzrun, docs/wip/arm64-kvm-on-a-mac.md). Built on CI's native arm64
# runner from a diagnostics branch, never on the Mac, which runs no Docker. Built from the slates tree as context, with
# this folder as the `l1` build context.
FROM rust:1.98.1 AS build
WORKDIR /work
COPY . .
ENV RUSTUP_TOOLCHAIN=1.98.1 CARGO_BUILD_JOBS=4
RUN mkdir -p /out/work/target/release /out/opt/slates \
 && cargo build --release --locked -p slates-cli --bin slates \
 && cp target/release/slates /out/work/target/release/slates \
 && cargo test --release --locked -p slates-cli --test guest_image --test guest_vhost --no-run --message-format=json > /tmp/test.json \
 && for t in guest_image guest_vhost; do \
      T=$(grep -o "\"executable\":\"[^\"]*/${t}-[^\"]*\"" /tmp/test.json | head -1 | cut -d'"' -f4); \
      echo "test binary $t: $T"; cp "$T" "/out/opt/slates/$t"; \
    done \
 && sed -i 's/slates.dax=always slates.modules/slates.dax=never slates.modules/' crates/cli/tests/guest_image.rs \
 && grep -q 'slates.dax=never slates.modules' crates/cli/tests/guest_image.rs \
 && sed -i 's/^const GUEST_ARGUMENTS: &str = "rdinit=\/init quiet slates.dax=always";/const GUEST_ARGUMENTS: \&str = "rdinit=\/init quiet slates.dax=never";/' crates/cli/tests/guest_vhost.rs \
 && grep -q '^const GUEST_ARGUMENTS: &str = "rdinit=/init quiet slates.dax=never";' crates/cli/tests/guest_vhost.rs \
 && cargo test --release --locked -p slates-cli --test guest_image --test guest_vhost --no-run --message-format=json > /tmp/red.json \
 && for t in guest_image guest_vhost; do \
      T=$(grep -o "\"executable\":\"[^\"]*/${t}-[^\"]*\"" /tmp/red.json | head -1 | cut -d'"' -f4); \
      echo "red test binary $t: $T"; cp "$T" "/out/opt/slates/${t}_red"; \
    done \
 && rustup target add aarch64-unknown-linux-musl \
 && RUSTFLAGS="-C target-feature=+crt-static" cargo build --release --locked -p slates-guest-probe --bin slates-guest-probe --target aarch64-unknown-linux-musl \
 && cp target/aarch64-unknown-linux-musl/release/slates-guest-probe /out/opt/slates/slates-guest-probe

# crosvm at the commit the x86_64 box runs (1c8d17a85), with its own toolchain (its rust-toolchain: 1.88.0), built
# without its default features (gpu, audio, usb, net, wl): pmem, the vhost-user front end and the fs device are core.
FROM debian:trixie AS crosvm
# python3: crosvm's jail build script compiles its seccomp policies with minijail's compile_seccomp_policy.py.
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends ca-certificates curl git build-essential \
      pkg-config libcap-dev clang libclang-dev python3 >/dev/null
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain 1.88.0
ENV PATH=/root/.cargo/bin:$PATH
RUN git clone --filter=blob:none https://chromium.googlesource.com/crosvm/crosvm /crosvm \
 && cd /crosvm && git checkout 1c8d17a85 && git submodule update --init third_party/minijail
RUN cd /crosvm && cargo build --release --no-default-features && cp target/release/crosvm /crosvm-bin \
 && /crosvm-bin version

FROM debian:trixie-slim AS fetch
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends curl ca-certificates >/dev/null
WORKDIR /fetch
RUN curl -fsSLO https://github.com/firecracker-microvm/firecracker/releases/download/v1.17.0/firecracker-v1.17.0-aarch64.tgz \
 && echo "e351ebe4f7a16b5873bbd51005d2e6767103cff4d5ebc829df2d3f95a93e2256  firecracker-v1.17.0-aarch64.tgz" | sha256sum -c \
 && tar -xzf firecracker-v1.17.0-aarch64.tgz && cp release-v1.17.0-aarch64/firecracker-v1.17.0-aarch64 /fetch/firecracker \
 && curl -fsSLO https://github.com/cloud-hypervisor/cloud-hypervisor/releases/download/v54.0/cloud-hypervisor-static-aarch64 \
 && echo "e8a6d9786ea678e739f1a386682846e93742dcb9c7b6e3e0c62e3bda6cff0ef3  cloud-hypervisor-static-aarch64" | sha256sum -c \
 && chmod 755 cloud-hypervisor-static-aarch64

FROM debian:trixie-slim
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends linux-image-arm64 e2fsprogs fuse3 kmod \
      busybox-static procps coreutils libcap2 >/dev/null 2>&1 && rm -rf /var/lib/apt/lists/*
COPY --from=build /out/ /
COPY --from=crosvm /crosvm-bin /opt/crosvm/crosvm
COPY --from=fetch /fetch/firecracker /opt/firecracker/firecracker
COPY --from=fetch /fetch/cloud-hypervisor-static-aarch64 /opt/cloud-hypervisor/cloud-hypervisor
COPY --from=l1 init /init
# crosvm's sandbox pivots each jailed device into /var/empty, which a slim image lacks.
RUN chmod 755 /init && mkdir -p /var/empty
```

```sh
#!/bin/sh
# The L1 VM's init: a real arm64 Linux with KVM (nested virtualization on an Apple M-series Mac; Step 7 of the VFS and
# DAX plan). The kernel argument `l1.run=` picks what it runs:
# `image`, guest_image.rs with Firecracker, Cloud Hypervisor and crosvm; `vhost`, guest_vhost.rs with crosvm; `red`,
# guest_image.rs and guest_vhost.rs built with the guest mounting dax=never, guest_image's maps test once in each VMM
# alone and guest_vhost's map test in crosvm, each of which must fail at its DAX check. Prints, then powers off.
#
# First it moves itself off the initramfs: the kernel's rootfs cannot be pivoted away from (pivot_root(2): EINVAL "The
# current root is on the rootfs (initial ramfs) mount"), and crosvm's sandbox pivots each jailed device into /var/empty,
# so on rootfs crosvm fails ("libminijail[1]: pivot_root: Invalid argument"). The tree is copied onto a tmpfs, which
# switch_root makes the root, freeing the initramfs; the copy then runs this script again.
if [ ! -e /.l1-switched ]; then
  mkdir -p /newroot
  mount -t tmpfs tmpfs /newroot
  (cd / && tar --one-file-system -cf - .) | tar -C /newroot -xpf -
  touch /newroot/.l1-switched
  exec busybox switch_root /newroot /init
fi
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev
exec </dev/console >/dev/console 2>&1
mkdir -p /dev/pts /dev/shm
mount -t devpts devpts /dev/pts
mount -t tmpfs tmpfs /dev/shm
mount -t tmpfs tmpfs /run
mount -t tmpfs tmpfs /tmp
busybox ip link set lo up
K=$(ls /lib/modules)
RUN=$(sed -n 's/.*l1\.run=\([a-z]*\).*/\1/p' /proc/cmdline)
echo "L1: $(uname -a)"
echo "L1: $(dmesg | grep -m1 'started at EL')"
ls -l /dev/kvm /dev/fuse
export HOME=/root PATH=/usr/sbin:/usr/bin:/sbin:/bin SLATES_TEST_CLI=1
mkdir -p /root /work/target/tmp
cd /root
echo "L1: firecracker $(/opt/firecracker/firecracker --version | head -1)"
echo "L1: cloud-hypervisor $(/opt/cloud-hypervisor/cloud-hypervisor --version | head -1)"
echo "L1: crosvm $(/opt/crosvm/crosvm version | head -1)"
export SLATES_GUEST_PROBE=/opt/slates/slates-guest-probe SLATES_GUEST_MODULES=/lib/modules/$K
FC="SLATES_GUEST_FIRECRACKER=/opt/firecracker/firecracker SLATES_GUEST_VMLINUX=/boot/vmlinuz-$K"
CH="SLATES_GUEST_CLOUD_HYPERVISOR=/opt/cloud-hypervisor/cloud-hypervisor SLATES_GUEST_VMLINUX=/boot/vmlinuz-$K"
CROSVM="SLATES_GUEST_CROSVM=/opt/crosvm/crosvm SLATES_GUEST_KERNEL=/boot/vmlinuz-$K"
case "$RUN" in
  vhost)
    env $CROSVM /opt/slates/guest_vhost --test-threads=1 --nocapture 2>&1
    echo "L1: test exit $?"
    ;;
  red)
    for vmm in FC CH CROSVM; do
      eval "gate=\$$vmm"
      echo "L1: red-check in $vmm"
      env $gate /opt/slates/guest_image_red --test-threads=1 --nocapture \
        --exact a_guest_maps_a_volume_image_with_dax_through_virtio_pmem 2>&1
      echo "L1: red $vmm exit $?"
    done
    echo "L1: red-check of slates' own device in CROSVM"
    env $CROSVM /opt/slates/guest_vhost_red --test-threads=1 --nocapture \
      --exact a_guest_maps_a_session_file_through_slates_own_device_with_dax_and_sees_each_change_as_its_writer_is_answered 2>&1
    echo "L1: red vhost CROSVM exit $?"
    ;;
  *)
    env $FC $CH $CROSVM /opt/slates/guest_image --test-threads=1 --nocapture 2>&1
    echo "L1: test exit $?"
    ;;
esac
busybox poweroff -f
```

```yaml
name: diag-s7-l1
# Diagnostics for Step 7 of the VFS and DAX plan, never main: the nested arm64 KVM host's initramfs and kernel, built on
# CI's native arm64 runner (Docker is the runner's, never the Mac's), as an artifact the Mac boots under its nested
# virtualization (vzrun, docs/wip/arm64-kvm-on-a-mac.md).
on:
  push:
    branches: ['diag-s7-l1-*']
jobs:
  l1:
    name: the nested arm64 KVM host's initramfs (Step 7)
    runs-on: ubuntu-24.04-arm
    timeout-minutes: 150
    steps:
      - uses: actions/checkout@v4
      - name: Build the L1 image
        run: docker build --build-context l1=ci/s7-l1 -t slates-l1 -f ci/s7-l1/Dockerfile .
      - name: Make the initramfs and take the kernel
        shell: bash
        run: |
          set -euo pipefail
          C=$(docker create slates-l1); docker export "$C" > rootfs.tar; docker rm "$C"
          mkdir rootfs
          # `docker export` writes dev/console as an empty regular file, which the kernel would open as init's console.
          sudo tar -xf rootfs.tar -C rootfs --exclude=dev/console
          K=$(cd rootfs && ls boot/vmlinuz-* | head -1); cp "rootfs/$K" l1-kernel
          (cd rootfs && sudo find . -print0 | sudo cpio --null -o -H newc --quiet) | gzip -1 > l1-initrd.gz
          ls -la l1-initrd.gz l1-kernel; sha256sum l1-initrd.gz l1-kernel; echo "kernel $K"
      - uses: actions/upload-artifact@v4
        with:
          name: s7-l1
          path: |
            l1-initrd.gz
            l1-kernel
          retention-days: 7
```

**Results** (2026-11-01; diagnostics runs 41318050347 and 41319438579, each artifact's digests checked on the Mac):
- `guest_image.rs` 7 of 7 in each of three runs (172.27, 156.38 and 173.35 s), Firecracker, Cloud Hypervisor and crosvm
  each booting 13 guests a run (nested, Firecracker in 2.6 to 3.4 s, Cloud Hypervisor in 2.7 to 3.3 s, crosvm in 3.8 to
  6.3 s): the pmem tests over the image file through a slates mount and over a copy `slates pmem` hands the VMM, the
  drive test, and the three hostile guests. Each run's six takeovers (three VMMs, two backings) went to a new daemon,
  the host's next write showing in the guest's map in 0 to 307 µs, and each kill mid-sweep left an exact boundary
  (sweeps 149 to 244), arm64's store order notwithstanding.
- `guest_vhost.rs` 8 of 8 in crosvm, twice (141.63 and 145.08 s): slates' own device's window on arm64, six daemon kills
  under a guest's write each taken over with the map following in 38 to 126 µs, and twelve guest kills.
- The red-checks: guest_image's maps test, with its guest mounting `dax=never`, fails at its DAX check (`Mapped { dax:
  false }: DAX is what this test is for`) in Firecracker, Cloud Hypervisor and crosvm alone, and guest_vhost's map test
  fails at its own (`the map is not DAX`) in crosvm.
