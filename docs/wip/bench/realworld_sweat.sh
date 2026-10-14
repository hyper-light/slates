#!/bin/bash
# "Make it sweat": heavy real-world workloads on a slates NFSv4.2 volume (Linux kernel client) vs tmpfs vs JuiceFS,
# to prove slates holds up under genuinely demanding use, not toy trees. Each workload is timed (wall-clock) and
# checked for correctness, then its output is removed before the next so peak memory stays bounded to one
# workload's footprint. Three arms side by side.
#
#   1. Clone the Linux kernel (shallow) — ~90k files, the canonical huge tree; `git fsck --full` must pass.
#   2. Build CPython from source — ./configure + make -j; the built interpreter must run and compute correctly.
#   3. A massive pip data-science stack — numpy, pandas, scipy, scikit-learn, matplotlib; every import must work.
#   4. A big npm toolchain — webpack, babel, react, typescript, eslint, jest, express; `npm install` must exit 0.
#
# Run (privileged for the NFS mount and /dev/fuse; repo read-only at /work, out at /out, cache volume at /ccache):
#   docker run --rm --privileged --memory=24g -v "$PWD":/work:ro -v <out>:/out -v slates-cc:/ccache rust:1.98.0 \
#     bash /work/docs/wip/bench/realworld_sweat.sh
set -u
mkdir -p /ccache/cargo /out
export CARGO_HOME=/ccache/cargo CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"
NPROC=$(nproc)

echo "## setup $(date -u +%FT%TZ)  nproc=$NPROC"
apt-get update -qq >/dev/null 2>&1
# cpython build deps + node + the mount/competitor tooling.
apt-get install -y -qq nfs-common python3-venv python3-pip git fuse3 sqlite3 ca-certificates curl \
  build-essential pkg-config libssl-dev zlib1g-dev libffi-dev libbz2-dev libreadline-dev libsqlite3-dev \
  nodejs npm >/dev/null 2>&1 || echo "WARN apt-get had failures"
echo "## node $(node --version 2>/dev/null) npm $(npm --version 2>/dev/null)"

# --- Build the Linux release of slates (cached). ---
BUILD_START=$(date +%s)
( cd /work && RUSTUP_TOOLCHAIN=1.98.0 CARGO_TARGET_DIR=/ccache/target cargo build --release --locked -p slates-cli ) \
  >/out/build.log 2>&1 || { echo "BUILD FAILED"; tail -20 /out/build.log; exit 1; }
cp /ccache/target/release/slates /usr/local/bin/slates
echo "## slates built in $(( $(date +%s) - BUILD_START ))s"

# --- JuiceFS. ---
command -v juicefs >/dev/null 2>&1 || curl -sSL https://d.juicefs.com/install 2>/dev/null | sh - >/out/jfs-install.log 2>&1
echo "## juicefs $(juicefs version 2>/dev/null || echo MISSING)"

# --- slates: anchor, a large dynamic volume, NFS export + mount. ---
slates --instance rw anchor --quick --shards 4 >/out/anchor.out 2>/out/anchor.err &
for _ in $(seq 1 200); do slates --instance rw volume list >/dev/null 2>&1 && break; sleep 0.1; done
slates --instance rw bootstrap root >/dev/null 2>&1
ID=$(slates --instance rw volume create rw --dynamic 12GiB 2>&1 | awk '/^id/{print $2}')
slates --instance rw export "$ID" > /out/export.txt 2>&1
PORT=$(awk '/^port:/{print $2}' /out/export.txt); EXPORT=$(awk '/^export:/{print $2}' /out/export.txt)
mkdir -p /mnt/slates /mnt/ram /mnt/jfs
trap "umount -f -l /mnt/slates 2>/dev/null; umount /mnt/ram 2>/dev/null; juicefs umount /mnt/jfs 2>/dev/null; umount -f -l /mnt/jfs 2>/dev/null" EXIT
mount -t nfs -o "vers=4.2,proto=tcp,port=${PORT:-0},hard,timeo=600" "127.0.0.1:${EXPORT:-/}" /mnt/slates \
  && echo "## slates mounted" || echo "## slates mount FAILED"
