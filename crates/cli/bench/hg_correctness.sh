#!/usr/bin/env bash
# UI goal 93, correctness by use (main's order of 2026-11-21: Mercurial on Linux): stock Mercurial against slates' hg
# server over its HTTP edge and its SSH edge, every answer compared with stock `hg serve`'s and with the source's own.
#
# For each SOURCE repository three servers are filled the same way and asked the same questions:
# - stock: `hg serve` over a fresh empty repository, push allowed without TLS, publishing (hg's default);
# - slates-http: `slates anchor --vcs-http 127.0.0.1:0 --vcs-ssh 127.0.0.1:0` (release), a repository made by
#   `slates repository create --tool hg`, a token from `slates credential token` as Basic's password;
# - slates-ssh: a second repository on the same anchor, reached over `ssh://` with a fresh key enrolled by
#   `slates credential key`, OpenSSH trusting only the host key the daemon named.
#
# Stock is the oracle: each step runs on stock first, and each slates server must give stock's exit status, stock's state
# (every changeset's node, parents, branch and phase; tags; bookmarks; branches, closed ones marked; heads) and, where
# the step prints, stock's bytes. Each step is bounded by BOUND seconds (`alarm`; a step killed by it shows status 142:
# an HTTP edge that spins, GAPS' vcs_http.rs:850, shows here). Each check is one line, PASS or FAIL with its evidence:
#  1. push: the whole of SOURCE in one push (secret changesets stay home);
#  2. clone -U, `hg verify`, the clone's state;
#  3. clone -r REV (mid-history) and clone -r BRANCH (its first named branch other than default);
#  4. incoming (bytes) and outgoing from that partial clone, then pull, then its state;
#  5. a new head pushed without -f (refused, its abort and hint lines compared with stock's), then with -f; a bookmark
#     pushed with -B; a draft and a tag pushed (the local phases after a push to a publishing server compared); a fresh
#     clone's state; `hg verify` (each of these changesets made at a fixed date, so every server gets the same nodes);
#  6. pull -r REV, then pull -B, into an empty repository, and its state.
#
# hg runs outside every configuration file (HGRCPATH empty, HGPLAIN), never prompts, and speaks C. Everything is written
# under WORK, made empty here and removed at the end unless KEEP is set; SOURCE is only read.
#
# Usage: [SLATES=slates] [BOUND=1800] [KEEP=1] hg_correctness.sh HG WORK SOURCE...
#   HG      stock Mercurial 7.2's command (a word list is fine: "uvx --quiet --python 3.12 --from mercurial==7.2 hg")
#   WORK    a scratch directory
#   SOURCE  a local Mercurial repository, e.g. `hg clone -U https://www.mercurial-scm.org/repo/hg`, or one made by
#           hg_correctness_fixture.sh
set -uo pipefail

read -r -a HG_CMD <<< "$1"
WORK=$2
shift 2
SOURCES=("$@")
SLATES=${SLATES:-slates}
BOUND=${BOUND:-1800}

rm -rf "$WORK"
mkdir -p "$WORK/home" || exit 1
WORK=$(cd "$WORK" && pwd) || exit 1
RESULTS="$WORK/results.txt"
: > "$RESULTS"

H=(env "HOME=$WORK/home" HGRCPATH= HGPLAIN=1 HGUSER="Test <test@example.com>" HGENCODING=utf-8 LC_ALL=C
  "${HG_CMD[@]}" --noninteractive)
h() { "${H[@]}" "$@"; }
bounded() { perl -e 'alarm shift @ARGV; exec @ARGV or die "exec: $!\n"' "$BOUND" "$@"; }

PASSES=0 FAILS=0
pass() { PASSES=$((PASSES + 1)); echo "PASS $*" | tee -a "$RESULTS"; }
fail() { FAILS=$((FAILS + 1)); echo "FAIL $*" | tee -a "$RESULTS"; }

