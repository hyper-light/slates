#!/bin/bash
# Real-world competition benchmark + hold-up-under-real-use check: real developer workloads on a slates NFSv4.2
# volume (Linux's own client) beside tmpfs and JuiceFS (the userspace/distributed competitor: FUSE, SQLite
# metadata, a local file object store). The workloads — a shallow git clone + `git fsck` + a tree hash, a cargo
# build with real crates.io dependencies, a pip install into a venv — are the everyday things a coding agent does,
# so "holds up" means: every one exits 0, the clone's tree hash matches tmpfs's byte for byte, the built binary
# runs, and the file list matches. Performance is the wall-clock of each, all three arms side by side.
#
# Run (privileged for the NFS mount and /dev/fuse; repo read-only at /work, an output dir at /out, a cache volume
# at /ccache so a rebuild and the dep downloads are not repeated):
#   docker run --rm --privileged -v "$PWD":/work:ro -v <out>:/out -v slates-cc:/ccache rust:1.98.0 \
#     bash /work/docs/wip/bench/realworld_compete.sh
set -u
mkdir -p /ccache/cargo /out
export CARGO_HOME=/ccache/cargo
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"

echo "## setup $(date -u +%FT%TZ)"
apt-get update -qq >/dev/null 2>&1
apt-get install -y -qq nfs-common python3-venv python3-pip git fuse3 sqlite3 ca-certificates curl >/dev/null 2>&1 \
  || echo "WARN apt-get had failures"

# --- Build the Linux release of slates from the read-only repo into the cache target. ---
echo "## build slates (release) — first run compiles deps, later runs are incremental"
BUILD_START=$(date +%s)
( cd /work && RUSTUP_TOOLCHAIN=1.98.0 CARGO_TARGET_DIR=/ccache/target \
    cargo build --release --locked -p slates-cli ) >/out/build.log 2>&1 || {
  echo "BUILD FAILED — tail:"; tail -20 /out/build.log; exit 1; }
cp /ccache/target/release/slates /usr/local/bin/slates
echo "## build done in $(( $(date +%s) - BUILD_START ))s; slates $(slates --help >/dev/null 2>&1 && echo ok)"

# --- Install JuiceFS (the competitor). ---
if ! command -v juicefs >/dev/null 2>&1; then
  curl -sSL https://d.juicefs.com/install 2>/out/jfs-install.log | sh - >>/out/jfs-install.log 2>&1 \
    || echo "WARN juicefs install failed (arm will be skipped)"
fi
echo "## juicefs $(juicefs version 2>/dev/null || echo MISSING)"

# --- slates: anchor, a dynamic volume, an NFS export, the kernel client mount. ---
slates --instance rw anchor --quick --shards 4 >/out/anchor.out 2>/out/anchor.err &
for _ in $(seq 1 200); do slates --instance rw volume list >/dev/null 2>&1 && break; sleep 0.1; done
slates --instance rw bootstrap root >/dev/null 2>&1
ID=$(slates --instance rw volume create rw --dynamic 4GiB 2>&1 | awk '/^id/{print $2}')
[ -n "$ID" ] || { echo "slates volume create failed"; }
slates --instance rw export "$ID" > /out/export.txt 2>&1
PORT=$(awk '/^port:/{print $2}' /out/export.txt); EXPORT=$(awk '/^export:/{print $2}' /out/export.txt)

mkdir -p /mnt/slates /mnt/ram /mnt/jfs
trap "umount -f -l /mnt/slates 2>/dev/null; umount /mnt/ram 2>/dev/null; juicefs umount /mnt/jfs 2>/dev/null; umount -f -l /mnt/jfs 2>/dev/null" EXIT
mount -t nfs -o "vers=4.2,proto=tcp,port=${PORT:-0},hard,timeo=600" "127.0.0.1:${EXPORT:-/}" /mnt/slates \
  && echo "## slates mounted" || echo "## slates mount FAILED"
