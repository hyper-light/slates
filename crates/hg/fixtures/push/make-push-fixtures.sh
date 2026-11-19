#!/usr/bin/env bash
# Makes slates-hg's push golden vectors (A-641; §4.17.3 "Push", piece 5) with stock Mercurial 7.2 from PyPI through
# uv (nothing installed system-wide). Deterministic: no configuration but this script's, user `test`, dates `0 0`.
# Usage: make-push-fixtures.sh OUTDIR
#
# Every push goes over SSH version 1 to a non-publishing `hg serve --stdio`, through an ssh stand-in that records both
# directions. For each push NAME: NAME.hg is the bundle2 the client sent with `unbundle`; NAME-reply.hg the server's
# answer, cut where its stream ends; NAME-server.txt the server's changesets just before it took the push
# (`{rev} {node} {p1} {p2} {phase} {branch}`, revision order) and NAME-bookmarks.txt its bookmarks (`{name} {node}`);
# NAME.hg.debug and NAME-reply.hg.debug are 7.2's `hg debugbundle --all` of each.
#   push-bookmark: a bookmark moved onto a changeset the server holds (check:bookmarks, check:phases, bookmarks).
#   push-phase: a draft published (check:phases, phase-heads).
#   push-plain: one changeset on the one head (check:updated-heads, check:phases, changegroup): reply:changegroup.
#   push-race: push-plain's session sent again after another client's push moved that head: error:pushraced.
#   push-bookmark-plain, push-race-bookmark: a changeset and a bookmark moved onto it; the session sent again after
#     another client moved the bookmark onto a changeset of another branch, so only the bookmark's check fails.
#   push-phase-plain, push-race-phase: a changeset on a draft; the session sent again after another client published
#     that draft, so only the phases' check fails.
#   push-newhead: a second head on default (`push --force`, so no race check travels) to a server whose pretxnclose
#     runs 7.2's own hooklib.reject_new_heads: error:abort in that hook's words.
#   push-unsupported-part, push-unsupported-param: push-plain's bundle with an unknown mandatory part added, or its
#     changegroup part given an unknown mandatory parameter, sent by hand: error:unsupportedcontent.
#
# Run to make the fixtures beside this script, on the Mac Studio (macOS 26.4, arm64), with uv's Mercurial 7.2 from
# PyPI on CPython 3.12, through kiln (sync off), from a clone of the branch:
#   OUT=$(mktemp -d ~/.cache/g301-push.XXXXXX) && crates/hg/fixtures/push/make-push-fixtures.sh "$OUT"
set -euo pipefail
OUT=$(cd "$1" && pwd)
export HGPLAIN=1 HGRCPATH= HGENCODING=utf-8 PYTHONHASHSEED=0 TZ=UTC LC_ALL=C
WORK=$(mktemp -d "$HOME/.cache/g301-pushfix.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
printf '#!/bin/sh\nexec uvx --quiet --python 3.12 --from mercurial==7.2 hg "$@"\n' > "$WORK/hg72"
chmod +x "$WORK/hg72"
hg() { "$WORK/hg72" --config ui.username=test --config devel.default-date='0 0' "$@"; }
# A push of nothing but a bookmark or a phase exits 1 ("no changes found"); any other failure stops the script.
hgpush() {
  local code=0
  hg push "$@" || code=$?
  case $code in 0 | 1) ;; *) return "$code" ;; esac
}
# A recorded push may also be refused (255): what the server answered is the fixture, read by 7.2's debugbundle below
# and by the tests.
hgpush_recorded() {
  local code=0
  hg push "$@" || code=$?
  case $code in 0 | 1 | 255) ;; *) return "$code" ;; esac
}
cat > "$WORK/ssh.py" <<'PY'
import os, subprocess, sys, threading
# A push the script does not record names no CAPTURE (its first run's other clients died here, so no race happened).
command, capture = sys.argv[-1], os.environ.get("CAPTURE")
child = subprocess.Popen(command, shell=True, stdin=subprocess.PIPE, stdout=subprocess.PIPE, bufsize=0)
def pump(source, target, path, close):
    log = open(path, "ab") if path else None
    while True:
        data = os.read(source, 65536)
        if not data:
            break
        if log:
            log.write(data)
            log.flush()
        view = memoryview(data)
        while view:
            view = view[os.write(target, view):]
    if close:
        os.close(target)