# Runs stock hg as step $2 of server $1, with the config words of key $3 (stock, http, ssh, or none: a local command),
# the rest its arguments, bounded; keeps its output and status as WORK/<server>.<step>.{out,err,status}. Stock is the
# oracle: its status is recorded; another server's must equal it, or the step FAILs with both statuses and the tail of
# its stderr, then what the slates anchor and daemon logged while the step ran (`anchor_since`): a watchdog's kill, a
# restart, a refusal, so a failing round names its cause.
run() {
  local server=$1 name=$2 key=$3 code want from
  shift 3
  local -a words=()
  [ "$key" != none ] && eval 'words=(${CFG_'"$key"'[@]+"${CFG_'"$key"'[@]}"})'
  from=$(anchor_lines)
  bounded "${H[@]}" ${words[@]+"${words[@]}"} "$@" > "$WORK/$server.$name.out" 2> "$WORK/$server.$name.err"
  code=$?
  echo "$code" > "$WORK/$server.$name.status"
  if [ "$server" = stock ]; then
    echo "stock $name: status $code" | tee -a "$RESULTS"
    return 0
  fi
  want=$(cat "$WORK/stock.$name.status" 2> /dev/null || echo none)
  if [ "$code" = "$want" ]; then
    pass "$server $name (status $code, as stock)"
  else
    local why="status $code, stock's $want"
    [ "$code" = 142 ] && why="$why: killed at the ${BOUND}s bound (a hang; an HTTP edge spin is GAPS' vcs_http.rs:850)"
    fail "$server $name: $why; stderr: $(tail -3 "$WORK/$server.$name.err" | tr '\n' ' ')"
    anchor_since "$from" | sed 's/^/  anchor: /' | tee -a "$RESULTS"
  fi
}

# How many lines the slates anchor's log holds now (0 before an anchor starts).
anchor_lines() { if [ -n "${ANCHOR_LOG:-}" ] && [ -f "$ANCHOR_LOG" ]; then wc -l < "$ANCHOR_LOG" | tr -d ' '; else echo 0; fi; }

# What the slates anchor and its daemon logged after line $1, their start's derived values left out, the last 30 lines.
anchor_since() {
  [ -n "${ANCHOR_LOG:-}" ] && [ -f "$ANCHOR_LOG" ] || return 0
  tail -n "+$(($1 + 1))" "$ANCHOR_LOG" | grep -vE '^slates daemon: [a-z0-9_]+ = ' | tail -30
}

# The state of repository $1 as WORK/state.$2.<kind>: every changeset (node, parents, branch, phase), tags, bookmarks,
# branches (closed ones marked), heads.
state() {
  local repo=$1 out=$WORK/state.$2
  h -R "$repo" log -r 'all()' -T '{node} {p1.node} {p2.node} {branch} {phase}\n' | sort > "$out.changesets"
  h -R "$repo" tags -T '{tag} {node}\n' | grep -v '^tip ' | sort > "$out.tags"
  h -R "$repo" bookmarks -T '{bookmark} {node}\n' | sort > "$out.bookmarks"
  h -R "$repo" branches -c -T '{branch} {node} {if(closed, "closed")}\n' | sort > "$out.branches"
  h -R "$repo" heads -T '{node}\n' | sort > "$out.heads"
}

# Compares server $2's state $3 with stock's state $3, kind by kind, as check $1.
against_stock() {
  local name=$1 server=$2 which=$3 kind a b
  for kind in changesets tags bookmarks branches heads; do
    a=$WORK/state.stock-$which.$kind b=$WORK/state.$server-$which.$kind
    if [ ! -f "$a" ] || [ ! -f "$b" ]; then fail "$server $name $kind: no state to compare"; continue; fi
    if cmp -s "$a" "$b"; then
      pass "$server $name $kind as stock's ($(wc -l < "$a" | tr -d ' '))"
    else
      fail "$server $name $kind DIFFER from stock's: $(diff "$a" "$b" | grep -c '^[<>]') lines; $(diff "$a" "$b" | grep '^[<>]' | head -3 | tr '\n' ' ')"
    fi
  done
}

# Compares files $2 (stock's) and $3 byte for byte as check $1.
same_bytes() {
  if cmp -s "$2" "$3"; then pass "$1 as stock's ($(wc -l < "$2" | tr -d ' ') lines)"
  else fail "$1 DIFFER from stock's: $(diff "$2" "$3" | grep '^[<>]' | head -3 | tr '\n' ' ')"; fi
}

