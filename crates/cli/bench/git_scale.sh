#!/usr/bin/env bash
# A-699 (UI goal 93; goal 6's first measurement): stock git against a real slates anchor's `--vcs-http` edge with a
# large real repository, every step timed against stock git serving the same repository. `docs/wip/BENCHMARKS.md`
# records each run of it; the command there is the contract.
#
# The servers, a fresh one for every run, the two alternating (stock first):
# - slates: `slates anchor --vcs-http 127.0.0.1:0` from the release build, its daemon's shards and budget derived from
#   this machine (no `--quick`, no `--shards`); a git repository made by `slates repository create --tool git` with
#   SOURCE's default branch; a token from `slates credential token`, run with the anchor's issuer surface, which git
#   sends as a bearer credential (`http.extraHeader`);
# - stock: `git daemon` serving a fresh bare repository over git:// on loopback, receive-pack enabled and
#   `uploadpack.allowFilter` set, as a stock server that offers partial clones is configured.
#
# The steps, each timed on the wall clock and bounded (`alarm`), a failure recorded and the run going on:
# 1. push: every branch and tag of SOURCE pushed in one push, the repository's first;
# 2. clone: `git clone --mirror`; on the first run of each server the clone is checked: `git fsck --full` and its refs
#    and every object id (`git rev-list --all --objects`, ids sorted) equal SOURCE's;
# 3. incremental: one commit (one new file) made on SOURCE's default branch in a scratch copy and pushed, then fetched
#    into the mirror clone, which must then name it;
# 4. shallow: `git clone --bare --depth 1`;
# 5. partial: `git clone --bare --filter=blob:none`.
#
# git runs outside every configuration file and user (no system or global configuration, its home the scratch
# directory), never prompts, and speaks C. Everything is written under WORK, which is made empty and removed at the end;
# SOURCE is only read.
#
# Usage: git_scale.sh SLATES SOURCE WORK [RUNS] [BOUND]
#   SLATES  the release `slates` binary (`cargo build --release --locked -p slates-cli`)
#   SOURCE  a bare clone of the dataset (`git clone --bare https://github.com/git/git.git`)
#   WORK    a scratch directory, made empty here and removed at the end
#   RUNS    the runs of each server (default 3)
#   BOUND   the seconds any one step may take before it is killed and recorded as such (default 1800)
# SERVERS in the environment names the servers measured, in the order each run starts them (default "stock slates";
# "stock" alone takes the stock baseline, and SLATES may then be "-").
set -uo pipefail

SLATES=$1
SOURCE=$(cd "$2" && pwd)
WORK=$3
RUNS=${4:-3}
BOUND=${5:-1800}
SERVERS=${SERVERS:-stock slates}

rm -rf "$WORK"
mkdir -p "$WORK/home" || exit 1
WORK=$(cd "$WORK" && pwd) || exit 1
RESULTS="$WORK/results.txt"
: > "$RESULTS"

export GIT_AUTHOR_NAME=Test GIT_AUTHOR_EMAIL=test@example.com GIT_COMMITTER_NAME=Test GIT_COMMITTER_EMAIL=test@example.com
unset GIT_ASKPASS SSH_ASKPASS GIT_DIR GIT_WORK_TREE GIT_PROTOCOL GIT_CONFIG_PARAMETERS

# Stock git, outside every configuration file and user, never prompting, in C's words: a command (`env` then `git`),
# so a bounded step can exec it.
G=(env "HOME=$WORK/home" GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_COUNT=0 GIT_TERMINAL_PROMPT=0
  LC_ALL=C git)
g() { "${G[@]}" "$@"; }

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

# The daemon's resident set in MiB, or "-" where it is gone.
rss_of() { ps -o rss= -p "$1" 2> /dev/null | awk '{ printf "%.0f", $1 / 1024 }' || echo -; }

