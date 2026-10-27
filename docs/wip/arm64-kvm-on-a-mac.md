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

`/init`, the VM's first process:

```sh
#!/bin/sh
# The L1 VM's init: a real arm64 Linux with KVM (nested virtualization on an Apple M-series Mac), running slates'
# Firecracker image test, whose pmem tests are the DAX matrix's arm64 microVM cell. Prints, then powers off.
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev
exec </dev/console >/dev/console 2>&1
mkdir -p /dev/pts /dev/shm
mount -t devpts devpts /dev/pts
mount -t tmpfs tmpfs /dev/shm
mount -t tmpfs tmpfs /run
mount -t tmpfs tmpfs /tmp
K=$(ls /lib/modules)
echo "L1: $(uname -a)"
echo "L1: $(dmesg | grep -m1 'started at EL')"
ls -l /dev/kvm /dev/fuse
echo "L1: firecracker $(/opt/firecracker/firecracker --version | head -1)"
export HOME=/root PATH=/usr/sbin:/usr/bin:/sbin:/bin SLATES_TEST_CLI=1 \
  SLATES_GUEST_FIRECRACKER=/opt/firecracker/firecracker SLATES_GUEST_VMLINUX=/boot/vmlinuz-$K \
  SLATES_GUEST_PROBE=/opt/slates/slates-guest-probe SLATES_GUEST_MODULES=/lib/modules/$K
mkdir -p /root
cd /root
/opt/slates/guest_image --test-threads=1 --nocapture 2>&1
echo "L1: test exit $?"
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

## Results

- **Firecracker's pmem DAX (2026-10-27, slates 9799eb77 with the test's module changes):** the image test 4 of 4 in
  18.44 s, the host's write in the guest's DAX map in 47 µs, and in 133 µs after a takeover under the live map.
  Red-checked with the guest mounting `dax=never` (its own image, built with a separate cargo cache, `Compiling
  slates-cli` seen): the three pmem tests fail at "Mapped { dax: false }: DAX is what this test is for", the drive's
  passes. `virtiofs.md` §8, the microVM row for Linux arm64.

## What the first runs found

- **The image test assumed a kernel that builds its drive and file system in.** CachyOS's does; Debian's arm64 kernel
  builds `virtio_blk`, `ext4` and the nvdimm stack as modules. The test now carries every module a form needs that
  `modules.builtin` does not list (`crates/cli/tests/guest_image.rs`, `PMEM_MODULES`, `DRIVE_MODULES`).
- **Soft dependencies.** ext4 and jbd2 ask the crypto API for `crc32c` when they load (`softdep ext4 pre:
  crypto-crc32c`), which `modprobe` honours and a guest without `modprobe` does not: "EXT4-fs (pmem0): Cannot load crc32c
  driver", all four tests failing. The test now carries a module's `modules.softdep` `pre:` modules too, resolved
  through `modules.alias` (`crypto-crc32c` is `crc32c_generic`).
