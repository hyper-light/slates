#!/usr/bin/env bash
# A-699 (UI goal 93): stock Mercurial serving a large real repository, every step timed: the baseline slates' Mercurial
# site is measured against. `docs/wip/BENCHMARKS.md` records each run of it; the command there is the contract.
#
# The servers, a fresh one for every run, in SERVERS' order:
# - stock: `hg serve` over HTTP on loopback over a fresh empty repository, push allowed without TLS
#   (`web.push_ssl=False`, `web.allow-push=*`), as a stock server that takes pushes is configured;
# - slates: `slates anchor --vcs-http 127.0.0.1:0` from the release build (its shards and budget derived from this
#   machine), a Mercurial repository made by `slates repository create --tool hg`, and a token from `slates credential
#   token` (run with the anchor's issuer surface) as Basic's password (`auth.*`).
#
# The steps, each timed on the wall clock and bounded (`alarm`), a failure recorded and the run going on:
# 1. push: every changeset, bookmark and tag of SOURCE pushed in one push, the repository's first;
# 2. clone: `hg clone -U`; on the first run the clone is checked: `hg verify`, and its changeset ids and its tags equal
#    SOURCE's;
# 3. incremental: one changeset (one new file) made on SOURCE's tip in a scratch clone and pushed, then pulled into the
#    clone, which must then hold it;
# 4. stream: `hg clone -U --stream`, the server's store sent as files.
#
# hg runs outside every configuration file (HGRCPATH empty, HGPLAIN), never prompts, and speaks C. Everything is
# written under WORK, which is made empty and removed at the end; SOURCE is only read.
#
# Usage: [SERVERS="stock slates"] [SLATES=slates] hg_scale.sh HG SOURCE WORK [RUNS] [BOUND]
#   SERVERS the servers measured, in the order each run starts them (default "stock")
#   SLATES  the release `slates` binary (`cargo build --release --locked -p slates-cli`), for the slates server
#   HG      the `hg` executable (Mercurial 7.2 here: the `hg` of `uvx --from mercurial==7.2`'s environment)
#   SOURCE  a clone of the dataset (`hg clone -U https://www.mercurial-scm.org/repo/hg`)
#   WORK    a scratch directory, made empty here and removed at the end
#   RUNS    the runs (default 3)
#   BOUND   the seconds any one step may take before it is killed and recorded as such (default 1800)
set -uo pipefail

HG_BIN=$1
SOURCE=$(cd "$2" && pwd)
WORK=$3
RUNS=${4:-3}
BOUND=${5:-1800}
SERVERS=${SERVERS:-stock}
SLATES=${SLATES:-slates}

rm -rf "$WORK"
mkdir -p "$WORK/home" || exit 1
WORK=$(cd "$WORK" && pwd) || exit 1
RESULTS="$WORK/results.txt"
: > "$RESULTS"

# Stock hg, outside every configuration file and user, never prompting, in C's words: a command (`env` then `hg`), so
# a bounded step can exec it.
H=(env "HOME=$WORK/home" HGRCPATH= HGPLAIN=1 HGUSER="Test <test@example.com>" HGENCODING=utf-8 LC_ALL=C "$HG_BIN"
  --noninteractive)
h() { "${H[@]}" "$@"; }

# The wall clock, in seconds with milliseconds.
now() { perl -MTime::HiRes=time -e 'printf "%.3f\n", time'; }

# Runs the command after BOUND seconds' alarm is set: past it the command is killed by SIGALRM (status 142).
bounded() { perl -e 'alarm shift @ARGV; exec @ARGV or die "exec: $!\n"' "$BOUND" "$@"; }

# Times one step: its name, then its command. Records "name seconds status" in the results and prints it; the
# command's output is kept beside the results.
timed() {
  local name=$1 start end status
  shift
  start=$(now)
  if bounded "$@" > "$WORK/out.${name// /-}" 2> "$WORK/err.${name// /-}"; then status=ok; else status="failed($?)"; fi
  end=$(now)
  echo "$name $(perl -e 'printf "%.3f", $ARGV[1] - $ARGV[0]' "$start" "$end") $status" | tee -a "$RESULTS"
  if [ "$status" != ok ]; then tail -5 "$WORK/err.${name// /-}" | sed 's/^/    /'; fi
}

