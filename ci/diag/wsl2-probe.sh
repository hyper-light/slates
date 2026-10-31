#!/bin/sh
# DIAG, not for main: how WSL2 shares Windows' drives once `.wslconfig` asks for virtio-fs, and whether a DAX window is
# offered. Run inside the distribution as root by the diagnostics workflow; a script file, since PowerShell rewrote the
# inline form's quoting and nothing of it ran (diagnostics run 41133554543).
echo "--- uname"; uname -a
echo "--- mounts"; grep -E ' /mnt/[a-z] | 9p | virtiofs | drvfs ' /proc/mounts
echo "--- filesystems"; grep -i -E 'virtiofs|9p|fuse' /proc/filesystems
echo "--- kernel"; dmesg 2>/dev/null | grep -i -E 'virtio_fs|virtiofs|dax|cache cap|9p' | head -40
echo "--- iomem"; grep -i -E 'dax|virtio' /proc/iomem 2>/dev/null | head -20
echo "--- virtio devices"
for d in /sys/bus/virtio/devices/*; do
  [ -e "$d" ] || continue
  echo "$d device=$(cat "$d/device" 2>/dev/null) driver=$(basename "$(readlink "$d/driver" 2>/dev/null)" 2>/dev/null)"
done
echo "--- config"; zcat /proc/config.gz 2>/dev/null | grep -E 'CONFIG_FUSE_DAX|CONFIG_VIRTIO_FS|CONFIG_FS_DAX|CONFIG_DAX='
echo "--- a virtio-fs mount with dax=always"
tag=$(awk '$3=="virtiofs"{print $1; exit}' /proc/mounts)
echo "tag=$tag"
if [ -n "$tag" ]; then
  mkdir -p /mnt/daxtest
  mount -t virtiofs "$tag" /mnt/daxtest -o dax=always
  echo "mount-exit=$?"
  grep daxtest /proc/mounts
  dmesg 2>/dev/null | tail -5
else
  echo "no virtiofs mount found"
fi
