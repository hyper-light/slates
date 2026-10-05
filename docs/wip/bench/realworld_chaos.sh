#!/bin/bash
# Condition 11 under real load: the daemon SIGKILLed twice in the middle of each real workload on its NFSv4.2 volume. The
# anchor restarts it over the RAM it holds; the hard mount retries; each workload must finish exactly as on tmpfs.
# Run as `realworld_native.sh` is (privileged; /target, /out). Each `wait` names the killer's pid: a bare `wait` also
# waits for the anchor, a background child of this script that never exits, and hung the first run (2026-10-05).
set -u
apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq nfs-common python3-venv git >/dev/null 2>&1
cp /target/release/slates /usr/local/bin/slates
slates --instance ch anchor --quick --shards 4 >/out/anchor.out 2>/out/anchor.err &
for _ in $(seq 1 200); do slates --instance ch volume list >/dev/null 2>&1 && break; sleep 0.1; done
slates --instance ch bootstrap root >/dev/null 2>&1
ID=$(slates --instance ch volume create ch --dynamic 3GiB 2>&1 | awk '/^id/{print $2}')
[ -n "$ID" ] || { echo "create failed"; exit 1; }
slates --instance ch export "$ID" > /out/export.txt 2>&1
PORT=$(awk '/^port:/{print $2}' /out/export.txt); EXPORT=$(awk '/^export:/{print $2}' /out/export.txt)
mkdir -p /mnt/slates
trap "umount -f -l /mnt/slates 2>/dev/null" EXIT
mount -t nfs -o "vers=4.2,proto=tcp,port=$PORT,hard,timeo=600" "127.0.0.1:$EXPORT" /mnt/slates || { echo "mount failed"; exit 1; }
D=/mnt/slates
# Kills the daemon (never the anchor) after each delay in turn, while the workload runs.
killer() { for delay in "$@"; do sleep "$delay"; P=$(pgrep -f "slates --instance ch daemon" | head -1); [ -n "$P" ] && kill -9 "$P" && echo "  killed daemon $P at +$delay s"; done; }
run() { local name=$1; shift; local s=$(date +%s.%N); "$@" > /out/$name.log 2>&1; local c=$?; echo "$name $(python3 -c "print(round($(date +%s.%N)-$s,2))") s exit=$c"; }
echo "== git clone, killed twice"
killer 0.3 0.6 & K=$!; run clone git clone -q --depth 1 https://github.com/BurntSushi/ripgrep $D/rg; wait $K
echo "fsck $(git -C $D/rg fsck --full >/dev/null 2>&1; echo exit=$?)  tree $(cd $D/rg && find . -path ./.git -prune -o -type f -print0 | sort -z | xargs -0 sha256sum | sha256sum | cut -c1-16)"
echo "== cargo build, killed twice"
mkdir -p $D/proj && cd $D/proj && cargo init -q --name probe . >/dev/null 2>&1
printf 'regex = "1"\nserde = { version = "1", features = ["derive"] }\nserde_json = "1"\n' >> Cargo.toml
printf 'use serde::Serialize;\n#[derive(Serialize)] struct P { n: usize }\nfn main() { let r = regex::Regex::new("a+b").unwrap(); println!("{}", serde_json::to_string(&P { n: r.find_iter("aab ab b").count() }).unwrap()); }\n' > src/main.rs
killer 1.0 1.5 & K=$!; CARGO_HOME=$D/cargo-home run cargo cargo build -q --release; wait $K
echo "run $($D/proj/target/release/probe 2>&1)"
cd /
echo "== pip install, killed twice"
killer 0.5 0.8 & K=$!; run pip sh -c "python3 -m venv $D/venv && $D/venv/bin/pip install -q --disable-pip-version-check requests flask"; wait $K
echo "import $($D/venv/bin/python -c 'import flask, requests; print(flask.__name__, requests.__version__)' 2>&1)"
echo "== the anchor's account"
slates --instance ch status 2>/dev/null | grep -E "^generation|^restarts" | tr '\n' ' '; echo
grep -c "restart" /out/anchor.err