# A repository's changeset ids (sorted) and its tags, into $2.changesets and $2.tags.
listing() {
  h -R "$1" log -r 'all()' -T '{node}\n' | sort > "$2.changesets"
  h -R "$1" tags -T '{tag} {node}\n' | grep -v '^tip ' | sort > "$2.tags"
}

echo "dataset: $SOURCE at $(h -R "$SOURCE" log -r tip -T '{rev}:{node}'); $(h -R "$SOURCE" log -r 'all()' -T x | wc -c | tr -d ' ') changesets, $(h -R "$SOURCE" tags -T x | wc -c | tr -d ' ') tags, $(h -R "$SOURCE" bookmarks -T x | wc -c | tr -d ' ') bookmarks, $(h -R "$SOURCE" heads -T x | wc -c | tr -d ' ') heads"
listing "$SOURCE" "$WORK/source"
echo "hg: $(h --version | head -1); slates: $("$SLATES" --version 2> /dev/null || echo none); store: $(du -sh "$SOURCE/.hg/store" | cut -f1)"
echo "machine: $(sysctl -n machdep.cpu.brand_string 2> /dev/null), $(sysctl -n hw.ncpu 2> /dev/null) cores; load: $(uptime | sed 's/.*load averages*: //')"

# The daemon's resident set in MiB, or "-" where it is gone.
rss_of() { ps -o rss= -p "$1" 2> /dev/null | awk '{ printf "%.0f", $1 / 1024 }' || echo -; }

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

# The value of the first line of FILE matching the sed expression, or nothing.
first() { sed -n "$2" "$1" | head -1; }

# Starts a fresh slates anchor for run $1 and makes the repository and a token: sets URL, AUTH, ANCHOR, DAEMON.
start_slates() {
  local instance="w-perf-hg-$$-$1" log="$WORK/anchor-$1.log" exports token streak=0 port
  "$SLATES" --instance "$instance" anchor --vcs-http 127.0.0.1:0 > /dev/null 2> "$log" &
  ANCHOR=$!
  for _ in $(seq 1 1200); do
    if grep -q "^slates daemon: instance $instance serving on" "$log" \
      && grep -q '^slates anchor: issuer surface: export ' "$log"; then break; fi
    kill -0 "$ANCHOR" 2> /dev/null || { cat "$log"; return 1; }
    sleep 0.1
  done
  DAEMON=$(first "$log" 's/^slates anchor: instance .* up; daemon pid \([0-9]*\);.*/\1/p')
  port=$(first "$log" 's/^slates daemon: the version-control HTTP edge serves cleartext on 127\.0\.0\.1:\([0-9]*\).*/\1/p')
  exports=$(first "$log" 's/^slates anchor: issuer surface: export //p')
  echo "slates run $1: $(first "$log" 's/^\(slates daemon: instance .* serving on .*\)/\1/p')"
  for _ in $(seq 1 1200); do
    if "$SLATES" --instance "$instance" volume list > /dev/null 2>&1; then streak=$((streak + 1)); else streak=0; fi
    [ "$streak" -ge 10 ] && break
    sleep 0.1
  done
  "$SLATES" --instance "$instance" bootstrap root > /dev/null
  "$SLATES" --instance "$instance" repository create project --tool hg > /dev/null
  # shellcheck disable=SC2086 # the surface's NAME=VALUE pairs are words
  token=$(env $exports "$SLATES" --instance "$instance" credential token | sed -n 's/^token: //p')
  URL="http://127.0.0.1:$port/project"
  AUTH=(--config "auth.slates.prefix=http://127.0.0.1:$port/" --config auth.slates.username=slates
    --config "auth.slates.password=$token")
}
trap 'stop_servers; rm -rf "$WORK"' EXIT

# Starts `hg serve` over a fresh empty repository for run $1: sets URL and STOCK.
start_stock() {
  local root="$WORK/stock-$1" port
  h init "$root"
  port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
  h -R "$root" serve --address 127.0.0.1 --port "$port" --config web.push_ssl=False --config 'web.allow-push=*' \
    --accesslog "$WORK/stock-$1.access" --errorlog "$WORK/stock-$1.log" > /dev/null 2>&1 &
  STOCK=$!
  URL="http://127.0.0.1:$port/"
  AUTH=()
  for _ in $(seq 1 100); do h identify "$URL" > /dev/null 2>&1 && break; sleep 0.1; done
  echo "stock run $1: hg serve on 127.0.0.1:$port"
}

