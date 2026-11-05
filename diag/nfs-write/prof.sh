#!/bin/bash
# DIAG, not for main. The incumbent harness's large-file step (a new file written in 1 MiB writes, fsynced) through
# Linux's own NFSv4.2 client on loopback, against slates (unprivileged, 4 shards) and the kernel's nfsd exporting a
# tmpfs; then the same write at MIB MiB under perf on slates' daemon. Mounts and nfsd need root (sudo); slates does not.
# Usage: prof.sh <slates binary> [MIB] [ROUNDS]
set -u
BIN=$1; MIB=${2:-1024}; ROUNDS=${3:-7}
OUT=${OUT:-$RUNNER_TEMP/nfswrite}; mkdir -p "$OUT"
cleanup() { sudo umount -f -l /mnt/slates /mnt/knfs 2>/dev/null; kill "$ANCHOR" 2>/dev/null; }
trap cleanup EXIT
sudo mkdir -p /exp /mnt/knfs /mnt/slates
sudo mount -t tmpfs tmpfs /exp
echo "/exp 127.0.0.1(rw,sync,no_subtree_check,no_root_squash,fsid=0)" | sudo tee /etc/exports >/dev/null
sudo systemctl stop nfs-server 2>/dev/null
mountpoint -q /proc/fs/nfsd || sudo mount -t nfsd nfsd /proc/fs/nfsd
echo 10 | sudo tee /proc/fs/nfsd/nfsv4gracetime >/dev/null; echo 10 | sudo tee /proc/fs/nfsd/nfsv4leasetime >/dev/null
sudo systemctl start nfs-server && sudo exportfs -ra
sudo mount -t nfs -o vers=4.2,proto=tcp,hard 127.0.0.1:/ /mnt/knfs || { echo "kernel nfs mount failed"; exit 1; }
sleep 12; sudo touch /mnt/knfs/.warm && sudo rm -f /mnt/knfs/.warm
"$BIN" --instance pw anchor --quick --shards 4 >"$OUT/anchor.out" 2>"$OUT/anchor.err" &
ANCHOR=$!
for _ in $(seq 1 200); do "$BIN" --instance pw volume list >/dev/null 2>&1 && break; sleep 0.1; done
"$BIN" --instance pw bootstrap root >/dev/null 2>&1
ID=$("$BIN" --instance pw volume create pw --dynamic 3GiB 2>&1 | awk '/^id/{print $2}')
"$BIN" --instance pw export "$ID" > "$OUT/export.txt" 2>&1
PORT=$(awk '/^port:/{print $2}' "$OUT/export.txt"); EXPORT=$(awk '/^export:/{print $2}' "$OUT/export.txt")
sudo mount -t nfs -o "vers=4.2,proto=tcp,port=$PORT,hard" "127.0.0.1:$EXPORT" /mnt/slates || { echo "slates mount failed"; exit 1; }
grep -E " /mnt/(slates|knfs) " /proc/mounts
DAEMON=$(awk -v a="$ANCHOR" '/^PPid:/ && $2 == a {split(FILENAME, p, "/"); print p[3]; exit}' /proc/[0-9]*/status 2>/dev/null)
echo "anchor $ANCHOR daemon $DAEMON"
cat > "$OUT/writer.py" <<'PY'
import os, sys, time, random
path, mib = sys.argv[1], int(sys.argv[2])
rng = random.Random(7)
chunk = bytes(rng.getrandbits(8) for _ in range(1 << 20))
t = time.perf_counter()
fd = os.open(path, os.O_CREAT | os.O_WRONLY | os.O_TRUNC, 0o644)
for _ in range(mib):
    os.write(fd, chunk)
os.fsync(fd)
os.close(fd)
d = time.perf_counter() - t
print(f"{mib / d:.1f}")
PY
cpu() { awk '{print $14 + $15}' "/proc/$1/stat"; }
echo "== interleaved 64 MiB new-file writes, MiB/s (slates, nfsd), load $(cut -d' ' -f1-3 /proc/loadavg)"
for r in $(seq 1 "$ROUNDS"); do
  s=$(sudo python3 "$OUT/writer.py" /mnt/slates/large$r.bin 64); k=$(sudo python3 "$OUT/writer.py" /mnt/knfs/large$r.bin 64)
  echo "round $r slates $s nfsd $k"
  sudo rm -f /mnt/slates/large$r.bin /mnt/knfs/large$r.bin
done
echo "== $MIB MiB new-file write, interleaved, MiB/s and slates daemon CPU ticks (100/s)"
for r in 1 2 3; do
  b=$(cpu "$DAEMON"); s=$(sudo python3 "$OUT/writer.py" /mnt/slates/big.bin "$MIB"); a=$(cpu "$DAEMON")
  k=$(sudo python3 "$OUT/writer.py" /mnt/knfs/big.bin "$MIB")
  echo "round $r slates $s (daemon cpu $((a - b)) ticks) nfsd $k"
  sudo rm -f /mnt/slates/big.bin /mnt/knfs/big.bin
done
for t in /proc/$DAEMON/task/*; do awk '{printf "%s %s %d\n", $1, $2, $14+$15}' $t/stat; done | sort -k3 -n -r | head -8 > "$OUT/t0"
echo "== perf of the daemon during a $MIB MiB write"
sudo perf record -F 1999 --call-graph fp -p "$DAEMON" -o "$OUT/perf.data" >/dev/null 2>&1 &
PERF=$!; sleep 1
s=$(sudo python3 "$OUT/writer.py" /mnt/slates/big.bin "$MIB"); echo "profiled write $s MiB/s"
sudo kill -INT $PERF; wait $PERF
echo "== busiest daemon threads (cumulative ticks)"; for t in /proc/$DAEMON/task/*; do awk '{printf "%s %s %d\n", $1, $2, $14+$15}' $t/stat; done | sort -k3 -n -r | head -8
for m in slates knfs; do
  echo "== mountstats $m (op, ops, ..., avg queue/rtt/exe ms)"
  awk -v m="/mnt/$m" '$0 ~ "mounted on " m " " {f=1} f&&/per-op statistics/{p=1;next} p&&NF==0{exit} p{print}' /proc/self/mountstats \
    | awk '$2>0 && ($1=="WRITE:"||$1=="COMMIT:"||$1=="GETATTR:"||$1=="OPEN:"||$1=="CLOSE:"||$1=="SETATTR:"||$1=="LAYOUTSTATS:") {printf "%s ops=%s bytes_sent=%s rtt_total_ms=%s exe_total_ms=%s\n", $1, $2, $5, $8, $9}'
done
"$BIN" --instance pw status --json > "$OUT/status.json" 2>/dev/null
sudo chown -R "$(id -u)" "$OUT"
perf report -i "$OUT/perf.data" --no-children --sort symbol --stdio 2>/dev/null | grep -v "^#" | grep "%" | head -70
echo "== with children"
perf report -i "$OUT/perf.data" --children --sort symbol --stdio -g none 2>/dev/null | grep -v "^#" | grep "%" | head -90
echo "== by thread"
perf report -i "$OUT/perf.data" --no-children --sort comm --stdio -g none 2>/dev/null | grep -v "^#" | grep "%" | head -10
perf report -i "$OUT/perf.data" --no-children --sort symbol --stdio -G --percent-limit 0.8 2>/dev/null > "$OUT/perf-callers.txt"
perf report -i "$OUT/perf.data" --children --sort symbol --stdio -g caller,0.5,callee --percent-limit 3 2>/dev/null > "$OUT/perf-tree.txt"
