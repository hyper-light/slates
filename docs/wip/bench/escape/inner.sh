#!/bin/bash
# The generated escape battery in a Linux container: a slates FUSE volume mounted --shared by `tester`, hostile
# operations as `tester` (the agent) and `other` (a tool), the outside tree hashed before and after.
apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq fuse3 python3 procps >/dev/null 2>&1 || { echo APT-FAILED; exit 1; }
id tester >/dev/null 2>&1 || useradd -m tester; id other >/dev/null 2>&1 || useradd -m other
chmod 666 /dev/fuse; grep -q '^user_allow_other' /etc/fuse.conf 2>/dev/null || echo user_allow_other >> /etc/fuse.conf
mkdir -p /target/esc && chown -R tester /target/esc /usr/local/cargo/registry
su tester -c "cd /src && PATH=/usr/local/cargo/bin:\$PATH CARGO_TARGET_DIR=/target/esc CARGO_HOME=/usr/local/cargo cargo build -q --release -p slates-cli 2>&1 | tail -2"
B=/target/esc/release/slates; I=esc$$; M=/home/tester/m
mkdir -p /secret && head -c 4096 /dev/urandom > /secret/canary && chmod 644 /secret/canary && chmod 755 /secret
outside_hash() { find / -xdev \( -path /proc -o -path /sys -o -path /dev -o -path /home/tester -o -path /target -o -path /tmp -o -path /run -o -path /var/lib/apt -o -path /var/cache -o -path /var/log \) -prune -o -type f -print0 2>/dev/null | sort -z | xargs -0 sha256sum 2>/dev/null | sha256sum | cut -c1-16; }
BEFORE=$(outside_hash)
su tester -c "mkdir -p $M; $B --instance $I anchor --quick > /home/tester/a.log 2>&1 &"
for _ in $(seq 1 200); do su tester -c "$B --instance $I volume list" >/dev/null 2>&1 && break; sleep 0.1; done
sleep 1; su tester -c "$B --instance $I bootstrap root" >/dev/null
ID=$(su tester -c "$B --instance $I volume create esc --bounded 512MiB" | awk '/^id/{print $NF}')
su tester -c "$B --instance $I mount --shared $ID $M" >/dev/null || { echo MOUNT-FAILED; su tester -c "$B --instance $I mount --shared $ID $M"; exit 1; }
chmod 755 /home/tester; D=$(pgrep -u tester -f "slates --instance $I daemon" | head -1)
W0=$(su tester -c "awk '/^write_bytes/{print \$2}' /proc/$D/io")
[ -n "$W0" ] || { echo "CANNOT-READ-DAEMON-IO (vacuous)"; exit 1; }
# Non-vacuity: the other user can reach the shared mount at all.
su other -s /bin/sh -c "ls $M >/dev/null && echo 'other reaches the mount'" || echo "OTHER-CANNOT-REACH (vacuous)"
FAIL=0
for SEED in ${SEEDS:-1 2 3 4 5}; do python3 /battery.py $M /secret $SEED ${STEPS:-300} || FAIL=1; done
W1=$(su tester -c "awk '/^write_bytes/{print \$2}' /proc/$D/io")
echo "daemon disk writes during the battery: $((W1 - W0)) bytes"
su tester -c "fusermount3 -u -z $M"; pkill -u tester -f "$B --instance $I"; sleep 1
AFTER=$(outside_hash)
[ "$BEFORE" = "$AFTER" ] && echo "outside tree unchanged ($BEFORE)" || { echo "OUTSIDE TREE CHANGED: $BEFORE -> $AFTER"; FAIL=1; }
cmp -s /secret/canary /secret/canary && echo "canary intact"
exit $FAIL