mount -t tmpfs -o size=14g tmpfs /mnt/ram && echo "## tmpfs mounted"
if command -v juicefs >/dev/null 2>&1; then
  rm -f /ccache/jfs-meta.db; rm -rf /ccache/jfs-store; mkdir -p /ccache/jfs-store
  juicefs format --storage file --bucket /ccache/jfs-store "sqlite3:///ccache/jfs-meta.db" rwjfs >/out/jfs-format.log 2>&1
  juicefs mount "sqlite3:///ccache/jfs-meta.db" /mnt/jfs --background >/out/jfs-mount.log 2>&1
  for _ in $(seq 1 50); do mountpoint -q /mnt/jfs && break; sleep 0.1; done
  mountpoint -q /mnt/jfs && echo "## juicefs mounted" || echo "## juicefs mount FAILED"
fi

t() { local s c; s=$(date +%s.%N); "$@" >/dev/null 2>&1; c=$?; printf '%ss exit=%s' "$(python3 -c "print(round($(date +%s.%N)-$s,1))")" "$c"; }

echo; echo "## HEAVY WORKLOADS (per workload: time exit, correctness, then cleanup) ============================"
for where in ram slates jfs; do
  D=/mnt/$where
  mountpoint -q "$D" || { echo "== $where: SKIP (not mounted)"; continue; }
  echo "== $where ======================================================================"

  # 1. Linux kernel — the canonical huge tree.
  echo "  [linux] clone     $(t git clone -q --depth 1 https://github.com/torvalds/linux "$D/linux")"
  echo "  [linux] fsck      $(t git -C "$D/linux" fsck --full)"
  echo "  [linux] files     $(find "$D/linux" -type f 2>/dev/null | wc -l)   head $(git -C "$D/linux" rev-parse --short HEAD 2>/dev/null)"
  echo "  [linux] grep -r   $(t sh -c "grep -rI EXPORT_SYMBOL_GPL $D/linux/kernel | wc -l")"
  rm -rf "$D/linux"

  # 2. Build CPython from source.
  echo "  [cpython] clone   $(t git clone -q --depth 1 https://github.com/python/cpython "$D/cpython")"
  echo "  [cpython] config  $(t sh -c "cd $D/cpython && ./configure -q --with-ensurepip=no")"
  echo "  [cpython] make-j$NPROC $(t sh -c "cd $D/cpython && make -s -j$NPROC")"
  echo "  [cpython] runs    $("$D/cpython/python" -c 'print("cpython-ok", sum(range(10000)))' 2>&1 || echo FAILED)"
  rm -rf "$D/cpython"

  # 3. Massive pip data-science stack.
  python3 -m venv "$D/venv" >/dev/null 2>&1
  echo "  [pip] install     $(t "$D/venv/bin/pip" install -q --disable-pip-version-check numpy pandas scipy scikit-learn matplotlib)"
  echo "  [pip] import      $("$D/venv/bin/python" -c 'import numpy,pandas,scipy,sklearn,matplotlib; print("imports-ok", numpy.__version__)' 2>&1 || echo FAILED)"
  echo "  [pip] files       $(find "$D/venv" -type f 2>/dev/null | wc -l)"
  rm -rf "$D/venv"

  # 4. Big npm toolchain.
  mkdir -p "$D/node"
  cat > "$D/node/package.json" <<'JSON'
{ "name": "sweat", "version": "1.0.0", "private": true,
  "dependencies": { "webpack": "^5", "webpack-cli": "^5", "@babel/core": "^7", "@babel/preset-env": "^7",
    "react": "^18", "react-dom": "^18", "typescript": "^5", "eslint": "^8", "jest": "^29",
    "express": "^4", "lodash": "^4", "axios": "^1" } }
JSON
  echo "  [npm] install     $(t npm install --prefix "$D/node" --no-audit --no-fund --loglevel=error)"
  echo "  [npm] modules     $(find "$D/node/node_modules" -type f 2>/dev/null | wc -l) files"
  rm -rf "$D/node"
  echo
done

echo "## slates status"; slates --instance rw status 2>/dev/null | grep -E "local_p50|local_p99|refused|bytes_in_use|clients_refused" | head -8
echo "## done $(date -u +%FT%TZ)"