ANCHOR="" DAEMON="" STOCK=""
stop_servers() {
  if [ -n "$ANCHOR" ]; then kill -9 "$ANCHOR" 2> /dev/null || true; wait "$ANCHOR" 2> /dev/null || true; fi
  if [ -n "$DAEMON" ]; then
    for _ in $(seq 1 100); do kill -0 "$DAEMON" 2> /dev/null || break; sleep 0.1; done
    kill -9 "$DAEMON" 2> /dev/null || true
  fi
  if [ -n "$STOCK" ]; then kill "$STOCK" 2> /dev/null || true; wait "$STOCK" 2> /dev/null || true; fi
  ANCHOR="" DAEMON="" STOCK=""
}
trap 'stop_servers; [ -z "${KEEP:-}" ] && rm -rf "$WORK"' EXIT
first() { sed -n "$2" "$1" | head -1; }

# Each server's URL (URL_stock, URL_http, URL_ssh) and config words (CFG_*), plain variables so bash 3.2 (macOS's own)
# runs this as well as Linux's bash.
URL_stock="" URL_http="" URL_ssh=""
CFG_stock=() CFG_http=() CFG_ssh=()
url() { eval "printf '%s' \"\$URL_$1\""; }

# Starts stock `hg serve` over a fresh empty repository for source $1: sets URL_stock.
start_stock() {
  local root="$WORK/stock-$1" port
  h init "$root"
  port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
  h -R "$root" serve --address 127.0.0.1 --port "$port" --config web.push_ssl=False --config 'web.allow-push=*' \
    --errorlog "$WORK/stock-$1.log" > /dev/null 2>&1 &
  STOCK=$!
  URL_stock="http://127.0.0.1:$port/"
  for _ in $(seq 1 100); do h identify "$URL_stock" > /dev/null 2>&1 && break; sleep 0.1; done
}

# Starts a slates anchor with both edges for source $1, makes two hg repositories, a token and an enrolled key: sets
# URL_http, URL_ssh, CFG_http, CFG_ssh, ANCHOR, DAEMON.
start_slates() {
  local instance="w-balance-hgc-$$-$1" log="$WORK/anchor-$1.log" exports token streak=0 port sshport fp known
  ANCHOR_LOG=$log
  mkdir -p "$WORK/store-$1"
  "$SLATES" --instance "$instance" anchor --vcs-http 127.0.0.1:0 --vcs-ssh 127.0.0.1:0 --vcs-store "$WORK/store-$1" \
    > /dev/null 2> "$log" &
  ANCHOR=$!
  for _ in $(seq 1 1200); do
    if grep -q "^slates daemon: instance $instance serving on" "$log" \
      && grep -q '^slates anchor: issuer surface: export ' "$log" \
      && grep -q '^slates daemon: the version-control SSH edge serves on' "$log"; then break; fi
    kill -0 "$ANCHOR" 2> /dev/null || { cat "$log"; return 1; }
    sleep 0.1
  done
  DAEMON=$(first "$log" 's/^slates anchor: instance .* up; daemon pid \([0-9]*\);.*/\1/p')
  port=$(first "$log" 's/^slates daemon: the version-control HTTP edge serves cleartext on 127\.0\.0\.1:\([0-9]*\).*/\1/p')
  sshport=$(first "$log" 's/^slates daemon: the version-control SSH edge serves on 127\.0\.0\.1:\([0-9]*\).*/\1/p')
  fp=$(first "$log" 's/^slates daemon: the version-control SSH edge serves on .* host key \(SHA256:[^ ]*\).*/\1/p')
  exports=$(first "$log" 's/^slates anchor: issuer surface: export //p')
  for _ in $(seq 1 1200); do
    if "$SLATES" --instance "$instance" volume list > /dev/null 2>&1; then streak=$((streak + 1)); else streak=0; fi
    [ "$streak" -ge 10 ] && break
    sleep 0.1
  done
  "$SLATES" --instance "$instance" bootstrap root > /dev/null
  "$SLATES" --instance "$instance" repository create viahttp --tool hg > /dev/null
  "$SLATES" --instance "$instance" repository create viassh --tool hg > /dev/null
  # shellcheck disable=SC2086 # the surface's NAME=VALUE pairs are words
  token=$(env $exports "$SLATES" --instance "$instance" credential token | sed -n 's/^token: //p')
  URL_http="http://127.0.0.1:$port/viahttp"
  CFG_http=(--config "auth.slates.prefix=http://127.0.0.1:$port/" --config auth.slates.username=slates
    --config "auth.slates.password=$token")
  ssh-keygen -q -t ed25519 -N '' -f "$WORK/key-$1" -C "w-balance-hgc"
  # shellcheck disable=SC2086
  env $exports "$SLATES" --instance "$instance" credential key "$(cat "$WORK/key-$1.pub")" > /dev/null
  known="$WORK/known-$1"
  ssh-keyscan -t ed25519 -p "$sshport" 127.0.0.1 2> /dev/null > "$known"
  if ! ssh-keygen -l -f "$known" | grep -q "$fp"; then echo "the scanned host key is not the one the daemon named" >&2; return 1; fi
  URL_ssh="ssh://slates@127.0.0.1:$sshport/viassh"
  CFG_ssh=(--config "ui.ssh=ssh -F /dev/null -i $WORK/key-$1 -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=yes -o UserKnownHostsFile=$known -o GlobalKnownHostsFile=/dev/null")
  echo "slates: $(first "$log" 's/^\(slates daemon: instance .* serving on .*\)/\1/p')"
}