into = threading.Thread(target=pump, args=(0, child.stdin.fileno(), capture and capture + ".client", True), daemon=True)
out = threading.Thread(target=pump, args=(child.stdout.fileno(), 1, capture and capture + ".server", False))
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
# The reply is a bundle2 stream written unframed over SSH v1 (the last HG20 the server wrote: the capabilities before it
# name HG20 too), and a later response may follow it at once: walk its frames to its end, refusing an interrupt.
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
PY
cat > "$WORK/craft.py" <<'PY'
# push-plain's bundle with an unknown mandatory part after its first, and with its changegroup part given an unknown
# mandatory parameter; each sent as a bundle2 push's `unbundle` (heads = "force" in hex) over SSH v1.
import struct, sys
source, work = sys.argv[1], sys.argv[2]
bundle = open(source, "rb").read()
def i32(at):
    return struct.unpack(">i", bundle[at:at + 4])[0]
assert bundle[:4] == b"HG20"
position = 8 + i32(4)
params, frames = bundle[4:position], []
while i32(position) != 0:
    size = i32(position)
    header = bundle[position + 4:position + 4 + size]
    position += 4 + size
    chunks = []
    while i32(position) != 0:
        size = i32(position)
        assert size > 0
        chunks.append(bundle[position + 4:position + 4 + size])
        position += 4 + size
    position += 4
    frames.append((header, chunks))
assert position + 4 == len(bundle)
def parse(header):
    kind = header[1:1 + header[0]]
    at = 1 + header[0]
    ident = struct.unpack(">I", header[at:at + 4])[0]
    mandatory, advisory = header[at + 4], header[at + 5]
    sizes = header[at + 6:at + 6 + 2 * (mandatory + advisory)]
    at += 6 + len(sizes)
    pairs = []
    for index in range(mandatory + advisory):
        key = header[at:at + sizes[2 * index]]
        at += len(key)
        value = header[at:at + sizes[2 * index + 1]]
        at += len(value)
        pairs.append((key, value))
    assert at == len(header)
    return kind, ident, pairs[:mandatory], pairs[mandatory:]
def build(kind, ident, mandatory, advisory):
    out = bytes([len(kind)]) + kind + struct.pack(">I", ident) + bytes([len(mandatory), len(advisory)])
    for key, value in mandatory + advisory:
        out += bytes([len(key), len(value)])
    for key, value in mandatory + advisory:
        out += key + value
    return out
def encode(frames):
    out = b"HG20" + params
    for header, chunks in frames:
        out += struct.pack(">i", len(header)) + header
        for chunk in chunks:
            out += struct.pack(">i", len(chunk)) + chunk
        out += struct.pack(">i", 0)
    return out + struct.pack(">i", 0)
def session(name, crafted):
    heads = b"force".hex().encode()
    out = b"unbundle\nheads %d\n%s" % (len(heads), heads)
    for at in range(0, len(crafted), 4096):
        chunk = crafted[at:at + 4096]
        out += b"%d\n%s" % (len(chunk), chunk)
    open("%s/%s.client" % (work, name), "wb").write(out + b"0\n")
ids = [parse(header)[1] for header, _ in frames]
unknown = (build(b"TEST:UNKNOWN", max(ids) + 1, [], []), [])
session("push-unsupported-part", encode(frames[:1] + [unknown] + frames[1:]))
changed = []
for header, chunks in frames:
    kind, ident, mandatory, advisory = parse(header)
    if kind.lower() == b"changegroup":
        header = build(kind, ident, mandatory + [(b"slates-unknown", b"1")], advisory)
    changed.append((header, chunks))
