#!/bin/bash
set -u
# A-96: 2,000 creates (open O_CREAT, write, close, stat; `creates.py`) through Linux's FUSE mount of a slates volume,
# beside tmpfs, then again under `perf record` of the daemon. Run with the Linux release build at /target:
#   docker run --rm --privileged -v <linux-target>:/target:ro -v "$PWD/docs/wip/bench/fuse":/c:ro -v <out>:/out \
#     rust:1.98.0 bash /c/fuse-perf.sh
apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq fuse3 linux-perf python3 >/dev/null 2>&1
cp /target/release/slates /usr/local/bin/slates
slates --instance fp anchor --quick --shards 2 >/out/anchor.out 2>/out/anchor.err &
for _ in $(seq 1 200); do slates --instance fp volume list >/dev/null 2>&1 && break; sleep 0.1; done
slates --instance fp bootstrap root >/dev/null 2>&1
ID=$(slates --instance fp volume create fp --dynamic 1GiB 2>&1 | awk '/^id/{print $2}')
mkdir -p /mnt/v /ram; trap "fusermount3 -u -z /mnt/v 2>/dev/null" EXIT
slates --instance fp mount "$ID" /mnt/v >/dev/null || exit 1
mount -t tmpfs tmpfs /ram
echo "tmpfs: $(python3 /c/creates.py /ram/a 2000)"
echo "slates: $(python3 /c/creates.py /mnt/v/a 2000)"
DP=$(pgrep -f "slates --instance fp daemon" | head -1)
perf record -F 4999 -g -o /out/fuse.perf -p $DP -- sleep 4 >/out/perf.err 2>&1 &
PP=$!; sleep 0.3
echo "slates (profiled): $(python3 /c/creates.py /mnt/v/b 2000)"
wait $PP
perf report -i /out/fuse.perf --no-children --sort symbol --stdio -g none 2>/dev/null | grep -v "^#" | grep "%" | head -30 > /out/fuse-self.txt
perf report -i /out/fuse.perf --children --sort symbol --stdio -g none 2>/dev/null | grep -v "^#" | grep "%" | head -45 > /out/fuse-children.txt
tail -1 /out/perf.err
slates --instance fp status 2>/dev/null | grep -iE "fuse\.|local_p" | head -20
