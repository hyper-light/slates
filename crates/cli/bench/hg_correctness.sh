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
# The verbs, each bounded by BOUND seconds (`alarm`; a step killed by it records status 142: an HTTP edge that spins,
# GAPS' vcs_http.rs:850, shows here), and each result one line, PASS or FAIL with its evidence:
#  1. push: the whole of SOURCE in one push (every changeset, bookmark and tag; secret changesets stay home).
#  2. clone -U, then `hg verify`, then the clone's state against SOURCE's (its public and draft changesets) and against
#     stock's clone: every changeset's node, parents, branch and phase; tags; bookmarks; branches; heads.
#  3. clone -r REV (a mid-history revision) and `-r BRANCH` where SOURCE has named branches: the same comparisons, over
#     the revisions' ancestors.
#  4. incoming and outgoing between that partial clone and each server, byte for byte against stock's; then pull, and
#     the state compared again.
#  5. push of a new head without -f (refused, as stock refuses), then with -f; a bookmark pushed with -B; a draft pushed
#     (public on a publishing server); a tag made and pushed; each then read back by a fresh clone and compared.
#  6. pull -r and pull -B into a fresh empty repository.
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

# Runs stock hg as one step of server $1 named $2, with the config words of key $3 (stock, http, ssh, or none), the rest
# its arguments; bounded; its output kept as WORK/<server>.<name>.{out,err}; echoes its status.
step() {
  local server=$1 name=$2 key=$3 code
  shift 3
  local -a words=()
  [ "$key" != none ] && mapfile -t words < <(cfg "$key")
  bounded "${H[@]}" ${words[@]+"${words[@]}"} "$@" > "$WORK/$server.$name.out" 2> "$WORK/$server.$name.err"
  code=$?
  echo "$code"
}

# Requires step status $3 of server $1 named $2 to be $4 (default 0), else records the failure with stderr's tail.
expect_status() {
  local server=$1 name=$2 got=$3 want=${4:-0}
  if [ "$got" = "$want" ]; then
    pass "$server $name (status $got)"
  else
    local why="status $got, wanted $want"
    [ "$got" = 142 ] && why="$why: killed at the ${BOUND}s bound (a hang; an HTTP edge spin is GAPS' vcs_http.rs:850)"
    fail "$server $name: $why; $(tail -3 "$WORK/$server.$name.err" | tr '\n' ' ')"
  fi
}

# The state of repository $1 over revset $2, written as WORK/state.$3.<kind>: changesets (node, parents, branch, phase),
# tags, bookmarks, branches (with closed ones), heads.
state() {
  local repo=$1 revs=$2 out=$WORK/state.$3
  h -R "$repo" log -r "$revs" -T '{node} {p1.node} {p2.node} {branch} {phase}\n' | sort > "$out.changesets"
  h -R "$repo" tags -T '{tag} {node}\n' | grep -v '^tip ' | sort > "$out.tags"
  h -R "$repo" bookmarks -T '{bookmark} {node}\n' | sort > "$out.bookmarks"
  h -R "$repo" branches -c -T '{branch} {node} {if(closed, "closed")}\n' | sort > "$out.branches"
  h -R "$repo" heads -T '{node}\n' | sort > "$out.heads"
}

# Compares the states $2 and $3 kind by kind as check $1; KINDS (default every kind) limits it.
compare() {
  local name=$1 a=$WORK/state.$2 b=$WORK/state.$3 kind
  for kind in ${KINDS:-changesets tags bookmarks branches heads}; do
    if cmp -s "$a.$kind" "$b.$kind"; then
      pass "$name $kind identical ($(wc -l < "$a.$kind" | tr -d ' '))"
    else
      fail "$name $kind DIFFER ($2 against $3): $(diff "$a.$kind" "$b.$kind" | grep -c '^[<>]') lines; $(diff "$a.$kind" "$b.$kind" | grep '^[<>]' | head -3 | tr '\n' ' ')"
    fi
  done
}

