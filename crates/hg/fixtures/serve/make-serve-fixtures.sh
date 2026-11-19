#!/usr/bin/env bash
# Makes slates-hg's read-serving golden vectors (A-619) with stock Mercurial 7.2, from PyPI through uv (nothing
# installed system-wide). Deterministic but for the run's paths: no configuration but this script's, a fixed user,
# every date 0 0.
# Usage: make-serve-fixtures.sh OUTDIR
#   OUTDIR receives, for one non-publishing repository (two named branches, a closed head on a branch whose name needs
#   quoting, a merge, public and draft changesets, an amended draft whose predecessor is hidden, bookmarks):
#   state-<when>.txt   the served view, oldest first: node, first and second parents, phase, 1 where it closes its
#                      branch, then | and its branch; bookmarks-<when>.txt: node and name per bookmark
#   probe.client/.server  what a client wrote to `hg serve --stdio` and what the server answered: heads, known,
#                      between, branches, branchmap, and listkeys of namespaces, bookmarks and phases
#   session-<clone|pull|incoming>.client/.server/.stderr  a stock client's clone, then pull, then incoming, recorded by
#                      an ssh stand-in, the server's state before each in state-<when>.txt
#   incoming.txt (the incoming's output) and SHA256SUMS of them all.
set -euo pipefail
OUT=$(cd "$1" && pwd)
export HGPLAIN=1 HGRCPATH= HGENCODING=utf-8 PYTHONHASHSEED=0 TZ=UTC LC_ALL=C
WORK=$(mktemp -d "$HOME/.cache/g079-hgserve.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
printf '#!/bin/sh\nexec uvx --quiet --python 3.12 --from mercurial==7.2 hg "$@"\n' > "$WORK/hg72"
chmod +x "$WORK/hg72"
hg() { "$WORK/hg72" --config ui.username=test --config devel.default-date='0 0' \
  --config experimental.evolution.createmarkers=true --config experimental.evolution.exchange=true "$@"; }
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
# The probe: SSH version 1 by hand against `hg serve --stdio`, each command as 7.2's `sshv1peer` writes it.
cat > "$WORK/probe.py" <<'PY'
import subprocess, sys
hg, repo, state, out = sys.argv[1:5]
nodes = [line.split()[0].encode() for line in open(state)]
null, unknown = b"0" * 40, b"1234567890abcdef" * 2 + b"12345678"
child = subprocess.Popen([hg, "-R", repo, "serve", "--stdio"], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
sent, got = bytearray(), bytearray()
def call(name, args=(), star=False):
    request = name + b"\n"
    for key, value in args:
        request += key + b" %d\n" % len(value) + value
    if star:
        request += b"* 0\n"
    child.stdin.write(request); child.stdin.flush(); sent.extend(request)
    line = child.stdout.readline(); got.extend(line)
    got.extend(child.stdout.read(int(line)))
call(b"heads")
call(b"known", [(b"nodes", b" ".join([nodes[-1], nodes[0], unknown, nodes[len(nodes) // 2], null]))], star=True)
call(b"between", [(b"pairs", b" ".join([nodes[-1] + b"-" + null, nodes[-1] + b"-" + nodes[1], null + b"-" + null]))])
call(b"branches", [(b"nodes", b" ".join([nodes[-1], nodes[len(nodes) // 2]]))])
call(b"branchmap")
for namespace in (b"namespaces", b"bookmarks", b"phases"):
    call(b"listkeys", [(b"namespace", namespace)])
child.stdin.close(); child.wait()
open(out + ".client", "wb").write(sent); open(out + ".server", "wb").write(got)
PY
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
dump probe
python3 -I "$WORK/probe.py" "$WORK/hg72" "$WORK/server" "$OUT/state-probe.txt" "$OUT/probe"
SSH=(--config "ui.ssh=python3 -I $WORK/ssh.py" --config "ui.remotecmd=$WORK/hg72")
REMOTE="ssh://user@dummy/$WORK/server"
dump clone
CAPTURE="$OUT/session-clone" hg clone -q -U "${SSH[@]}" "$REMOTE" client
(cd server && hg update -q main && printf 'c\nf\n' > c && hg commit -q -m 'f on main' && hg phase -q --public -r .)
dump pull
(cd client && CAPTURE="$OUT/session-pull" hg pull -q "${SSH[@]}" "$REMOTE")
(cd server && hg update -q topic && printf 'g\n' > g && hg add -q g && hg commit -q -m 'draft g')
dump incoming
(cd client && CAPTURE="$OUT/session-incoming" hg incoming -q "${SSH[@]}" "$REMOTE" > "$OUT/incoming.txt")
cd "$OUT"
shasum -a 256 state-*.txt bookmarks-*.txt probe.* session-* incoming.txt > SHA256SUMS
cat SHA256SUMS
