#!/bin/bash
# The condition-12 three-way comparison, inside a Linux container: the kernel's overlayfs, a tmpfs, and slates
# over FUSE, each given the same workload over the same 500-file base, in one run on one host so the arms are
# comparable. Prints one line per arm for creates and one per arm for create+fsync.
#
# Needs: a Linux release build at /target/release/slates, /c = docs/wip/bench/fuse (for creates.py), /f = this
# directory, /out writable. Run it through run.sh rather than by hand.
set -u
: "${CREATES:=2000}" "${FSYNCS:=1000}" "${BASE_FILES:=500}"
apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq fuse3 python3 >/dev/null 2>&1 || { echo APT-FAILED; exit 1; }
cp /target/release/slates /usr/local/bin/slates
mkdir -p /base /mnt/v /ram /ovl
mount -t tmpfs tmpfs /ram
mount -t tmpfs tmpfs /ovl
mkdir -p /ovl/lower /ovl/upper /ovl/work /ovl/merged
for i in $(seq 1 "$BASE_FILES"); do echo "base $i" > "/ovl/lower/b$i"; done
cp -r /ovl/lower/. /base/
mount -t overlay overlay -o lowerdir=/ovl/lower,upperdir=/ovl/upper,workdir=/ovl/work /ovl/merged || echo "overlayfs mount FAILED"
slates --instance fsb anchor --quick --shards 2 >/out/anchor.out 2>/out/anchor.err &
for _ in $(seq 1 300); do slates --instance fsb volume list >/dev/null 2>&1 && break; sleep 0.1; done
slates --instance fsb bootstrap root >/dev/null 2>&1
ID=$(slates --instance fsb volume create fsb --dynamic 1GiB --base /base 2>&1 | awk '/^id/{print $2}')
# Unmount the FUSE mount lazily and the overlay forcibly on every exit: a left-behind mount wedges the VM.
trap "fusermount3 -u -z /mnt/v 2>/dev/null; umount -f -l /ovl/merged 2>/dev/null" EXIT
slates --instance fsb mount "$ID" /mnt/v --shared >/out/mount.out 2>&1 || { echo "slates mount failed:"; cat /out/mount.out; exit 1; }
echo "host: $(uname -m) $(uname -r); load $(cut -d' ' -f1-3 /proc/loadavg)"
echo "base entries through the mount: $(ls /mnt/v | wc -l)"
echo "creates ($CREATES) --"
echo "  tmpfs:     $(python3 /c/creates.py /ram/a "$CREATES")"
echo "  overlayfs: $(python3 /c/creates.py /ovl/merged/a "$CREATES")"
echo "  slates:    $(python3 /c/creates.py /mnt/v/a "$CREATES")"
echo "create+fsync ($FSYNCS) --"
echo "  tmpfs:     $(python3 /f/fsync.py /ram/f "$FSYNCS")"
echo "  overlayfs: $(python3 /f/fsync.py /ovl/merged/f "$FSYNCS")"
echo "  slates:    $(python3 /f/fsync.py /mnt/v/f "$FSYNCS")"
echo "load at end: $(cut -d' ' -f1-3 /proc/loadavg)"
