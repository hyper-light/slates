# The live-guest image (AUD-29-68, AC-9.7; docs/bugs/2026-10-01-no-live-guest-had-run-the-workloads.md): the
# toolchain image plus QEMU, the distribution's kernel (virtio-fs, FUSE, virtio-pci and the serial console built
# in), a busybox initramfs whose /init mounts the slates tag, and the roster's tools. Built for the host's
# architecture (arm64 locally, amd64 on the CI runner). Ada authorized QEMU in CI on 2026-10-01.
FROM rust:1.98.0
RUN set -e; arch=$(dpkg --print-architecture); \
    case "$arch" in arm64) qemu=qemu-system-arm;; amd64) qemu=qemu-system-x86;; *) echo "no guest recipe for $arch"; exit 1;; esac; \
    apt-get update -qq && apt-get install -y -qq --no-install-recommends "$qemu" "linux-image-$arch" busybox-static \
      cpio gzip xz-utils kmod rsync ripgrep vim nodejs npm sqlite3 inotify-tools >/dev/null && rm -rf /var/lib/apt/lists/*
COPY init /guest/init
RUN set -e; K=$(ls /lib/modules); mkdir -p /guest/root/bin /guest/root/lib/9p; \
    for m in netfs 9pnet 9pnet_virtio 9p; do xz -dc "$(modinfo -k "$K" -n "$m")" > "/guest/root/lib/9p/$m.ko"; done; \
    cp /bin/busybox /guest/root/bin/busybox; cp /guest/init /guest/root/init; chmod 755 /guest/root/init; \
    (cd /guest/root && find . | cpio -o -H newc 2>/dev/null | gzip > /guest/initrd.gz); \
    cp /boot/vmlinuz-* /guest/vmlinuz; chmod -R a+rX /guest
# The conformance exercisers for the guest's fsx and fsstress legs (AUD-29-78): the harness's own pins and flags.
COPY build-exercisers.sh /guest/build-exercisers.sh
RUN sh /guest/build-exercisers.sh
