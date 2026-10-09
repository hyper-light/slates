#!/bin/bash
# Condition 5: an overlay volume over a real tree (`volume create --base`), read and changed through the Linux NFSv4.2
# client, and its change set planned by `slates land`. The comparators are the same tree read on the host, the same tree
# exported by the kernel's own NFS server through the same client (the client's floor), and `diff -rq` over a host copy
# given the same changes. The tree is copied into tmpfs first so every reader starts from RAM. Each round uses a fresh
# volume and fresh mounts, so the first read is cold in the NFS client. Run in a privileged container with the Linux
# release build:
#   docker run --rm --privileged -v <target-volume>:/target:ro -v "$PWD/docs/wip/bench/overlay":/o:ro rust:1.98.0 \
#     bash /o/overlay_diff.sh [TREE] [ROUNDS]     (SLATES names the binary when it is not /target/release/slates)
set -u
TREE=${1:-/usr/lib}; ROUNDS=${2:-3}
apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq nfs-common nfs-kernel-server >/dev/null 2>&1
cleanup() { umount -f -l /mnt/slates /mnt/knfs 2>/dev/null; exportfs -ua 2>/dev/null; rpc.nfsd 0 2>/dev/null; }
trap cleanup EXIT
mkdir -p /base /mnt/knfs /mnt/slates /copy
mount -t tmpfs tmpfs /base && mount -t tmpfs tmpfs /copy
cp -a "$TREE/." /base/
mountpoint -q /proc/fs/nfsd || mount -t nfsd nfsd /proc/fs/nfsd
echo "/base 127.0.0.1(ro,sync,no_subtree_check,no_root_squash,fsid=0)" > /etc/exports
# A fresh nfsd refuses new opens for its v4 grace period; shorten it and wait it out (copy_tree.sh, 2026-10-08).
echo 10 > /proc/fs/nfsd/nfsv4gracetime 2>/dev/null; echo 10 > /proc/fs/nfsd/nfsv4leasetime 2>/dev/null
rpcbind >/dev/null 2>&1; rpc.nfsd 8 && exportfs -ra && rpc.mountd >/dev/null 2>&1
sleep 12
cp "${SLATES:-/target/release/slates}" /usr/local/bin/slates
slates --instance ov anchor --quick --shards 4 >/dev/null 2>&1 &
for _ in $(seq 1 300); do slates --instance ov volume list >/dev/null 2>&1 && break; sleep 0.1; done
slates --instance ov bootstrap root >/dev/null 2>&1
FILES=$(find /base -type f | wc -l)
echo "tree $TREE: $FILES files, $(find /base | wc -l) entries, $(du -sb /base | cut -f1) bytes; load $(cut -d' ' -f1-3 /proc/loadavg)"
ms() { echo $(( ($2 - $1) / 1000000 )); }
now() { date +%s%N; }
# The changes: 10 appends, 5 creates, 3 removals, on files picked by a fixed stride so every round picks the same ones.
mapfile -t PICKS < <(cd /base && find . -type f -size +0 | sort | awk -v n="$FILES" 'NR % int(n / 14 + 1) == 1' | head -13)
change() {
  local root=$1
  for k in $(seq 0 9); do printf 'appended\n' >> "$root/${PICKS[$k]}"; done
  for k in $(seq 1 5); do printf 'created %s\n' "$k" > "$root/created-$k"; done
  for k in 10 11 12; do rm -f "$root/${PICKS[$k]}"; done
}
plan_summary() {
  python3 -c 'import json; s = json.load(open("/root/plan.json"))["summary"]; print(" ".join(a["action"] + "=" + str(a["count"]) for a in s["by_action"]))'
}
walk() { find "$1" -type f | wc -l >/dev/null; }
read_all() { find "$1" -type f -print0 | xargs -0 cat > /dev/null; }
# The client's calls per operation since the last snapshot, for an NFS mount (from /proc/self/mountstats), so a phase's
# cost can be split into its round trips.
ops() {
  python3 - "$1" /root/ops-"$(basename "$1")".prev <<'PY'
import json, os, sys
mount, prev_path = sys.argv[1], sys.argv[2]
counts, inside, stats = {}, False, False
for line in open("/proc/self/mountstats"):
    if line.startswith("device "):
        inside = (" mounted on " + mount + " ") in line
        stats = False
    elif inside and "per-op statistics" in line:
        stats = True
    elif inside and stats and ":" in line:
        name, rest = line.strip().split(":", 1)
        fields = rest.split()
        if fields:
            counts[name] = int(fields[0])
prev = json.load(open(prev_path)) if os.path.exists(prev_path) else {}
json.dump(counts, open(prev_path, "w"))
delta = {k: v - prev.get(k, 0) for k, v in counts.items() if v - prev.get(k, 0) > 0}
print(" ".join(f"{k}={v}" for k, v in sorted(delta.items(), key=lambda kv: -kv[1])))
PY
}
for round in $(seq 1 "$ROUNDS"); do
  T0=$(now); ID=$(slates --instance ov volume create "ov$round" --dynamic 16GiB --base /base 2>&1 | awk '/^id/{print $2}'); T1=$(now)
  slates --instance ov export "$ID" > /root/e.txt 2>&1
  PORT=$(awk '/^port:/{print $2}' /root/e.txt); EXPORT=$(awk '/^export:/{print $2}' /root/e.txt)
  mount -t nfs -o "vers=4.2,proto=tcp,port=$PORT,hard" "127.0.0.1:$EXPORT" /mnt/slates || { echo "slates mount failed"; exit 1; }
  mount -t nfs -o vers=4.2,proto=tcp,hard,ro 127.0.0.1:/ /mnt/knfs || { echo "kernel nfs mount failed"; exit 1; }
  line="round $round: create $(ms "$T0" "$T1") ms"
  for target in slates knfs host; do
    case $target in slates) D=/mnt/slates;; knfs) D=/mnt/knfs;; host) D=/base;; esac
    A=$(now); walk "$D"; B=$(now); read_all "$D"; C=$(now); read_all "$D"; E=$(now)
    line="$line | $target walk $(ms "$A" "$B") cold $(ms "$B" "$C") warm $(ms "$C" "$E") ms"
  done
  echo "$line"
  if [ "$round" = 1 ]; then
    # One more pass per NFS mount, each phase's calls counted: a fresh mount, then the walk, the cold and warm reads.
    for D in /mnt/slates /mnt/knfs; do
      ops "$D" >/dev/null; walk "$D"; echo "  $D walk again: $(ops "$D")"
      read_all "$D"; echo "  $D read again: $(ops "$D")"
    done
  fi
  change /mnt/slates
  A=$(now); slates --instance ov land "$ID" /base --json > /root/plan.json 2>&1; B=$(now)
  rm -rf /copy/* && cp -a /base/. /copy/ && change /copy
  C=$(now); diff -rq /base /copy > /root/diff.txt 2>&1; E=$(now)
  echo "round $round: land plan $(ms "$A" "$B") ms, $(plan_summary) | diff -rq $(ms "$C" "$E") ms, $(grep -c '^Files .* differ$' /root/diff.txt) differ, $(grep -c '^Only in /copy' /root/diff.txt) only in the copy, $(grep -c '^Only in /base' /root/diff.txt) only in the base, $(grep -vc '^Files .* differ$\|^Only in' /root/diff.txt) other lines"
  [ "$round" = 1 ] && grep -v '^Files .* differ$\|^Only in' /root/diff.txt | head -5
  umount /mnt/slates /mnt/knfs
done
slates --instance ov status --json > /root/status.json 2>/dev/null
grep -o '"nfs.local_p99_ns","value":[0-9]*\|"nfs.local_p50_ns","value":[0-9]*' /root/status.json | head -2