# The config words of server $1.

# Every verb against every server for source $1 (its index $2). Stock runs each step first: it is the oracle.
verbs() {
  local src=$1 n=$2 server mid branch tip w e head drafted tagged
  local servers="stock http ssh"
  tip=$(h -R "$src" log -r 'max(not secret())' -T '{rev}')
  mid=$((tip / 2))
  branch=$(h -R "$src" branches -T '{branch}\n' | grep -vx default | head -1)
  echo "== source $n: $src, $((tip + 1)) revisions not secret, mid $mid, named branch '${branch:-none}'" | tee -a "$RESULTS"

  for server in $servers; do
    # 1. The whole history in one push.
    run "$server" push "$server" -R "$src" push -q "$(url "$server")"
  done
  for server in $servers; do
    # 2. A full clone, verified, its state against stock's.
    run "$server" clone "$server" clone -q -U "$(url "$server")" "$WORK/$server-full"
    [ -d "$WORK/$server-full/.hg" ] || continue
    run "$server" verify none -R "$WORK/$server-full" verify
    state "$WORK/$server-full" "$server-full"
    [ "$server" != stock ] && against_stock "clone" "$server" full
  done
  for server in $servers; do
    # 3. Partial clones: a mid-history revision and a named branch.
    run "$server" clone-r "$server" clone -q -U -r "$mid" "$(url "$server")" "$WORK/$server-mid"
    [ -d "$WORK/$server-mid/.hg" ] && state "$WORK/$server-mid" "$server-mid"
    [ "$server" != stock ] && against_stock "clone -r $mid" "$server" mid
    [ -n "$branch" ] || continue
    run "$server" clone-branch "$server" clone -q -U -r "$branch" "$(url "$server")" "$WORK/$server-branch"
    [ -d "$WORK/$server-branch/.hg" ] && state "$WORK/$server-branch" "$server-branch"
    [ "$server" != stock ] && against_stock "clone -r $branch" "$server" branch
  done
  for server in $servers; do
    # 4. incoming and outgoing from the partial clone, byte for byte; then pull.
    [ -d "$WORK/$server-mid/.hg" ] || continue
    run "$server" incoming "$server" -R "$WORK/$server-mid" incoming -q -T '{node} {branch} {phase}\n' "$(url "$server")"
    [ "$server" != stock ] && same_bytes "$server incoming" "$WORK/stock.incoming.out" "$WORK/$server.incoming.out"
    run "$server" outgoing "$server" -R "$WORK/$server-mid" outgoing -q -T '{node}\n' "$(url "$server")"
    run "$server" pull "$server" -R "$WORK/$server-mid" pull -q "$(url "$server")"
    state "$WORK/$server-mid" "$server-pulled"
    [ "$server" != stock ] && against_stock "pulled clone" "$server" pulled
  done
  for server in $servers; do
    # 5. A new head (refused without -f, in stock's words), -f, a bookmark with -B, a draft and a tag; every changeset
    #    made at a fixed date, so each server receives the same nodes and their states compare whole.
    w="$WORK/$server-work"
    h clone -q -r "$mid" "$src" "$w" || { fail "$server: could not make the work clone"; continue; }
    echo "a new head" > "$w/w-balance-head.txt"
    h -R "$w" commit -q -A -d "1700100000 0" -m "a new head" "$w/w-balance-head.txt"
    head=$(h -R "$w" log -r . -T '{node}')
    run "$server" push-head "$server" -R "$w" push -r "$head" "$(url "$server")"
    grep -i 'abort\|hint' "$WORK/$server.push-head.err" | sed 's#http://[^ ]*#URL#g; s#ssh://[^ ]*#URL#g' > "$WORK/$server.push-head.words"
    [ "$server" != stock ] && same_bytes "$server new-head refusal's words" "$WORK/stock.push-head.words" "$WORK/$server.push-head.words"
    run "$server" push-head-f "$server" -R "$w" push -q -f -r "$head" "$(url "$server")"
    h -R "$w" bookmark -r "$head" w-balance-mark
    run "$server" push-mark "$server" -R "$w" push -q -B w-balance-mark "$(url "$server")"
    echo "a draft" > "$w/w-balance-draft.txt"
    h -R "$w" commit -q -A -d "1700100060 0" -m "a draft" "$w/w-balance-draft.txt"
    drafted=$(h -R "$w" log -r . -T '{node}')
    h -R "$w" tag -q -d "1700100120 0" -r "$drafted" w-balance-tag
    tagged=$(h -R "$w" log -r . -T '{node}')
    run "$server" push-draft "$server" -R "$w" push -q -r "$tagged" "$(url "$server")"
    h -R "$w" log -r "$drafted::" -T '{node} {phase}\n' > "$WORK/$server.local-phases"
    [ "$server" != stock ] && same_bytes "$server local phases after the push" "$WORK/stock.local-phases" "$WORK/$server.local-phases"
    run "$server" clone-after "$server" clone -q -U "$(url "$server")" "$WORK/$server-after"
    [ -d "$WORK/$server-after/.hg" ] || continue
    run "$server" verify-after none -R "$WORK/$server-after" verify
    state "$WORK/$server-after" "$server-after"
    [ "$server" != stock ] && against_stock "clone after the pushes" "$server" after
  done
  for server in $servers; do
    # 6. pull -r and pull -B into an empty repository.
    e="$WORK/$server-empty"
    h init "$e"
    run "$server" pull-r "$server" -R "$e" pull -q -r "$mid" "$(url "$server")"
    run "$server" pull-B "$server" -R "$e" pull -q -B w-balance-mark "$(url "$server")"
    state "$e" "$server-pulled-into-empty"
    [ "$server" != stock ] && against_stock "pull -r $mid then -B into an empty repository" "$server" pulled-into-empty
  done
}

echo "hg: $(h --version | head -1); slates: $("$SLATES" version 2> /dev/null || echo none); $(uname -srm), $(nproc 2> /dev/null || sysctl -n hw.ncpu) cores, load $(cut -d' ' -f1-3 /proc/loadavg 2> /dev/null || uptime)" | tee -a "$RESULTS"
n=0
for src in "${SOURCES[@]}"; do
  n=$((n + 1))
  rm -rf "$WORK"/stock-* "$WORK"/*-full "$WORK"/*-mid "$WORK"/*-branch "$WORK"/*-work "$WORK"/*-after "$WORK"/*-empty
  start_stock "$n"
  if start_slates "$n"; then verbs "$(cd "$src" && pwd)" "$n"; else fail "source $n: the slates anchor did not start"; fi
  stop_servers
done
echo "== $PASSES passed, $FAILS failed; load at the end $(cut -d' ' -f1-3 /proc/loadavg 2> /dev/null || uptime)" | tee -a "$RESULTS"
[ "$FAILS" = 0 ]
