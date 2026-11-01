#!/bin/bash
# The mount benchmark on Windows (T-4.8; step 2 of the VFS and DAX plan): slates' WinFsp drive against NTFS on the
# runner's disk. crates/cli/examples/mount_bench.rs times every call, quiet and then with every core loaded; WinFsp's own
# benchmark, fsbench (tst/fsbench in WinFsp's tree, which its installer does not ship), then runs its file, read-write
# and mapped-file tests in a directory of each. Windows has no RAM disk of its own, so the NTFS arm is the runner's
# disk with the system cache in front of it, which is what a Windows user's tree sits on.
#
# Needs a release build and WinFsp installed. From the repository root, in Git Bash:
#
#   cargo build --release --locked -p slates-cli --bin slates --example mount_bench
#   bash docs/wip/bench/fsbench/windows.sh
#
# SLATES and BENCH name other binaries; FSBENCH names a built fsbench.exe (unset: that part is skipped, and said so).
# FILES and ROUNDS shrink the workload for a smoke run; recorded numbers use the program's defaults.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../../../.." && pwd)
SLATES=${SLATES:-$ROOT/target/release/slates.exe}
BENCH=${BENCH:-$ROOT/target/release/examples/mount_bench.exe}
FSBENCH=${FSBENCH:-}
W=${RUNNER_TEMP:-$ROOT/target}/fsbench-$$
INSTANCE=fsb-$$
ANCHOR=
DRIVE=
mkdir -p "$W/ntfs"

cleanup() {
  set +e
  if [ -n "$DRIVE" ] && [ -e "$DRIVE/" ]; then "$SLATES" --instance "$INSTANCE" unmount "$DRIVE"; fi
  if [ -n "$ANCHOR" ]; then kill "$ANCHOR" 2>/dev/null; wait "$ANCHOR" 2>/dev/null; fi
  rm -f "$W/anchor.err"
  rmdir "$W/ntfs/fsbench" "$W/ntfs" "$W" 2>/dev/null
}
trap cleanup EXIT

for letter in S T U V W X Y Z; do
  if [ ! -e "$letter:/" ]; then DRIVE="$letter:"; break; fi
done
[ -n "$DRIVE" ] || { echo "no free drive letter from S: to Z:"; exit 1; }

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
PLAIN=$("$SLATES" --instance "$INSTANCE" volume create fsb --bounded 256MiB | tr -d '\r' | awk '$1 == "id:" { print $2 }')
[ -n "$PLAIN" ] || { echo "volume create printed no id for plain"; exit 1; }
"$SLATES" --instance "$INSTANCE" mount "$PLAIN" "$DRIVE" > /dev/null
for _ in $(seq 1 600); do
  if [ -d "$DRIVE/" ]; then break; fi
  sleep 0.1
done

echo "# date $(date -u +%FT%TZ); $(cmd //c ver | tr -d '\r' | grep -v '^$'); $PROCESSOR_ARCHITECTURE; slates $(git -C "$ROOT" rev-parse --short HEAD 2> /dev/null || echo unknown), release"
"$BENCH" \
  --arm ntfs="$W/ntfs" --arm slates="$DRIVE/" \
  ${FILES:+--files "$FILES"} ${ROUNDS:+--rounds "$ROUNDS"}

if [ -z "$FSBENCH" ]; then
  echo "# WinFsp's fsbench: skipped, FSBENCH is unset"
  exit 0
fi
for arm in ntfs slates; do
  if [ "$arm" = ntfs ]; then where="$W/ntfs/fsbench"; else where="$DRIVE/fsbench"; fi
  mkdir -p "$where"
  echo "# WinFsp's fsbench on $arm ($where), its default counts"
  (cd "$where" && "$FSBENCH")
  rmdir "$where"
done
