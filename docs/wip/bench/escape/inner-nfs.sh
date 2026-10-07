#!/bin/bash
# The generated escape battery over Linux's own NFSv4.2 client (a privileged container): the daemon exports a
# volume, the kernel mounts it on loopback, and battery.py runs as `tester` (the agent) and `other` (a tool).
apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq nfs-common python3 procps util-linux >/dev/null 2>&1 || { echo APT-FAILED; exit 1; }
id tester >/dev/null 2>&1 || useradd -m tester; id other >/dev/null 2>&1 || useradd -m other
mkdir -p /target/esc && chown -R tester /target/esc /usr/local/cargo/registry
su tester -c "cd /src && PATH=/usr/local/cargo/bin:\$PATH CARGO_TARGET_DIR=/target/esc CARGO_HOME=/usr/local/cargo cargo build -q --release -p slates-cli 2>&1 | tail -2"
B=/target/esc/release/slates; I=escn$$; M=/mnt/slates
mkdir -p /secret && head -c 4096 /dev/urandom > /secret/canary && chmod 644 /secret/canary && chmod 755 /secret
outside_hash() { find / -xdev \( -path /proc -o -path /sys -o -path /dev -o -path /home/tester -o -path /target -o -path /tmp -o -path /run -o -path /var/lib/apt -o -path /var/cache -o -path /var/log -o -path /mnt \) -prune -o -type f -print0 2>/dev/null | sort -z | xargs -0 sha256sum 2>/dev/null | sha256sum | cut -c1-16; }
BEFORE=$(outside_hash)
su tester -c "$B --instance $I anchor --quick > /home/tester/a.log 2>&1 &"
for _ in $(seq 1 200); do su tester -c "$B --instance $I volume list" >/dev/null 2>&1 && break; sleep 0.1; done
sleep 1; su tester -c "$B --instance $I bootstrap root" >/dev/null
ID=$(su tester -c "$B --instance $I volume create esc --bounded 512MiB" | awk '/^id/{print $NF}')
su tester -c "$B --instance $I export $ID" > /tmp/export.txt 2>&1
PORT=$(awk '/^port:/{print $2}' /tmp/export.txt); EXPORT=$(awk '/^export:/{print $2}' /tmp/export.txt)
[ -n "$PORT" ] || { echo "EXPORT-FAILED: $(cat /tmp/export.txt)"; exit 1; }
mkdir -p $M
# Mounted nosuid,nodev, as kubelet mounts the export (GAPS 2026-10-06, "Kubernetes pods do not get the PersistentVolume's nosuid").
# Unmount on every exit path while the daemon runs (a hard mount whose server exited wedges teardown).
trap "umount -f -l $M 2>/dev/null" EXIT
mount -t nfs -o "vers=4.2,proto=tcp,port=$PORT,hard,timeo=600,nosuid,nodev" "127.0.0.1:$EXPORT" $M || { echo MOUNT-FAILED; exit 1; }
chown tester $M 2>/dev/null; chmod 777 $M
D=$(pgrep -u tester -f "slates --instance $I daemon" | head -1)
W0=$(su tester -c "awk '/^write_bytes/{print \$2}' /proc/$D/io"); [ -n "$W0" ] || { echo "CANNOT-READ-DAEMON-IO (vacuous)"; exit 1; }
su other -s /bin/sh -c "ls $M >/dev/null && echo 'other reaches the mount'" || echo "OTHER-CANNOT-REACH (vacuous)"
FAIL=0
for SEED in ${SEEDS:-1 2 3 4 5}; do python3 /battery.py $M /secret $SEED ${STEPS:-300} || FAIL=1; done
W1=$(su tester -c "awk '/^write_bytes/{print \$2}' /proc/$D/io")
echo "daemon disk writes during the battery: $((W1 - W0)) bytes"
umount -f -l $M; trap - EXIT; pkill -u tester -f "$B --instance $I"; sleep 1
AFTER=$(outside_hash)
[ "$BEFORE" = "$AFTER" ] && echo "outside tree unchanged ($BEFORE)" || { echo "OUTSIDE TREE CHANGED: $BEFORE -> $AFTER"; FAIL=1; }
exit $FAIL
