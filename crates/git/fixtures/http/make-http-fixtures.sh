#!/usr/bin/env bash
# Makes slates-git's smart HTTP golden vectors (A-595): stock git's own HTTP traffic with `git http-backend`, through
# cgi-host.py, a loopback CGI host that records each exchange (its docstring names each file). As main ruled with A-607,
# the fixtures hold inflated bytes alone: a gzip body is held inflated (by Python's gzip, an inflater apart from slates')
# and the test gzips it again; no pack is held. Deterministic but for one thing: no configuration but this script's, a
# fixed identity, every date 2005-04-07T22:13:13Z; the loopback port in each `Host` line differs from run to run.
# Usage: make-http-fixtures.sh OUTDIR
#
# The exchanges, in order (cgi-host.py numbers them):
#   01      `git ls-remote`, protocol v0: the upload-pack advertisement, `# service=` first.
#   02      the same at protocol v1 (`Git-Protocol: version=1`).
#   03-05   `git clone` at protocol v2 of small.git (main and 30 branches): the capability advertisement, `ls-refs`, and
#           a `fetch` wanting 31 tips, past git's 1024-byte threshold, so gzipped (`remote-curl.c`, `post_rpc`).
#   06-07   `git push` of one commit: the receive-pack advertisement and the push (its body, a pack, is not held).
#   08-10   `git clone` at protocol v2 of many.git (1400 branches) with a post buffer of 64 KiB, which its 70 KB
#           `fetch` request outgrows, so it is sent chunked. (git 2.50.1 refuses a buffer of LARGE_PACKET_MAX, 65520,
#           or less over a stateless connection: "BUG: remote-curl.c:1529: The entire rpc->buf should be larger than
#           LARGE_PACKET_MAX".)
#
# Sources: git's Documentation/gitprotocol-http.txt, git-http-backend.txt, config/http.txt (`http.postBuffer`);
# http-backend.c and remote-curl.c (read 2026-11-19).
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=$(cd "$1" && pwd)
unset GIT_CONFIG_PARAMETERS GIT_DIR GIT_WORK_TREE
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_COUNT=0 TZ=UTC LC_ALL=C LANG=C
export GIT_AUTHOR_NAME=test GIT_AUTHOR_EMAIL=test@example.com GIT_COMMITTER_NAME=test GIT_COMMITTER_EMAIL=test@example.com
export GIT_AUTHOR_DATE='1112911993 +0000' GIT_COMMITTER_DATE='1112911993 +0000'
WORK=$(mktemp -d "$HOME/.cache/g076b-httpfix.XXXXXX")
HOST_PID=
trap '[ -z "$HOST_PID" ] || kill "$HOST_PID" 2>/dev/null; rm -rf "$WORK"' EXIT
export HOME="$WORK/home"
mkdir -p "$HOME" "$WORK/root"
{ git --version; python3 --version; } > "$OUT/versions.txt"

# Branches PREFIX0001.. PREFIXnnnn, each a commit of its own on the empty tree, for fast-import.
commits() {
  local i msg
  for i in $(seq 1 "$2"); do
    msg=$(printf 'commit %s%04d' "$1" "$i")
    printf 'commit refs/heads/%s%04d\ncommitter test <test@example.com> 1112911993 +0000\ndata %d\n%s\n\n' \
      "$1" "$i" $(( ${#msg} + 1 )) "$msg"
  done
}
git init -q --bare -b main "$WORK/root/small.git"
{ printf 'commit refs/heads/main\ncommitter test <test@example.com> 1112911993 +0000\ndata 5\nmain\n\n'
  commits side 30; } | git -C "$WORK/root/small.git" fast-import --quiet
git -C "$WORK/root/small.git" config http.receivepack true
git init -q --bare -b b0001 "$WORK/root/many.git"
commits b 1400 | git -C "$WORK/root/many.git" fast-import --quiet

python3 -I "$HERE/cgi-host.py" "$WORK/port" "$OUT" "$WORK/root" &
HOST_PID=$!
for _ in $(seq 1 100); do [ -s "$WORK/port" ] && break; sleep 0.1; done
URL="http://127.0.0.1:$(cat "$WORK/port")"
git -c protocol.version=0 ls-remote "$URL/small.git" > /dev/null
git -c protocol.version=1 ls-remote "$URL/small.git" > /dev/null
git -c protocol.version=2 clone -q "$URL/small.git" "$WORK/small"
git -C "$WORK/small" commit -q --allow-empty -m pushed
git -C "$WORK/small" push -q origin main
git -c protocol.version=2 -c http.postBuffer=65536 clone -q --bare "$URL/many.git" "$WORK/many"
kill "$HOST_PID"
wait "$HOST_PID" 2> /dev/null || true
HOST_PID=
cd "$OUT"
shasum -a 256 [0-9][0-9]-* versions.txt > SHA256SUMS