DEFAULT=$(g -C "$SOURCE" symbolic-ref --short HEAD)
echo "dataset: $SOURCE at $(g -C "$SOURCE" rev-parse HEAD) ($DEFAULT); $(g -C "$SOURCE" for-each-ref refs/heads | wc -l | tr -d ' ') branches, $(g -C "$SOURCE" for-each-ref refs/tags | wc -l | tr -d ' ') tags, $(g -C "$SOURCE" rev-list --all --count) commits"
g -C "$SOURCE" for-each-ref --format='%(objectname) %(refname)' refs/heads refs/tags | sort > "$WORK/source.refs"
g -C "$SOURCE" rev-list --all --objects | cut -d' ' -f1 | sort > "$WORK/source.objects"
echo "objects: $(wc -l < "$WORK/source.objects" | tr -d ' '); git: $(git --version); slates: $("$SLATES" version 2> /dev/null || echo none)"
echo "machine: $(sysctl -n machdep.cpu.brand_string 2> /dev/null), $(sysctl -n hw.ncpu 2> /dev/null) cores; load: $(uptime | sed 's/.*load averages*: //')"

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
# KEEP_LOGS in the environment names a directory the servers' logs and each step's output are copied into at the end.
trap 'stop_servers; [ -n "${KEEP_LOGS:-}" ] && mkdir -p "$KEEP_LOGS" && cp "$WORK"/*.log "$WORK"/err.* "$WORK"/results.txt "$KEEP_LOGS"/ 2> /dev/null; rm -rf "$WORK"' EXIT

# The value of the first line of FILE matching the sed expression, or nothing.
first() { sed -n "$2" "$1" | head -1; }

# Starts a fresh slates anchor for run $1 and makes the repository and a token: sets URL, AUTH, ANCHOR, DAEMON.
start_slates() {
  local instance="g309-scale-$$-$1" log="$WORK/anchor-$1.log" exports token streak=0
  "$SLATES" --instance "$instance" anchor --vcs-http 127.0.0.1:0 > /dev/null 2> "$log" &
  ANCHOR=$!
  for _ in $(seq 1 1200); do
    if grep -q "^slates daemon: instance $instance serving on" "$log" \
      && grep -q '^slates anchor: issuer surface: export ' "$log"; then break; fi
    kill -0 "$ANCHOR" 2> /dev/null || { cat "$log"; return 1; }
    sleep 0.1
  done
  DAEMON=$(first "$log" 's/^slates anchor: instance .* up; daemon pid \([0-9]*\);.*/\1/p')
  local port
  port=$(first "$log" 's/^slates daemon: the version-control HTTP edge serves cleartext on 127\.0\.0\.1:\([0-9]*\).*/\1/p')
  exports=$(first "$log" 's/^slates anchor: issuer surface: export //p')
  echo "slates run $1: $(first "$log" 's/^\(slates daemon: instance .* serving on .*\)/\1/p')"
  for _ in $(seq 1 1200); do
    if "$SLATES" --instance "$instance" volume list > /dev/null 2>&1; then streak=$((streak + 1)); else streak=0; fi
    [ "$streak" -ge 10 ] && break
    sleep 0.1
  done
  "$SLATES" --instance "$instance" bootstrap root > /dev/null
  "$SLATES" --instance "$instance" repository create project --tool git --default-branch "$DEFAULT" > /dev/null
  # shellcheck disable=SC2086 # the surface's NAME=VALUE pairs are words
  token=$(env $exports "$SLATES" --instance "$instance" credential token | sed -n 's/^token: //p')
  URL="http://127.0.0.1:$port/project"
  AUTH=(-c "http.extraHeader=Authorization: Bearer $token")
}

# Starts `git daemon` over a fresh bare repository for run $1: sets URL, AUTH, STOCK.
start_stock() {
  local root="$WORK/stock-$1" port
  mkdir -p "$root"
  g init -q --bare -b "$DEFAULT" "$root/project.git"
  g -C "$root/project.git" config uploadpack.allowFilter true
  port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
  g daemon --reuseaddr --listen=127.0.0.1 --port="$port" --base-path="$root" --export-all --enable=receive-pack \
    "$root" > /dev/null 2> "$WORK/stock-$1.log" &
  STOCK=$!
  URL="git://127.0.0.1:$port/project.git"
  AUTH=()
  for _ in $(seq 1 100); do g ls-remote "$URL" > /dev/null 2>&1 && break; sleep 0.1; done
  echo "stock run $1: git daemon on 127.0.0.1:$port"
}

