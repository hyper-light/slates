# An arm64 Linux host with KVM, on an Apple M-series Mac (2026-10-27)

The DAX matrix's arm64 guest cells (`virtiofs.md` §8: Firecracker and Shards on Linux arm64) need an arm64 Linux host
with KVM. Neither CI nor this kiln has one as a machine:

- **GitHub's `ubuntu-24.04-arm`** is a Hyper-V guest on Cobalt 100 (Neoverse-N2) that boots without hypervisor mode:
  `kvm [1]: HYP mode not available`, and no `/dev/kvm` (Linux 6.17.0-1022-azure, diagnostics run 40570705949).
- **Docker Desktop's VM** on this kiln's Macs has no `/dev/kvm` (`docker run --device /dev/kvm` refused, 2026-10-27).

Apple's Virtualization framework offers nested virtualization on M3 and later Macs since macOS 15
(`VZGenericPlatformConfiguration.isNestedVirtualizationSupported`, true on this Mac: Apple M5 Max, macOS 26.4.1). A
Linux guest with it on boots at EL2 and runs KVM, so it is a real arm64 KVM host: hardware virtualization, nested. This
file is the recipe. Nothing of it is in slates or its CI: a 67-line Swift launcher, an image built with Docker, and a
boot. Each run is a command on the record below.

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