# Compares two files byte for byte as check $1.
same_bytes() {
  if cmp -s "$2" "$3"; then pass "$1 identical ($(wc -l < "$2" | tr -d ' ') lines)"
  else fail "$1 DIFFER: $(diff "$2" "$3" | grep '^[<>]' | head -3 | tr '\n' ' ')"; fi
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

# Starts stock `hg serve` over a fresh empty repository for source $1: sets URL[stock], CFG_stock.
declare -A URL
CFG_stock=() CFG_http=() CFG_ssh=()
start_stock() {
  local root="$WORK/stock-$1" port
  h init "$root"
  port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
  h -R "$root" serve --address 127.0.0.1 --port "$port" --config web.push_ssl=False --config 'web.allow-push=*' \
    --errorlog "$WORK/stock-$1.log" > /dev/null 2>&1 &
  STOCK=$!
  URL[stock]="http://127.0.0.1:$port/"
  for _ in $(seq 1 100); do h identify "${URL[stock]}" > /dev/null 2>&1 && break; sleep 0.1; done
}

# Starts a slates anchor with both edges for source $1, makes two hg repositories, a token and an enrolled key: sets
# URL[http], URL[ssh], CFG_http, CFG_ssh, ANCHOR, DAEMON.
start_slates() {
  local instance="w-balance-hgc-$$-$1" log="$WORK/anchor-$1.log" exports token streak=0 port sshport fp known
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
  URL[http]="http://127.0.0.1:$port/viahttp"
  CFG_http=(--config "auth.slates.prefix=http://127.0.0.1:$port/" --config auth.slates.username=slates
    --config "auth.slates.password=$token")
  ssh-keygen -q -t ed25519 -N '' -f "$WORK/key-$1" -C "w-balance-hgc"
  # shellcheck disable=SC2086
  env $exports "$SLATES" --instance "$instance" credential key "$(cat "$WORK/key-$1.pub")" > /dev/null
  known="$WORK/known-$1"
  ssh-keyscan -t ed25519 -p "$sshport" 127.0.0.1 2> /dev/null > "$known"
  if ! ssh-keygen -l -f "$known" | grep -q "$fp"; then echo "the scanned host key is not the one the daemon named" >&2; return 1; fi
  URL[ssh]="ssh://slates@127.0.0.1:$sshport/viassh"
  CFG_ssh=(--config "ui.ssh=ssh -F /dev/null -i $WORK/key-$1 -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=yes -o UserKnownHostsFile=$known -o GlobalKnownHostsFile=/dev/null")
  echo "slates: $(first "$log" 's/^\(slates daemon: instance .* serving on .*\)/\1/p')"
}

# The config words of server $1.
cfg() { local -n configured="CFG_$1"; printf '%s\n' "${configured[@]+"${configured[@]}"}"; }

# Every verb against every server for source $1 (its index $2).
verbs() {
  local src=$1 n=$2 server code mid branch tip
  local servers=(stock http ssh)
  state "$src" 'not secret()' "source"
  tip=$(h -R "$src" log -r 'max(not secret())' -T '{rev}')
  mid=$((tip / 2))
  branch=$(h -R "$src" branches -T '{branch}\n' | grep -vx default | head -1)
  echo "== source $n: $src, $((tip + 1)) revisions, mid $mid, named branch '${branch:-none}'" | tee -a "$RESULTS"

  # 1. push the whole history.
  for server in "${servers[@]}"; do
    code=$(step "$server" push "$server" -R "$src" push -q "${URL[$server]}")
    expect_status "$server" "push all" "$code"
  done
  # 2. full clone, verify, state.
  for server in "${servers[@]}"; do
    code=$(step "$server" clone "$server" clone -q -U "${URL[$server]}" "$WORK/$server-full")
    expect_status "$server" "clone -U" "$code"
    [ -d "$WORK/$server-full/.hg" ] || continue
    code=$(step "$server" verify none -R "$WORK/$server-full" verify)
    expect_status "$server" "verify" "$code"
    state "$WORK/$server-full" 'all()' "$server-full"
    compare "$server clone against the source" "source" "$server-full"
    [ "$server" != stock ] && compare "$server clone against stock's" "stock-full" "$server-full"
  done
  # 3. partial clones.
  for server in "${servers[@]}"; do
    code=$(step "$server" clone-r "$server" clone -q -U -r "$mid" "${URL[$server]}" "$WORK/$server-mid")
    expect_status "$server" "clone -r $mid" "$code"
    if [ -d "$WORK/$server-mid/.hg" ]; then
      state "$WORK/$server-mid" 'all()' "$server-mid"
      [ "$server" = stock ] && state "$src" "::$mid" "source-mid"
      KINDS=changesets compare "$server clone -r $mid against the source's ancestors" "source-mid" "$server-mid"
      [ "$server" != stock ] && compare "$server clone -r $mid against stock's" "stock-mid" "$server-mid"
    fi
    if [ -n "$branch" ]; then
      code=$(step "$server" clone-b "$server" clone -q -U -r "$branch" "${URL[$server]}" "$WORK/$server-branch")
      expect_status "$server" "clone -r $branch" "$code"
      [ -d "$WORK/$server-branch/.hg" ] && state "$WORK/$server-branch" 'all()' "$server-branch"
      [ "$server" != stock ] && [ -d "$WORK/$server-branch/.hg" ] && compare "$server clone -r $branch against stock's" "stock-branch" "$server-branch"
    fi
  done
  # 4. incoming, outgoing, pull, byte for byte against stock's.
  for server in "${servers[@]}"; do
    [ -d "$WORK/$server-mid/.hg" ] || continue
    code=$(step "$server" incoming "$server" -R "$WORK/$server-mid" incoming -q -T '{node} {branch} {phase}\n' "${URL[$server]}")
    expect_status "$server" "incoming" "$code"
    [ "$server" != stock ] && same_bytes "$server incoming against stock's" "$WORK/stock.incoming.out" "$WORK/$server.incoming.out"
    code=$(step "$server" outgoing "$server" -R "$WORK/$server-mid" outgoing -q -T '{node}\n' "${URL[$server]}")
    expect_status "$server" "outgoing (none: status 1)" "$code" 1
    code=$(step "$server" pull "$server" -R "$WORK/$server-mid" pull -q "${URL[$server]}")
    expect_status "$server" "pull" "$code"
    state "$WORK/$server-mid" 'all()' "$server-pulled"
    compare "$server pulled clone against the source" "source" "$server-pulled"
  done
  # 5. new heads, bookmarks, phases, tags.
  for server in "${servers[@]}"; do
    local w="$WORK/$server-work" head marked drafted
    rm -rf "$w"; h clone -q -r "$mid" "$src" "$w" || continue
    echo "new head on $server" > "$w/w-balance-head.txt"
    h -R "$w" commit -q -A -m "a new head" "$w/w-balance-head.txt"
    head=$(h -R "$w" log -r . -T '{node}')
    code=$(step "$server" push-head "$server" -R "$w" push -r "$head" "${URL[$server]}")
    expect_status "$server" "push of a new head without -f (refused)" "$code" 255
    [ "$server" != stock ] && same_bytes "$server new-head refusal's words against stock's" \
      <(grep -i 'abort\|hint' "$WORK/stock.push-head.err" | sed 's#http://[^ ]*##; s#ssh://[^ ]*##') \
      <(grep -i 'abort\|hint' "$WORK/$server.push-head.err" | sed 's#http://[^ ]*##; s#ssh://[^ ]*##')
    code=$(step "$server" push-head-f "$server" -R "$w" push -q -f -r "$head" "${URL[$server]}")
    expect_status "$server" "push -f of a new head" "$code"
    h -R "$w" bookmark -r "$head" w-balance-mark
    code=$(step "$server" push-mark "$server" -R "$w" push -q -B w-balance-mark "${URL[$server]}")
    expect_status "$server" "push -B w-balance-mark" "$code"
    h -R "$w" update -q -r "$head"
    echo "draft on $server" > "$w/w-balance-draft.txt"
    h -R "$w" commit -q -A -m "a draft" "$w/w-balance-draft.txt"
    drafted=$(h -R "$w" log -r . -T '{node}')
    h -R "$w" tag -q -r "$drafted" w-balance-tag
    marked=$(h -R "$w" log -r . -T '{node}')
    code=$(step "$server" push-draft "$server" -R "$w" push -q -r "$marked" "${URL[$server]}")
    expect_status "$server" "push of a draft and a tag" "$code"
    h -R "$w" log -r "$drafted::" -T '{phase}\n' | sort -u > "$WORK/$server.phase-after"
    code=$(step "$server" clone-after "$server" clone -q -U "${URL[$server]}" "$WORK/$server-after")
    expect_status "$server" "clone after the pushes" "$code"
    [ -d "$WORK/$server-after/.hg" ] || continue
    h -R "$WORK/$server-after" log -r "$head + $drafted + $marked" -T '{node} {phase}\n' > "$WORK/$server.after-nodes"
    h -R "$WORK/$server-after" bookmarks -T '{bookmark} {node}\n' | grep w-balance-mark > "$WORK/$server.after-mark"
    h -R "$WORK/$server-after" tags -T '{tag} {node}\n' | grep w-balance-tag > "$WORK/$server.after-tag"
    if [ "$server" != stock ]; then
      # Stock's nodes differ (each server got its own head), so compare shapes: phases, the bookmark's and tag's targets.
      same_bytes "$server pushed phases (publishing) against stock's" \
        <(awk '{print $2}' "$WORK/stock.after-nodes") <(awk '{print $2}' "$WORK/$server.after-nodes")
      same_bytes "$server local phase after push against stock's" "$WORK/stock.phase-after" "$WORK/$server.phase-after"
    fi
    [ "$(awk '{print $2}' "$WORK/$server.after-mark")" = "$head" ] && pass "$server bookmark at its head" \
      || fail "$server bookmark: '$(cat "$WORK/$server.after-mark")', wanted $head"
    [ "$(awk '{print $2}' "$WORK/$server.after-tag")" = "$drafted" ] && pass "$server tag at its changeset" \
      || fail "$server tag: '$(cat "$WORK/$server.after-tag")', wanted $drafted"
    code=$(step "$server" verify-after none -R "$WORK/$server-after" verify)
    expect_status "$server" "verify after the pushes" "$code"
  done
  # 6. pull -r and pull -B into an empty repository.
  for server in "${servers[@]}"; do
    local e="$WORK/$server-empty"
    rm -rf "$e"; h init "$e"
    code=$(step "$server" pull-r "$server" -R "$e" pull -q -r "$mid" "${URL[$server]}")
    expect_status "$server" "pull -r $mid into an empty repository" "$code"
    state "$e" 'all()' "$server-pull-r"
    KINDS=changesets compare "$server pull -r $mid against the source's ancestors" "source-mid" "$server-pull-r"
    code=$(step "$server" pull-B "$server" -R "$e" pull -q -B w-balance-mark "${URL[$server]}")
    expect_status "$server" "pull -B w-balance-mark" "$code"
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