mount -t tmpfs -o size=4g tmpfs /mnt/ram && echo "## tmpfs mounted"

# --- JuiceFS: SQLite metadata + a local file object store, mounted through FUSE. ---
if command -v juicefs >/dev/null 2>&1; then
  rm -f /ccache/jfs-meta.db; rm -rf /ccache/jfs-store; mkdir -p /ccache/jfs-store
  juicefs format --storage file --bucket /ccache/jfs-store "sqlite3:///ccache/jfs-meta.db" rwjfs \
    >/out/jfs-format.log 2>&1 || echo "WARN juicefs format failed"
  juicefs mount "sqlite3:///ccache/jfs-meta.db" /mnt/jfs --background >/out/jfs-mount.log 2>&1 || echo "WARN juicefs mount failed"
  for _ in $(seq 1 50); do mountpoint -q /mnt/jfs && break; sleep 0.1; done
  mountpoint -q /mnt/jfs && echo "## juicefs mounted" || echo "## juicefs mount FAILED"
fi

# Time a command: wall seconds and exit code.
t() { local s c; s=$(date +%s.%N); "$@" >/dev/null 2>&1; c=$?; printf '%ss exit=%s' "$(python3 -c "print(round($(date +%s.%N)-$s,2))")" "$c"; }

MAINRS='use serde::Serialize;
#[derive(Serialize)] struct P { n: usize }
fn main() { let r = regex::Regex::new("a+b").unwrap(); println!("{}", serde_json::to_string(&P { n: r.find_iter("aab ab b").count() }).unwrap()); }'

echo; echo "## workloads (git clone+fsck+treehash, cargo build w/ deps, pip install) per arm"
for where in ram slates jfs; do
  D=/mnt/$where
  mountpoint -q "$D" || { echo "== $where: SKIP (not mounted)"; continue; }
  echo "== $where"
  echo "  git clone   $(t git clone -q --depth 1 https://github.com/BurntSushi/ripgrep $D/rg)"
  echo "  git fsck    $(t git -C $D/rg fsck --full)"
  echo "  tree hash   $(cd "$D/rg" 2>/dev/null && find . -path ./.git -prune -o -type f -print0 | sort -z | xargs -0 sha256sum 2>/dev/null | sha256sum | cut -c1-16)"
  mkdir -p "$D/proj" && ( cd "$D/proj" && cargo init -q --name probe . >/dev/null 2>&1
    printf 'regex = "1"\nserde = { version = "1", features = ["derive"] }\nserde_json = "1"\n' >> Cargo.toml
    printf '%s\n' "$MAINRS" > src/main.rs )
  echo "  cargo build $(CARGO_TARGET_DIR=$D/proj/target t cargo build -q --release --manifest-path $D/proj/Cargo.toml)"
  echo "  run         $("$D/proj/target/release/probe" 2>&1 || echo FAILED)"
  echo "  pip install $(t sh -c "python3 -m venv $D/venv && $D/venv/bin/pip install -q --disable-pip-version-check requests flask")"
  echo "  files       $(find "$D" -type f 2>/dev/null | wc -l)"
  ( cd "$D" 2>/dev/null && find . -path ./rg/.git -prune -o -type f -print 2>/dev/null | sort ) > "/out/files-$where.txt"
  cd /
done

echo; echo "## correctness: slates vs tmpfs file-list diff (empty = identical)"
diff /out/files-ram.txt /out/files-slates.txt | head -20 || true
echo "## correctness: juicefs vs tmpfs file-list diff"
[ -f /out/files-jfs.txt ] && { diff /out/files-ram.txt /out/files-jfs.txt | head -20 || true; }
echo; echo "## slates status (p50/p99/refusals)"; slates --instance rw status 2>/dev/null | grep -E "local_p50|local_p99|refused|bytes" | head -12
echo "## done $(date -u +%FT%TZ)"