# Checks clone $1 of server $2 against SOURCE: verify, then changeset ids and tags.
check() {
  local clone=$1 server=$2
  if [ ! -d "$clone" ]; then
    echo "$server check skipped: no clone" | tee -a "$RESULTS"
    return 0
  fi
  timed "$server verify" "${H[@]}" -R "$clone" verify
  listing "$clone" "$WORK/$server"
  for kind in changesets tags; do
    if cmp -s "$WORK/source.$kind" "$WORK/$server.$kind"; then
      echo "$server check $kind identical ($(wc -l < "$WORK/source.$kind" | tr -d ' '))" | tee -a "$RESULTS"
    else
      echo "$server check $kind DIFFER: $(comm -3 "$WORK/source.$kind" "$WORK/$server.$kind" | wc -l | tr -d ' ') lines" \
        | tee -a "$RESULTS"
      comm -3 "$WORK/source.$kind" "$WORK/$server.$kind" | head -5 | sed 's/^/    /'
    fi
  done
}

# Runs the steps against the server started for run $2 ($1 its name).
steps() {
  local server=$1 run=$2 clone="$WORK/$1-clone-$2" scratch="$WORK/$1-scratch-$2" new
  timed "$server push $run" "${H[@]}" ${AUTH[@]+"${AUTH[@]}"} -R "$SOURCE" push -q "$URL"
  [ "$server" = slates ] && echo "slates daemon rss after push: $(rss_of "$DAEMON") MiB" | tee -a "$RESULTS"
  timed "$server clone $run" "${H[@]}" ${AUTH[@]+"${AUTH[@]}"} clone -q -U "$URL" "$clone"
  [ "$run" = 1 ] && check "$clone" "$server"
  h clone -q -r tip "$SOURCE" "$scratch"
  echo "one more changeset, $server run $run" > "$scratch/a-699-scale.txt"
  h -R "$scratch" commit -q -A -m "one more changeset" "$scratch/a-699-scale.txt"
  new=$(h -R "$scratch" log -r . -T '{node}')
  timed "$server incremental-push $run" "${H[@]}" ${AUTH[@]+"${AUTH[@]}"} -R "$scratch" push -q -r "$new" "$URL"
  timed "$server incremental-pull $run" "${H[@]}" ${AUTH[@]+"${AUTH[@]}"} -R "$clone" pull -q "$URL"
  if [ "$(h -R "$clone" log -r "$new" -T '{node}' 2> /dev/null)" = "$new" ]; then
    echo "$server incremental check pulled" | tee -a "$RESULTS"
  else
    echo "$server incremental check MISSED" | tee -a "$RESULTS"
  fi
  timed "$server stream $run" "${H[@]}" ${AUTH[@]+"${AUTH[@]}"} clone -q -U --stream "$URL" "$WORK/$server-stream-$run"
  [ "$server" = slates ] && echo "slates daemon rss at the end: $(rss_of "$DAEMON") MiB" | tee -a "$RESULTS"
  rm -rf "$clone" "$scratch" "$WORK/$server-stream-$run"
}

for run in $(seq 1 "$RUNS"); do
  for server in $SERVERS; do
    case $server in
      stock)
        start_stock "$run"
        steps stock "$run"
        stop_servers
        rm -rf "$WORK/stock-$run"
        ;;
      slates)
        if start_slates "$run"; then steps slates "$run"; else echo "slates run $run: the anchor did not start" | tee -a "$RESULTS"; fi
        stop_servers
        ;;
      *) echo "unknown server $server" >&2; exit 1 ;;
    esac
  done
done

echo "== results (seconds; every run shown)"
for step in push clone incremental-push incremental-pull stream; do
  for server in $SERVERS; do
  printf '%-18s %-6s' "$step" "$server"
  grep "^$server $step " "$RESULTS" | awk '{ printf " %s%s", $4, ($5 == "ok" ? "" : "(" $5 ")") }
    $5 == "ok" && (best == "" || $4 < best) { best = $4 } END { printf "  best %s", (best == "" ? "-" : best) }'
  echo
  done
done
echo "load at the end: $(uptime | sed 's/.*load averages*: //')"
