#!/usr/bin/env bash
# Makes slates-hg's wire-protocol golden vectors (A-569) with stock Mercurial 7.2, from PyPI through uv (nothing
# installed system-wide). No configuration but this script's, a fixed user, dates 0 0. Usage: make-wire-fixtures.sh OUT
#
# SSH version 1: each session runs through an ssh stand-in that starts the server's `hg serve --stdio` and records what
# the client wrote (.client), and what the server wrote on stdout (.server) and on stderr (.stderr):
#   ssh-clone; ssh-pull (discovery's `batch`); ssh-push (`unbundle` and its chunked input); ssh-wireargs
#   (`debugwireargs`: named arguments and the dictionary); ssh-oob (a bundle1 client refused by a server that requires
#   bundle2: the out-of-band error form).
# HTTP version 1: `hg serve` behind a recording proxy; each exchange is written apart as NN.request (the bytes the client
# sent), NN.response-head and NN.response-body (de-chunked):
#   http-clone (X-HgArg headers, X-HgProto, the 0.2 media type); http-none (the same, `server.compressionengines=none`,
#   so the 0.2 payload is a plain bundle2 stream); http-push; http-wireargs; http-postargs (`X-HgArgs-Post`,
#   `experimental.httppostargs`); http-query (`server.maxhttpheaderlen=0`: arguments in the query string); http-oob.
# The HTTP captures carry the run's ports (Host) and dates (Date): those headers differ from run to run.
set -euo pipefail
OUT=$(cd "$1" && pwd)
export HGPLAIN=1 HGRCPATH= HGENCODING=utf-8 PYTHONHASHSEED=0 TZ=UTC LC_ALL=C
WORK=$(mktemp -d "$HOME/.cache/g291-wirefix.XXXXXX")
PIDS=()
cleanup() { for pid in "${PIDS[@]:-}"; do [ -n "$pid" ] && kill "$pid" 2>/dev/null || true; done; rm -rf "$WORK"; }
trap cleanup EXIT
printf '#!/bin/sh\nexec uvx --quiet --python 3.12 --from mercurial==7.2 hg "$@"\n' > "$WORK/hg72"
chmod +x "$WORK/hg72"
hg() { "$WORK/hg72" --config ui.username=test --config devel.default-date='0 0' "$@"; }
cat > "$WORK/ssh.py" <<'PY'
import os, subprocess, sys, threading
command, capture = sys.argv[-1], os.environ["CAPTURE"]
child = subprocess.Popen(command, shell=True, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                         bufsize=0)
def pump(source, target, path, close):
    with open(path, "ab") as log:
        while True:
            data = os.read(source, 65536)
            if not data:
                break
            log.write(data)
            log.flush()
            view = memoryview(data)
            while view:
                view = view[os.write(target, view):]
    if close:
        os.close(target)
into = threading.Thread(target=pump, args=(0, child.stdin.fileno(), capture + ".client", True), daemon=True)
out = threading.Thread(target=pump, args=(child.stdout.fileno(), 1, capture + ".server", False))
err = threading.Thread(target=pump, args=(child.stderr.fileno(), 2, capture + ".stderr", False))
into.start(); out.start(); err.start(); out.join(); err.join()
sys.exit(child.wait())
PY
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
# Starts `hg serve` on the server repository with the given configuration, and the recording proxy before it.
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
THREE='a b=c&d+e%f;g,h:i'
FOUR=$(printf 'caf\303\251')
cd "$WORK"
hg version -q
hg init server
(cd server && printf 'a\n' > a && hg add -q a && hg commit -m base && hg bookmark -r . main)
SSH=(--config "ui.ssh=python3 -I $WORK/ssh.py" --config "ui.remotecmd=$WORK/hg72")
REMOTE="ssh://user@dummy/$WORK/server"
CAPTURE="$WORK/ssh-clone" hg clone -q "${SSH[@]}" "$REMOTE" sshclient
(cd server && printf 'b\n' > b && hg add -q b && hg commit -m second)
(cd sshclient && CAPTURE="$WORK/ssh-pull" hg pull -q "${SSH[@]}" "$REMOTE")
(cd sshclient && printf 'c\n' > c && hg add -q c && hg commit -m third)
(cd sshclient && CAPTURE="$WORK/ssh-push" hg push -q "${SSH[@]}" "$REMOTE")
CAPTURE="$WORK/ssh-wireargs" hg debugwireargs "${SSH[@]}" "$REMOTE" un deux --three "$THREE" --four "$FOUR" \
  > "$OUT/ssh-wireargs.answer"
printf '[server]\nbundle1 = False\n' >> server/.hg/hgrc
if CAPTURE="$WORK/ssh-oob" hg clone -q "${SSH[@]}" --config devel.legacy.exchange=bundle1 "$REMOTE" oobclient \
  2> "$OUT/ssh-oob.client-stderr"; then echo "ssh-oob: the bundle1 clone was not refused" >&2; exit 1; fi
for session in ssh-clone ssh-pull ssh-push ssh-wireargs ssh-oob; do
  for side in client server stderr; do touch "$WORK/$session.$side"; cp "$WORK/$session.$side" "$OUT/"; done
done
serve http-clone
hg clone -q "$URL" httpclient
stop http-clone
serve http-none --config server.compressionengines=none
hg clone -q "$URL" noneclient
stop http-none
serve http-push --config web.push_ssl=False --config 'web.allow-push=*'
(cd httpclient && printf 'd\n' > d && hg add -q d && hg commit -m fourth && hg push -q "$URL")
stop http-push
serve http-wireargs
hg debugwireargs "$URL" un deux --three "$THREE" --four "$FOUR" > "$OUT/http-wireargs.answer"
stop http-wireargs
serve http-postargs --config experimental.httppostargs=True
hg debugwireargs "$URL" un deux --three "$THREE" --four "$FOUR" > "$OUT/http-postargs.answer"
stop http-postargs
serve http-query --config server.maxhttpheaderlen=0
hg debugwireargs "$URL" un deux --three "$THREE" --four "$FOUR" > "$OUT/http-query.answer"
stop http-query
serve http-oob
if hg clone -q --config devel.legacy.exchange=bundle1 "$URL" oobhttp 2> "$OUT/http-oob.client-stderr"; then
  echo "http-oob: the bundle1 clone was not refused" >&2; exit 1
fi
stop http-oob
hg -R server log -T '{rev} {node} {phase} {desc}\n' > "$OUT/server-log.txt"
ls -la "$OUT"
