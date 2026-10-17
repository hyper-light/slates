#!/bin/sh
# End to end: a slates VFS applied inside a shards microVM, over slates' NFS transport (§4.6).
# macOS arm64 + Hypervisor.framework (proven: `shards run --rm alpine echo ...` boots a microVM here).
# This mounts a RAM-only slates volume inside one and reads a seeded file back. DAX is NOT available
# over NFS — see README.md for the vhost-user-fs frontend shards would need for DAX.
set -eu
SLATES="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$SLATES"
FWD_PID=""

echo "==> start the slates NFS server (a RAM-only volume with a seeded hello.txt)"
LOG="$(mktemp -t slates-nfs.XXXXXX)"
RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-1.98.0}" \
  cargo run -q -p slates-bridge-nfs --example nfs_async >"$LOG" 2>&1 &
NFS_PID=$!
trap 'kill "$NFS_PID" ${FWD_PID:+"$FWD_PID"} 2>/dev/null || true; rm -f "$LOG"' EXIT

echo "==> wait for its loopback port"
PORT=""
for _ in $(seq 1 120); do
  PORT="$(grep -oiE 'port[^0-9]{0,3}[0-9]{3,5}' "$LOG" | grep -oE '[0-9]{3,5}' | head -1 || true)"
  [ -n "$PORT" ] && break
  kill -0 "$NFS_PID" 2>/dev/null || { echo "nfs server exited; log:"; cat "$LOG"; exit 1; }
  sleep 1
done
[ -n "$PORT" ] || { echo "no port found; log:"; cat "$LOG"; exit 1; }
echo "    slates NFS on 127.0.0.1:$PORT"

echo "==> forward 0.0.0.0:FWD -> 127.0.0.1:$PORT (slates binds loopback only; the guest reaches the"
echo "    host at the shards gateway, so bridge the two — socat is absent here, python3 is)"
FWD=$((PORT + 1))
# The forwarder body, inline so the script is self-contained:
python3 - "$FWD" "$PORT" <<'PY' >/dev/null 2>&1 &
import socket, sys, threading
fwd, dst = int(sys.argv[1]), int(sys.argv[2])
srv = socket.socket(); srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("0.0.0.0", fwd)); srv.listen(64)
def pipe(a, b):
    try:
        while True:
            d = a.recv(65536)
            if not d: break
            b.sendall(d)
    finally:
        for s in (a, b):
            try: s.shutdown(socket.SHUT_RDWR)
            except OSError: pass
while True:
    c, _ = srv.accept()
    try:
        u = socket.create_connection(("127.0.0.1", dst))
    except OSError:
        c.close(); continue
    threading.Thread(target=pipe, args=(c, u), daemon=True).start()
    threading.Thread(target=pipe, args=(u, c), daemon=True).start()
PY
FWD_PID=$!
sleep 1
echo "    forwarder on 0.0.0.0:$FWD"

echo "==> boot a shards microVM (HVF) that mounts the slates volume and reads hello.txt"
shards run --rm --user root -e SLATES_PORT="$FWD" alpine:3.22 /bin/sh -c '
  set -e
  apk add --no-cache nfs-utils >/dev/null 2>&1 || true
  gw=$(ip route | awk "/default/{print \$3; exit}")
  mkdir -p /slates
  mount -t nfs -o "vers=3,tcp,port=${SLATES_PORT},mountport=${SLATES_PORT},nolock,soft,ro" "${gw}:/" /slates
  echo "--- slates VFS mounted in a shards microVM (gateway ${gw}, port ${SLATES_PORT}) ---"
  ls -la /slates
  printf "hello.txt: "; cat /slates/hello.txt; echo
'
echo "==> done — a slates VFS was applied inside a shards microVM"
