#!/bin/bash
# The mount benchmark on Linux (T-4.8; step 2 of the VFS and DAX plan): slates' FUSE mount against tmpfs and the
# kernel's overlayfs, every arm on RAM. A plain volume stands beside tmpfs; a volume over a 500-file base stands beside
# an overlay whose lower directory is the same base. crates/cli/examples/mount_bench.rs times every call, quiet and
# then with every core loaded.
#
# Needs a release build, fuse3, and root for the tmpfs and overlay mounts (sudo, or root in a privileged container:
# run.sh). From the repository root:
#
#   cargo build --release --locked -p slates-cli --bin slates --example mount_bench
#   bash docs/wip/bench/fsbench/linux.sh
#
# SLATES and BENCH name other binaries. FILES and ROUNDS shrink the workload for a smoke run; recorded numbers use the
# program's defaults. The slates mounts are unmounted before their daemon stops, and the kernel's after that.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../../../.." && pwd)
SLATES=${SLATES:-$ROOT/target/release/slates}
BENCH=${BENCH:-$ROOT/target/release/examples/mount_bench}
# The base's files: as many as the 2026-10-19 runs' base, in one directory the work goes into.
BASE_FILES=500
SUDO=
if [ "$(id -u)" -ne 0 ]; then SUDO=sudo; fi
W=${RUNNER_TEMP:-$ROOT/target}/fsbench-$$
INSTANCE=fsb-$$
ANCHOR=
mkdir -p "$W/tmpfs" "$W/base" "$W/ovl" "$W/slates" "$W/slates-base"

cleanup() {
  set +e
  for mounted in "$W/slates" "$W/slates-base"; do
    if mountpoint -q "$mounted"; then
      "$SLATES" --instance "$INSTANCE" unmount "$mounted" || fusermount3 -u -z "$mounted"
    fi
  done
  if [ -n "$ANCHOR" ]; then kill "$ANCHOR" 2>/dev/null; wait "$ANCHOR" 2>/dev/null; fi
  for mounted in "$W/ovl" "$W/base" "$W/tmpfs"; do
    if mountpoint -q "$mounted"; then $SUDO umount "$mounted"; fi
  done
  rm -f "$W/anchor.err"
  rmdir "$W/tmpfs" "$W/base" "$W/ovl" "$W/slates" "$W/slates-base" "$W"
}
trap cleanup EXIT

$SUDO mount -t tmpfs -o size=1g,mode=1777 tmpfs "$W/tmpfs"
$SUDO mount -t tmpfs -o size=1g,mode=1777 tmpfs "$W/base"
mkdir -p "$W/base/lower/d" "$W/base/upper" "$W/base/work"
for index in $(seq 0 $((BASE_FILES - 1))); do printf 'base %d\n' "$index" > "$W/base/lower/d/b$index"; done
$SUDO mount -t overlay overlay \
  -o "lowerdir=$W/base/lower,upperdir=$W/base/upper,workdir=$W/base/work" "$W/ovl"

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
BASED=$("$SLATES" --instance "$INSTANCE" volume create fsb-base --bounded 256MiB --base "$W/base/lower" \
  | awk '$1 == "id:" { print $2 }')
[ -n "$PLAIN" ] || { echo "volume create printed no id for plain"; exit 1; }
[ -n "$BASED" ] || { echo "volume create printed no id for based"; exit 1; }
"$SLATES" --instance "$INSTANCE" mount "$PLAIN" "$W/slates" > /dev/null
"$SLATES" --instance "$INSTANCE" mount "$BASED" "$W/slates-base" > /dev/null

echo "# date $(date -u +%FT%TZ); $(uname -srm); slates $(git -C "$ROOT" rev-parse --short HEAD 2> /dev/null || echo unknown), release"
echo "# base: $BASE_FILES files in d/ under the overlay's lower directory and the slates-base volume"
"$BENCH" \
  --arm tmpfs="$W/tmpfs" --arm overlayfs="$W/ovl" \
  --arm slates="$W/slates" --arm slates-base="$W/slates-base" \
  ${FILES:+--files "$FILES"} ${ROUNDS:+--rounds "$ROUNDS"}
echo "# the daemon's counters after the run: NFS calls by procedure, FUSE and WinFsp events (status)"
"$SLATES" --instance "$INSTANCE" status | tr -d '\r' | grep -E "nfs_calls|fuse\.|winfsp\." || true
