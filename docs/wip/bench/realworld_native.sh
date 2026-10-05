#!/bin/bash
# Run in a privileged container with the Linux release build of slates at /target and an output directory at /out:
#   docker run --rm --privileged -v <linux-target>:/target:ro -v "$PWD/docs/wip/bench":/rw:ro -v <out>:/out \
#     rust:1.98.0 bash /rw/realworld_native.sh
# The mount is unmounted on every exit path while the daemon still runs (a hard mount of an exiting daemon in its own
# container wedges the container's teardown).
# Real workloads on a slates NFSv4.2 volume through Linux's own client, beside tmpfs: a git clone + fsck, a cargo build
# with real dependencies, a pip install. Correctness: the clone's tree hash must match tmpfs's; the build's binary runs.
set -u
apt-get update -qq >/dev/null 2>&1 && apt-get install -y -qq nfs-common python3-venv git >/dev/null 2>&1
cp /target/release/slates /usr/local/bin/slates
slates --instance rw anchor --quick --shards 4 >/out/anchor.out 2>/out/anchor.err &
for _ in $(seq 1 200); do slates --instance rw volume list >/dev/null 2>&1 && break; sleep 0.1; done
slates --instance rw bootstrap root >/dev/null 2>&1
ID=$(slates --instance rw volume create rw --dynamic 3GiB 2>&1 | awk '/^id/{print $2}')
[ -n "$ID" ] || { echo "create failed"; exit 1; }
slates --instance rw export "$ID" > /out/export.txt 2>&1
PORT=$(awk '/^port:/{print $2}' /out/export.txt); EXPORT=$(awk '/^export:/{print $2}' /out/export.txt)
mkdir -p /mnt/slates /mnt/ram
trap "umount -f -l /mnt/slates 2>/dev/null; umount /mnt/ram 2>/dev/null" EXIT
mount -t nfs -o "vers=4.2,proto=tcp,port=$PORT,hard,timeo=600" "127.0.0.1:$EXPORT" /mnt/slates || { echo "mount failed"; exit 1; }
mount -t tmpfs -o size=3g tmpfs /mnt/ram
t() { local s=$(date +%s.%N); "$@" > /dev/null 2>&1; local c=$?; echo "$(python3 -c "print(round($(date +%s.%N)-$s,2))") exit=$c"; }
for where in ram slates; do
  D=/mnt/$where
  echo "== $where"
  echo "git clone   $(t git clone -q --depth 1 https://github.com/BurntSushi/ripgrep $D/rg)"
  echo "git fsck    $(t git -C $D/rg fsck --full)"
  echo "tree hash   $(cd $D/rg && find . -path ./.git -prune -o -type f -print0 | sort -z | xargs -0 sha256sum | sha256sum | cut -c1-16)"
  mkdir -p $D/proj && cd $D/proj && cargo init -q --name probe . >/dev/null 2>&1
  printf 'regex = "1"\nserde = { version = "1", features = ["derive"] }\nserde_json = "1"\n' >> Cargo.toml
  printf 'use serde::Serialize;\n#[derive(Serialize)] struct P { n: usize }\nfn main() { let r = regex::Regex::new("a+b").unwrap(); println!("{}", serde_json::to_string(&P { n: r.find_iter("aab ab b").count() }).unwrap()); }\n' > src/main.rs
  echo "cargo build $(CARGO_HOME=$D/cargo-home CARGO_TARGET_DIR=$D/proj/target t cargo build -q --release)"
  echo "run         $($D/proj/target/release/probe 2>&1)"
  echo "pip install $(t sh -c "python3 -m venv $D/venv && $D/venv/bin/pip install -q --disable-pip-version-check requests flask")"
  echo "files       $(find $D -type f | wc -l)"
  (cd $D && find . -type f | sort) > /out/files-$where.txt
  cd /
done
diff /out/files-ram.txt /out/files-slates.txt | head -20
slates --instance rw status > /out/status.txt 2>&1
grep -E "local_p50|local_p99|refused" /out/status.txt | head -12
