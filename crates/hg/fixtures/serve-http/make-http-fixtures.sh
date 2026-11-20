#!/usr/bin/env bash
# Makes slates-hg's HTTP read-serving golden vectors (A-650 part 1) with stock Mercurial 7.2, from PyPI through uv
# (nothing installed system-wide). The repository and its three changes are fixtures/serve/make-serve-fixtures.sh's,
# made the same way (no configuration but this script's, a fixed user, every date 0 0), so its nodes are the SSH
# fixtures' own and the states this script dumps equal fixtures/serve's byte for byte; the clients reach it over HTTP
# instead: stock `hg serve` behind a recording proxy (make-wire-fixtures.sh's), each exchange written apart as
# http-<session>.NN.request (the bytes the client sent), .response-head and .response-body (de-chunked).
# Usage: make-http-fixtures.sh OUTDIR
#   http-clone, then http-pull after a public change under a bookmark, then http-incoming after a draft one; the
#   server's state before each in state-<when>.txt and bookmarks-<when>.txt (as fixtures/serve dumps them),
#   incoming.txt (the incoming's output), and SHA256SUMS of them all. The captures carry the run's port (Host) and
#   date (Date), which differ from run to run.
set -euo pipefail
OUT=$(cd "$1" && pwd)
export HGPLAIN=1 HGRCPATH= HGENCODING=utf-8 PYTHONHASHSEED=0 TZ=UTC LC_ALL=C
WORK=$(mktemp -d "$HOME/.cache/g303-hghttp.XXXXXX")
PIDS=()
cleanup() { for pid in "${PIDS[@]:-}"; do [ -n "$pid" ] && kill "$pid" 2>/dev/null || true; done; rm -rf "$WORK"; }
trap cleanup EXIT
printf '#!/bin/sh\nexec uvx --quiet --python 3.12 --from mercurial==7.2 hg "$@"\n' > "$WORK/hg72"
chmod +x "$WORK/hg72"
hg() { "$WORK/hg72" --config ui.username=test --config devel.default-date='0 0' \
  --config experimental.evolution.createmarkers=true --config experimental.evolution.exchange=true "$@"; }
cat > "$WORK/proxy.py" <<'PY'
import socket, sys, threading
target, prefix, portfile = int(sys.argv[1]), sys.argv[2], sys.argv[3]
listener = socket.socket()
listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
listener.bind(("127.0.0.1", 0))
listener.listen(16)
open(portfile, "w").write(str(listener.getsockname()[1]))
def pump(source, target_socket, path):
    with open(path, "ab") as log:
        while True:
            data = source.recv(65536)
            if not data:
                break
            log.write(data)
            log.flush()
            target_socket.sendall(data)
    try:
        target_socket.shutdown(socket.SHUT_WR)
    except OSError:
        pass
count = 0
while True:
    client, _ = listener.accept()
    count += 1
    server = socket.create_connection(("127.0.0.1", target))
    for source, sink, side in ((client, server, "request"), (server, client, "response")):
        threading.Thread(target=pump, args=(source, sink, "%s.%03d.%s" % (prefix, count, side)), daemon=True).start()
PY
cat > "$WORK/split.py" <<'PY'
import glob, os, sys
prefix, destination = sys.argv[1], sys.argv[2]
def fields(head):
    lines = head.split(b"\r\n")
    found = {}
    for line in lines[1:]:
        name, value = line.split(b":", 1)
        found[name.strip().lower()] = value.strip()
    return found
def requests(data):
    position = 0
    while position < len(data):
        end = data.index(b"\r\n\r\n", position) + 4
        length = int(fields(data[position:end - 4]).get(b"content-length", b"0"))
        yield data[position:end + length]
        position = end + length
def dechunk(data, position):
    body = bytearray()
    while True:
        line_end = data.index(b"\r\n", position)
        size = int(data[position:line_end].split(b";")[0], 16)
        position = line_end + 2
        if size == 0:
            return bytes(body), data.index(b"\r\n", position) + 2
        body += data[position:position + size]
        position += size + 2
def responses(data):
    position = 0
    while position < len(data):
        end = data.index(b"\r\n\r\n", position) + 4
        head = fields(data[position:end - 4])
        if head.get(b"transfer-encoding", b"").lower() == b"chunked":
            body, next_position = dechunk(data, end)
        elif b"content-length" in head:
            length = int(head[b"content-length"])
            body, next_position = data[end:end + length], end + length
        else:
            body, next_position = data[end:], len(data)
        yield data[position:end], body
        position = next_position