session("push-unsupported-param", encode(changed))
PY
state() {
  hg log -R "$WORK/$1" -r 'all()' -T '{rev} {node} {p1.node} {p2.node} {phase} {branch}\n' > "$OUT/$2-server.txt"
  hg bookmarks -R "$WORK/$1" -T '{bookmark} {node}\n' > "$OUT/$2-bookmarks.txt"
}
SSH=(--config "ui.ssh=python3 -I $WORK/ssh.py" --config "ui.remotecmd=$WORK/hg72")
remote() { printf 'ssh://user@dummy/%s' "$WORK/$1"; }
# capture NAME SERVER [push arguments]: a push from the current directory to SERVER, recorded.
capture() {
  local name=$1 server=$2
  shift 2
  state "$server" "$name"
  CAPTURE="$WORK/$name" hgpush_recorded "$@" "${SSH[@]}" "$(remote "$server")"
  python3 -I "$WORK/extract.py" "$WORK/$name" "$OUT/$name"
}
# replay NAME SERVER FROM: the session FROM recorded, sent again to SERVER.
replay() {
  local name=$1 server=$2 from=$3
  state "$server" "$name"
  cp "$WORK/$from.client" "$WORK/$name.client"
  "$WORK/hg72" -R "$WORK/$server" serve --stdio < "$WORK/$name.client" > "$WORK/$name.server"
  python3 -I "$WORK/extract.py" "$WORK/$name" "$OUT/$name"
}
cd "$WORK"
hg version -q
hg init server
printf '[phases]\npublish = False\n' > server/.hg/hgrc
(cd server && printf 'a\n' > a && hg add -q a && hg commit -m base && hg bookmark -r . main && hg phase -q --public -r .)
hg clone -q server c1
(cd c1 && printf 'b\n' > b && hg add -q b && hg commit -m second && hgpush -q "${SSH[@]}" "$(remote server)" &&
  hg bookmark -r . main && capture push-bookmark server -q -B main &&
  hg phase -q --public -r . && capture push-phase server -q)
cp -R server s1
for copy in s-plain s-race s-bplain s-brace s-newhead s-unsupported; do cp -R s1 "$copy"; done
hg clone -q s1 b1
(cd b1 && printf 'd\n' > d && hg add -q d && hg commit -m plain && capture push-plain s-plain -q)
hg clone -q s1 a1
(cd a1 && printf 'e\n' > e && hg add -q e && hg commit -m other && hgpush -q "${SSH[@]}" "$(remote s-race)")
replay push-race s-race push-plain
hg clone -q s1 b2
(cd b2 && printf 'f\n' > f && hg add -q f && hg commit -m marked && hg bookmark -r . main &&
  capture push-bookmark-plain s-bplain -q -B main)
hg clone -q s1 a2
(cd a2 && hg branch -q other && printf 'g\n' > g && hg add -q g && hg commit -m elsewhere && hg bookmark -r . main &&
  hgpush -q --new-branch -B main "${SSH[@]}" "$(remote s-brace)")
replay push-race-bookmark s-brace push-bookmark-plain
cp -R s-plain s2
cp -R s2 s-pplain
cp -R s2 s-prace
hg clone -q s2 b3
(cd b3 && printf 'h\n' > h && hg add -q h && hg commit -m drafted && capture push-phase-plain s-pplain -q)
hg clone -q s2 a3
(cd a3 && hg phase -q --public -r 'desc(plain)' && hgpush -q "${SSH[@]}" "$(remote s-prace)")
replay push-race-phase s-prace push-phase-plain
printf '[hooks]\npretxnclose.reject_new_heads = python:hgext.hooklib.reject_new_heads.hook\n' >> s-newhead/.hg/hgrc
hg clone -q s1 n1
(cd n1 && hg update -q -r 0 && printf 'k\n' > k && hg add -q k && hg commit -q -m headed &&
  capture push-newhead s-newhead -q --force)
python3 -I "$WORK/craft.py" "$OUT/push-plain.hg" "$WORK"
for name in push-unsupported-part push-unsupported-param; do
  state s-unsupported "$name"
  "$WORK/hg72" -R "$WORK/s-unsupported" serve --stdio < "$WORK/$name.client" > "$WORK/$name.server"
  python3 -I "$WORK/extract.py" "$WORK/$name" "$OUT/$name"
done
for f in "$OUT"/*.hg; do hg debugbundle --all "$f" > "$f.debug"; done
(cd "$OUT" && shasum -a 256 -- *.hg *.debug *.txt > SHA256SUMS)
ls -la "$OUT"
