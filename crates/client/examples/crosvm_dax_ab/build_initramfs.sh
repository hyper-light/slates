#!/bin/bash
# Builds a minimal initramfs for the crosvm virtio-fs DAX A/B (sibling main.rs + guest_init.c): a single static
# /init (no busybox, no dynamic loader) plus the mount points and a console device node. Linux + a static gcc.
#
# Usage: build_initramfs.sh <work_dir>
#   <work_dir> holds the build tree and the output initramfs.cpio.gz. guest_init.c is read from this script's
#   own directory, so the harness is reproducible from any checkout (no hardcoded paths).
set -e
HERE="$(cd "$(dirname "$0")" && pwd)"
OUT="${1:?usage: build_initramfs.sh <work_dir>}"
BUILD="$OUT/initramfs"
rm -rf "$BUILD"
mkdir -p "$BUILD/proc" "$BUILD/sys" "$BUILD/mnt" "$BUILD/dev"
# static init: no .so, no interpreter -> execs in the bare initramfs with no loader surface.
gcc -static -O2 -o "$BUILD/init" "$HERE/guest_init.c"
echo "init is: $(file "$BUILD/init" | cut -d: -f2-)"
# console (c 5 1) + null (c 1 3) so the kernel can wire init's stdio immediately. mknod needs privilege; the
# `sudo -n` path is for an unprivileged shell on a box that grants it, falling back to a plain mknod when root.
sudo -n mknod "$BUILD/dev/console" c 5 1 2>/dev/null || mknod "$BUILD/dev/console" c 5 1
sudo -n mknod "$BUILD/dev/null" c 1 3 2>/dev/null || true
sudo -n chmod 0666 "$BUILD/dev/console" "$BUILD/dev/null" 2>/dev/null || true
( cd "$BUILD" && find . -print0 | cpio --null -o --format=newc 2>/dev/null | gzip -9 > "$OUT/initramfs.cpio.gz" )
echo "built: $(ls -la "$OUT/initramfs.cpio.gz")"
