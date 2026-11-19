#!/usr/bin/env bash
# Makes slates-hg's part-payload golden vectors (A-545) with stock Mercurial 7.2, from PyPI through uv (nothing
# installed system-wide). Deterministic: no configuration but this script's, fixed user, dates 0 0 unless a marker
# names its own. Usage: make-payload-fixtures.sh OUTDIR
# Three pushes over SSH v1 to a non-publishing server, each captured by an ssh stand-in that runs the server's
# `hg serve --stdio` and records both directions: the bundle2 each client sent with `unbundle`, the server's reply cut
# where its stream ends, and the payload of the server's answer to the `listkeys phases` the client sends next.
#   push-modern: bookmarks, check:bookmarks, check:updated-heads, check:phases, obsmarkers, changegroup.
#   push-legacy: `devel.legacy.exchange=bookmarks,phases`, so bookmarks and phases travel as pushkey parts.
#   push-strict: the server set to `concurrent-push-mode = strict`, so the client sends check:heads.
# And two obsstores written by 7.2 in formats 0 and 1 (`format.obsstore-version`), with `hg debugobsolete` of each.
#
# Run to make the fixtures beside this script, on the Mac Studio (macOS 26.4, arm64), 2026-11-19, with uv's Mercurial
# 7.2 from PyPI on CPython 3.12, through kiln (sync off), from a clone of the branch, exactly:
#   OUT=$(mktemp -d ~/.cache/g291-payload.XXXXXX) && crates/hg/fixtures/payloads/make-payload-fixtures.sh "$OUT"
# Its first run cut each reply at the last byte the server wrote, so the next response's bytes trailed the stream
# (7.2's reader stops at the stream's end; slates-hg refuses the trailing bytes). Its second run, with the cut at the
# stream's end, made every other file again byte for byte; the files' SHA-256 were printed there and checked equal
# here after the copy. The obsstores are named obsstore-v0.bin and obsstore-v1.bin, so .gitattributes keeps their
# bytes.
set -euo pipefail
OUT=$(cd "$1" && pwd)
export HGPLAIN=1 HGRCPATH= HGENCODING=utf-8 PYTHONHASHSEED=0 TZ=UTC LC_ALL=C
WORK=$(mktemp -d "$HOME/.cache/g291-payfix.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
printf '#!/bin/sh\nexec uvx --quiet --python 3.12 --from mercurial==7.2 hg "$@"\n' > "$WORK/hg72"
chmod +x "$WORK/hg72"
hg() { "$WORK/hg72" --config ui.username=test --config devel.default-date='0 0' \
  --config experimental.evolution.createmarkers=true --config experimental.evolution.exchange=true "$@"; }
# A push of nothing but a bookmark or a phase may exit 1 ("no changes found"); the capture is checked by 7.2 below.
hgpush() { hg push "$@" || [ $? -eq 1 ]; }
cat > "$WORK/ssh.py" <<'PY'
import os, subprocess, sys, threading
command, capture = sys.argv[-1], os.environ["CAPTURE"]
child = subprocess.Popen(command, shell=True, stdin=subprocess.PIPE, stdout=subprocess.PIPE, bufsize=0)
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
into.start(); out.start(); out.join()
sys.exit(child.wait())
PY
cat > "$WORK/extract.py" <<'PY'
import struct, sys
capture, destination = sys.argv[1], sys.argv[2]
data = open(capture + ".client", "rb").read()
start = data.index(b"unbundle\nheads ") + len(b"unbundle\nheads ")
newline = data.index(b"\n", start)
position = newline + 1 + int(data[start:newline])
bundle = bytearray()
while True:
    newline = data.index(b"\n", position)
    size = int(data[position:newline])
    position = newline + 1
    if size == 0:
        break
    bundle += data[position:position + size]
    position += size
open(destination + ".hg", "wb").write(bytes(bundle))
# The client's next command after the bundle: 7.2 asks for the phases after every push (_pushsyncphase).
assert data[position:].startswith(b"listkeys\nnamespace 6\nphases"), data[position:position + 40]
# The server's reply is a bundle2 stream written unframed over SSH v1, and the next response follows it at once:
# walk the stream's frames to its end (a part header length of 0), refusing an interrupted part (none is expected).
reply = open(capture + ".server", "rb").read()
start = reply.rindex(b"HG20")
def length(at):
    return struct.unpack(">i", reply[at:at + 4])[0], at + 4
size, position = length(start + 4)
position += size
while True:
    size, position = length(position)
    if size == 0:
        break
    position += size
    while True:
        size, position = length(position)
        if size == 0:
            break
        assert size > 0, "an interrupted part in a reply"
        position += size
open(destination + "-reply.hg", "wb").write(reply[start:position])
# Then the listkeys response, framed as SSH v1 frames a reply: its length in decimal, a newline, the payload.
newline = reply.index(b"\n", position)
size = int(reply[position:newline])
payload = reply[newline + 1:newline + 1 + size]
assert len(payload) == size and newline + 1 + size == len(reply), (size, len(payload), len(reply))
open(destination + "-listkeys-phases.bin", "wb").write(payload)
PY
cd "$WORK"
hg version -q
hg init server
printf '[phases]\npublish = False\n[experimental]\nevolution.createmarkers = True\nevolution.exchange = True\n[ui]\nusername = test\n[devel]\ndefault-date = 0 0\n' > server/.hg/hgrc
(cd server && printf 'a\n' > a && hg add -q a && hg commit -m base && hg bookmark -r . main && hg phase -q --public -r .)
hg clone -q server client
SSH=(--config "ui.ssh=python3 -I $WORK/ssh.py" --config "ui.remotecmd=$WORK/hg72")
REMOTE="ssh://user@dummy/$WORK/server"
cd client
printf 'b\n' > b; hg add -q b; hg commit -m feature; hg bookmark feature
printf 'b2\n' > b; hg commit -q --amend -m 'feature, amended'
hg -R ../server debugpushkey ../server bookmarks > "$OUT/server-bookmarks-before.txt"
hg -R ../server debugpushkey ../server phases > "$OUT/server-phases-before.txt"
hg -R ../server debugpushkey ../server namespaces > "$OUT/server-namespaces.txt"
hg log --hidden -T '{rev} {node} {phase} {desc|firstline}\n' > "$OUT/client-log-1.txt"
CAPTURE="$WORK/modern" hgpush -q -B feature "${SSH[@]}" "$REMOTE"
python3 -I "$WORK/extract.py" "$WORK/modern" "$OUT/push-modern"
hg phase -q --public -r feature; hg bookmark -r feature other
CAPTURE="$WORK/legacy" hgpush -q --config devel.legacy.exchange=bookmarks,phases -B other "${SSH[@]}" "$REMOTE"
python3 -I "$WORK/extract.py" "$WORK/legacy" "$OUT/push-legacy"
printf '[server]\nconcurrent-push-mode = strict\n' >> ../server/.hg/hgrc
printf 'c\n' > c; hg add -q c; hg commit -m strict
CAPTURE="$WORK/strict" hgpush -q "${SSH[@]}" "$REMOTE"
python3 -I "$WORK/extract.py" "$WORK/strict" "$OUT/push-strict"
hg log --hidden -T '{rev} {node} {phase} {desc|firstline}\n' > "$OUT/client-log-2.txt"
cd "$WORK"
for f in "$OUT"/push-*.hg; do hg debugbundle --all "$f" > "$f.debug"; done
for version in 0 1; do
  hg init "obs$version"
  (cd "obs$version"
   o=(--config "format.obsstore-version=$version")
   printf 'x\n' > x; hg add -q x; hg commit -m one "${o[@]}"
   printf 'y\n' > x; hg commit -q --amend -m 'one, amended' "${o[@]}"
   printf 'z\n' > z; hg add -q z; hg commit -m 'to prune' "${o[@]}"
   hg debugobsolete -q --record-parents "$(hg log -r . -T '{node}')" "${o[@]}"
   hg debugobsolete -q --date '1234567890 -3600' -u alice 1111111111111111111111111111111111111111 \
     2222222222222222222222222222222222222222 3333333333333333333333333333333333333333 "${o[@]}"
   cp .hg/store/obsstore "$OUT/obsstore-v$version.bin"
   hg debugobsolete --hidden > "$OUT/obsstore-v$version.debug")
done
ls -la "$OUT"
