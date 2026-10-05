#!/bin/bash
# Condition 2 through containerd + runc: the OCI mounts[] entry `slates attach --oci` returns, applied by `ctr run`.
# containerd 1.7 and runc 1.1 from Debian trixie, in a privileged container (nested cgroup v2 delegated as dind does;
# the native snapshotter, since overlayfs does not stack on Docker's overlay). Run with the Linux release build:
#   docker run --rm --privileged -v <linux-target>:/target:ro -v "$PWD/docs/wip/bench/containerd":/c:ro \
#     -v "$PWD/docs/wip/bench/fuse":/f:ro -v <out>:/out rust:1.98.0 bash /c/run.sh
set -u
# Nested cgroup v2 (as docker:dind's entrypoint does): move this container's processes into a child group so runc may
# enable the domain controllers beneath the root.
mkdir -p /sys/fs/cgroup/init
while read -r pid; do echo "$pid" > /sys/fs/cgroup/init/cgroup.procs 2>/dev/null || true; done < /sys/fs/cgroup/cgroup.procs
sed -e 's/ / +/g' -e 's/^/+/' < /sys/fs/cgroup/cgroup.controllers > /sys/fs/cgroup/cgroup.subtree_control
apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq containerd runc fuse3 jq >/dev/null 2>&1
containerd --version; runc --version | head -1
containerd >/out/containerd.log 2>&1 &
for _ in $(seq 1 100); do ctr version >/dev/null 2>&1 && break; sleep 0.1; done
ctr images pull --snapshotter native -q docker.io/library/alpine:3.20 >/dev/null 2>&1; ctr images pull --snapshotter native -q docker.io/library/debian:trixie-slim >/dev/null 2>&1 || ctr images pull --snapshotter native docker.io/library/alpine:3.20 | tail -1
cp /target/release/slates /usr/local/bin/slates
slates --instance cd anchor --quick --shards 2 >/out/anchor.out 2>/out/anchor.err &
for _ in $(seq 1 200); do slates --instance cd volume list >/dev/null 2>&1 && break; sleep 0.1; done
slates --instance cd bootstrap root >/dev/null 2>&1
ID=$(slates --instance cd volume create cd --dynamic 1GiB 2>&1 | awk '/^id/{print $2}')
mkdir -p /mnt/v; trap "slates --instance cd unmount /mnt/v >/dev/null 2>&1; fusermount3 -u -z /mnt/v 2>/dev/null" EXIT
slates --instance cd mount "$ID" /mnt/v --shared || exit 1
grep " /mnt/v " /proc/self/mountinfo | cut -d' ' -f5,6,9,10
ATT=$(slates --instance cd attach "$ID" --write --oci-source /mnt/v --oci-destination /work --json 2>/out/attach.err)
ENTRY=$(echo "$ATT" | jq -c '.established.binding.mount')
echo "entry: $ENTRY"
[ "$ENTRY" = "null" ] || [ -z "$ENTRY" ] && { echo "attach failed: $(cat /out/attach.err)"; exit 1; }
SRC=$(echo "$ENTRY" | jq -r .source); DST=$(echo "$ENTRY" | jq -r .destination); OPTS=$(echo "$ENTRY" | jq -r '.options|join(":")')
# The setuid probe's binary, planted by root on the host side of the volume.
cp /usr/bin/id /mnt/v/planted-id && chmod 4755 /mnt/v/planted-id
WORK='set -e; cd /work; mkdir -p w; cd w; i=0; while [ $i -lt 2000 ]; do echo "line $i" > f$i; i=$((i+1)); done
ln f1 hard1; ln -s f2 soft2; tar cf ../w.tar .; mkdir ../x; cd ../x; tar xf ../w.tar; ls | wc -l
cat hard1 soft2; sha256sum ../w.tar | cut -c1-16; grep " /work " /proc/self/mountinfo | cut -d" " -f6'
t0=$(date +%s.%N)
ctr run --snapshotter native --rm --mount "type=bind,src=$SRC,dst=$DST,options=$OPTS" docker.io/library/alpine:3.20 work sh -c "$WORK"
echo "ctr workload exit $? in $(python3 -c "print(round($(date +%s.%N)-$t0,2))" 2>/dev/null || echo ?) s"
echo "== setuid probe in the container as nobody: $(ctr run --snapshotter native --rm --user 65534:65534 --mount "type=bind,src=$SRC,dst=$DST,options=$OPTS" docker.io/library/debian:trixie-slim probe /work/planted-id -u 2>&1)"
echo "== the host side sees: $(ls /mnt/v/x | wc -l) extracted entries; tar sha $(sha256sum /mnt/v/w.tar | cut -c1-16)"
mkdir -p /ram && mount -t tmpfs tmpfs /ram
t0=$(date +%s.%N); ctr run --snapshotter native --rm --mount "type=bind,src=/ram,dst=/work,options=bind:rw:private" docker.io/library/alpine:3.20 workram sh -c "$WORK" >/dev/null; echo "== tmpfs bind, same workload: $(python3 -c "print(round($(date +%s.%N)-$t0,2))") s"
mkdir -p /mnt/v/hostside; t0=$(date +%s.%N); (cd /mnt/v/hostside && i=0; while [ $i -lt 2000 ]; do echo "line $i" > f$i; i=$((i+1)); done); echo "== 2000 creates from the host shell on the FUSE mount: $(python3 -c "print(round($(date +%s.%N)-$t0,2))") s"
echo "== read-only entry:"
RO=$(slates --instance cd attach "$ID" --read --oci-source /mnt/v --oci-destination /ro --json 2>&1 | jq -c '.established.binding.mount')
echo "$RO"
ROPTS=$(echo "$RO" | jq -r '.options|join(":")')
ctr run --snapshotter native --rm --mount "type=bind,src=$SRC,dst=/ro,options=$ROPTS" docker.io/library/alpine:3.20 ro sh -c 'cat /ro/x/f7; touch /ro/nope 2>&1; echo "write exit $?"'