count = 0
for request_path in sorted(glob.glob(prefix + ".*.request")):
    response_path = request_path[:-len("request")] + "response"
    sent = list(requests(open(request_path, "rb").read()))
    answered = list(responses(open(response_path, "rb").read())) if os.path.exists(response_path) else []
    assert len(sent) == len(answered), (request_path, len(sent), len(answered))
    for request, (head, body) in zip(sent, answered):
        count += 1
        name = "%s.%02d" % (destination, count)
        open(name + ".request", "wb").write(request)
        open(name + ".response-head", "wb").write(head)
        open(name + ".response-body", "wb").write(body)
print(destination.rsplit("/", 1)[-1], count, "exchanges")
PY
free_port() { python3 -I -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'; }
wait_port() { python3 -I -c '
import socket, sys, time
for _ in range(600):
    try:
        socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=1).close()
        sys.exit(0)
    except OSError:
        time.sleep(0.1)
sys.exit(1)' "$1"; }
# Starts `hg serve` on the server repository and the recording proxy before it; URL is the proxy's.
serve() {
  local name=$1; shift
  SERVE_PORT=$(free_port)
  hg -R "$WORK/server" serve -a 127.0.0.1 -p "$SERVE_PORT" -d --pid-file "$WORK/serve.pid" "$@"
  PIDS+=("$(cat "$WORK/serve.pid")")
  wait_port "$SERVE_PORT"
  rm -f "$WORK/proxy.port"
  python3 -I "$WORK/proxy.py" "$SERVE_PORT" "$WORK/$name" "$WORK/proxy.port" &
  PROXY_PID=$!
  PIDS+=("$PROXY_PID")
  while [ ! -s "$WORK/proxy.port" ]; do sleep 0.1; done
  URL="http://127.0.0.1:$(cat "$WORK/proxy.port")/"
}
stop() {
  local name=$1
  kill "$PROXY_PID" "$(cat "$WORK/serve.pid")" 2>/dev/null || true
  wait "$PROXY_PID" 2>/dev/null || true
  python3 -I "$WORK/split.py" "$WORK/$name" "$OUT/$name"
}
dump() {
  hg -R "$WORK/server" log -r 'all()' \
    -T '{node} {p1node} {p2node} {phase} {ifeq(get(extras, "close"), "1", "1", "0")}|{branch}\n' > "$OUT/state-$1.txt"
  hg -R "$WORK/server" bookmarks -T '{node} {bookmark}\n' > "$OUT/bookmarks-$1.txt"
}
cd "$WORK"
hg version -q
hg init server
printf '[phases]\npublish = False\n[experimental]\nevolution.createmarkers = true\nevolution.exchange = true\n' >> server/.hg/hgrc
cd server
printf 'a\n' > a; hg add -q a; hg commit -m 'add a'
printf 'a\nb\n' > a; hg commit -m 'edit a'
hg branch -q stable; printf 's\n' > s; hg add -q s; hg commit -m 'on stable'
hg update -q default; printf 'c\n' > c; hg add -q c; hg commit -m 'c on default'
hg bookmark -r . main
hg branch -q 'old feature/x'; printf 'x\n' > x; hg add -q x; hg commit -m 'feature x'
hg commit -q --close-branch -m 'close feature x'
hg update -q default; hg merge -q stable; hg commit -m 'merge stable'
hg phase -q --public -r 'ancestors(.)'
printf 'd\n' > d; hg add -q d; hg commit -m 'draft d'
printf 'd\ne\n' > d; hg commit -m 'draft e'
printf 'd\ne, amended\n' > d; hg commit -q --amend -m 'draft e, amended'
hg bookmark -r . topic
hg update -q stable; printf 's\nt\n' > s; hg commit -m 'stable t'
cd "$WORK"
dump clone
serve http-clone
hg clone -q -U "$URL" client
stop http-clone
(cd server && hg update -q main && printf 'c\nf\n' > c && hg commit -q -m 'f on main' && hg phase -q --public -r .)
dump pull
serve http-pull
(cd client && hg pull -q "$URL")
stop http-pull
(cd server && hg update -q topic && printf 'g\n' > g && hg add -q g && hg commit -q -m 'draft g')
dump incoming
serve http-incoming
(cd client && hg incoming -q "$URL" > "$OUT/incoming.txt")
stop http-incoming
cd "$OUT"
shasum -a 256 state-*.txt bookmarks-*.txt http-* incoming.txt > SHA256SUMS
cat SHA256SUMS
