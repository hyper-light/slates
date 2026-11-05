#!/bin/bash
# The mount benchmark on macOS (T-4.8; step 2 of the VFS and DAX plan): slates' NFS mount against APFS, on a RAM disk
# (the design's baseline for a RAM-only file system, `hdiutil attach -nomount ram://`, no privilege) and on the boot
# disk (the file system a user's tree lives on). crates/cli/examples/mount_bench.rs times every call, quiet and then
# with every core loaded.
#
# Needs a release build. From the repository root:
#
#   cargo build --release --locked -p slates-cli --bin slates --example mount_bench
#   bash docs/wip/bench/fsbench/macos.sh
#
# SLATES and BENCH name other binaries. FILES and ROUNDS shrink the workload for a smoke run; recorded numbers use the
# program's defaults. The NFS mount is unmounted before its daemon stops: a daemon that dies under a kernel NFS mount
# has panicked a Mac (docs/bugs/2026-10-06-macos-nfs-client-panics-when-its-server-restarts.md).
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../../../.." && pwd)
SLATES=${SLATES:-$ROOT/target/release/slates}
BENCH=${BENCH:-$ROOT/target/release/examples/mount_bench}
# The RAM disk's 512-byte sectors: 1 GiB, many times what a round holds at once.
RAM_SECTORS=$((1 << 21))
W=${RUNNER_TEMP:-$ROOT/target}/fsbench-$$
INSTANCE=fsb-$$
ANCHOR=
DEVICE=
RAM=/Volumes/fsbram$$
mkdir -p "$W/apfs" "$W/slates"

cleanup() {
  set +e
  if mount | grep -q " on $W/slates "; then
    "$SLATES" --instance "$INSTANCE" unmount "$W/slates" || umount "$W/slates"
  fi
  if [ -n "$ANCHOR" ]; then kill "$ANCHOR" 2>/dev/null; wait "$ANCHOR" 2>/dev/null; fi
  if [ -n "$DEVICE" ]; then hdiutil detach "$DEVICE" > /dev/null; fi
  rm -f "$W/anchor.err"
  rmdir "$W/apfs" "$W/slates" "$W"
}
trap cleanup EXIT

DEVICE=$(hdiutil attach -nomount "ram://$RAM_SECTORS" | awk '{ print $1 }')
diskutil erasevolume APFS "fsbram$$" "$DEVICE" > /dev/null

"$SLATES" --instance "$INSTANCE" anchor 2> "$W/anchor.err" &
ANCHOR=$!
for _ in $(seq 1 600); do
  if "$SLATES" --instance "$INSTANCE" volume list > /dev/null 2>&1; then break; fi
  sleep 0.1
done
# A fresh instance may need its consensus configuration made before it creates; one already made says so.
"$SLATES" --instance "$INSTANCE" bootstrap root > /dev/null 2>&1 || true
# A bounded volume reserves its whole limit from its shard's share of the machine (about 1 GiB a shard on a
# 16 GiB, four-core runner); a round holds about 6.5 MB, so 256 MiB fits every runner with room to spare.
PLAIN=$("$SLATES" --instance "$INSTANCE" volume create fsb --bounded 256MiB | awk '$1 == "id:" { print $2 }')
[ -n "$PLAIN" ] || { echo "volume create printed no id for plain"; exit 1; }
"$SLATES" --instance "$INSTANCE" mount "$PLAIN" "$W/slates" > /dev/null

echo "# date $(date -u +%FT%TZ); macOS $(sw_vers -productVersion) ($(sw_vers -buildVersion)), $(uname -m); slates $(git -C "$ROOT" rev-parse --short HEAD 2> /dev/null || echo unknown), release"
"$BENCH" \
  --arm apfs-ram="$RAM" --arm apfs="$W/apfs" --arm slates="$W/slates" \
  ${FILES:+--files "$FILES"} ${ROUNDS:+--rounds "$ROUNDS"}
echo "# the daemon's counters after the run: NFS calls by procedure, FUSE and WinFsp events (status)"
"$SLATES" --instance "$INSTANCE" status | tr -d '\r' | grep -E "nfs_calls|fuse\.|winfsp\." || true