# Checks clone $1 of server $2 against SOURCE: fsck, then refs and object ids; a clone that is not there is recorded.
check() {
  local mirror=$1 server=$2
  if [ ! -d "$mirror" ]; then
    echo "$server check skipped: no clone" | tee -a "$RESULTS"
    return 0
  fi
  timed "$server fsck" "${G[@]}" -C "$mirror" fsck --full
  g -C "$mirror" for-each-ref --format='%(objectname) %(refname)' refs/heads refs/tags | sort > "$WORK/$server.refs"
  g -C "$mirror" rev-list --all --objects | cut -d' ' -f1 | sort > "$WORK/$server.objects"
  for kind in refs objects; do
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
  local server=$1 run=$2 mirror="$WORK/$1-mirror-$2" scratch="$WORK/$1-scratch-$2.git" blob tree new
  timed "$server push $run" "${G[@]}" ${AUTH[@]+"${AUTH[@]}"} -C "$SOURCE" push -q "$URL" 'refs/heads/*:refs/heads/*' \
    'refs/tags/*:refs/tags/*'
  [ "$server" = slates ] && echo "slates daemon rss after push: $(rss_of "$DAEMON") MiB" | tee -a "$RESULTS"
  timed "$server clone $run" "${G[@]}" ${AUTH[@]+"${AUTH[@]}"} clone -q --mirror "$URL" "$mirror"
  [ "$run" = 1 ] && check "$mirror" "$server"
  g clone -q --bare "$SOURCE" "$scratch"
  blob=$(echo "one more commit, $server run $run" | g -C "$scratch" hash-object -w --stdin)
  GIT_INDEX_FILE="$WORK/index" g -C "$scratch" read-tree HEAD
  GIT_INDEX_FILE="$WORK/index" g -C "$scratch" update-index --add --cacheinfo "100644,$blob,a-699-scale.txt"
  tree=$(GIT_INDEX_FILE="$WORK/index" g -C "$scratch" write-tree)
  rm -f "$WORK/index"
  new=$(g -C "$scratch" commit-tree "$tree" -p HEAD -m "one more commit")
  timed "$server incremental-push $run" "${G[@]}" ${AUTH[@]+"${AUTH[@]}"} -C "$scratch" push -q "$URL" "$new:refs/heads/$DEFAULT"
  timed "$server incremental-fetch $run" "${G[@]}" ${AUTH[@]+"${AUTH[@]}"} -C "$mirror" fetch -q
  if [ "$(g -C "$mirror" rev-parse "refs/heads/$DEFAULT" 2> /dev/null)" = "$new" ]; then
    echo "$server incremental check fetched" | tee -a "$RESULTS"
  else
    echo "$server incremental check MISSED" | tee -a "$RESULTS"
  fi
  timed "$server shallow $run" "${G[@]}" ${AUTH[@]+"${AUTH[@]}"} clone -q --bare --depth 1 "$URL" "$WORK/$server-shallow-$run"
  timed "$server partial $run" "${G[@]}" ${AUTH[@]+"${AUTH[@]}"} clone -q --bare --filter=blob:none "$URL" "$WORK/$server-partial-$run"
  [ "$server" = slates ] && echo "slates daemon rss at the end: $(rss_of "$DAEMON") MiB" | tee -a "$RESULTS"
  rm -rf "$mirror" "$scratch" "$WORK/$server-shallow-$run" "$WORK/$server-partial-$run"
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
for step in push clone incremental-push incremental-fetch shallow partial; do
  for server in $SERVERS; do
    printf '%-18s %-6s' "$step" "$server"
    grep "^$server $step " "$RESULTS" | awk '{ printf " %s%s", $4, ($5 == "ok" ? "" : "(" $5 ")") }
      $5 == "ok" && (best == "" || $4 < best) { best = $4 } END { printf "  best %s", (best == "" ? "-" : best) }'
    echo
  done
done
echo "load at the end: $(uptime | sed 's/.*load averages*: //')"
