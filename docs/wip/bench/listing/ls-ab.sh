#!/bin/bash
# A-95's listing loop on Linux's own NFSv4.2 client over loopback: 8,000 files, then 300 rounds of create-one-and-list
# under a fresh prefix, printing the listing p50/p99 and mountstats' READDIR count, bytes and round trip. SHARDS picks
# the daemon's shards; PERF=1 also profiles the daemon. Run with the Linux release build at /target:
#   docker run --rm --privileged -e SHARDS=4 -v <linux-target>:/target:ro -v "$PWD/docs/wip/bench/listing":/bench:ro \
#     -v <out>:/out python:3.12-slim-trixie bash /bench/ls-ab.sh
set -u
apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq nfs-common linux-perf >/dev/null 2>&1
cp /target/release/slates /usr/local/bin/slates
slates --instance ls anchor --quick --shards ${SHARDS:-4} >/out/anchor.out 2>/out/anchor.err &
for _ in $(seq 1 200); do slates --instance ls volume list >/dev/null 2>&1 && break; sleep 0.1; done
slates --instance ls bootstrap root >/dev/null 2>&1
ID=$(slates --instance ls volume create ls --bounded 256MiB 2>&1 | awk '/^id/{print $2}')
slates --instance ls export "$ID" > /out/export.txt 2>&1
PORT=$(awk '/^port:/{print $2}' /out/export.txt); EXPORT=$(awk '/^export:/{print $2}' /out/export.txt)
mkdir -p /mnt/slates; trap "umount -f -l /mnt/slates 2>/dev/null" EXIT
mount -t nfs -o "vers=4.2,proto=tcp,port=$PORT,hard,timeo=600" "127.0.0.1:$EXPORT" /mnt/slates || exit 1
python3 /bench/lsonly.py /mnt/slates 8000 0
python3 /bench/lsonly.py /mnt/slates 0 300 a
if [ "${PERF:-0}" = 1 ]; then
  perf record -F 4999 -g -o /out/ls.perf -p "$(pgrep -f 'slates --instance ls daemon' | head -1)" -- sleep 4 >/out/perf.err 2>&1 &
  PERFPID=$!
  sleep 0.3
  python3 /bench/lsonly.py /mnt/slates 0 300 b
  wait $PERFPID
  perf report -i /out/ls.perf --no-children --sort symbol --stdio 2>/dev/null | grep -v "^#" | grep "%" | head -40 > /out/ls-perf.txt
  perf report -i /out/ls.perf --children --sort symbol --stdio -g none 2>/dev/null | grep -v "^#" | grep "%" | head -40 > /out/ls-perf-children.txt
fi
awk '/^device .* mounted on \/mnt\/slates/{f=1} f&&/per-op statistics/{p=1;next} p&&NF==0{exit} p{print}' /proc/self/mountstats | awk '$1=="READDIR:" {printf "READDIR ops %d bytes_recv/op %d rtt %.3f\n", $2, $6/$2, $8/$2}'
