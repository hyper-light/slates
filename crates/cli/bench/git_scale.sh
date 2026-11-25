#!/usr/bin/env bash
# A-699 (UI goal 93; goal 6's first measurement): stock git against a real slates anchor's `--vcs-http` edge with a
# large real repository, every step timed against stock git serving the same repository. `docs/wip/BENCHMARKS.md`
# records each run of it; the command there is the contract.
#
# The servers, a fresh one for every run, the two alternating (stock first):
# - slates: `slates anchor --vcs-http 127.0.0.1:0 --vcs-store WORK/store-N` from the release build (its repositories kept
#   on the node's changeset store, which a git repository needs since C2), its daemon's shards and budget derived from
#   this machine (no `--quick`, no `--shards`); a git repository made by `slates repository create --tool git` with
#   SOURCE's default branch; a token from `slates credential token`, run with the anchor's issuer surface, which git
#   sends as a bearer credential (`http.extraHeader`);
# - stock: `git daemon` serving a fresh bare repository over git:// on loopback, receive-pack enabled and
#   `uploadpack.allowFilter` set, as a stock server that offers partial clones is configured.
# - stock-http: Apache httpd (HTTPD, default /usr/sbin/httpd; its modules in HTTPD_MODULES, default
#   /usr/libexec/apache2) running `git http-backend` as a CGI over a fresh bare repository on loopback, git's smart HTTP
#   as git-http-backend(1) configures it for Apache 2 ("ScriptAlias /git/ .../git-http-backend/", `GIT_PROJECT_ROOT`,
#   `GIT_HTTP_EXPORT_ALL`), receive-pack on (`http.receivepack`) and `uploadpack.allowFilter` set: the stock server
#   slates' own HTTP edge is the counterpart of (§4.17.1's method, "git-http-backend").
#
# The steps, each timed on the wall clock and bounded (`alarm`), a failure recorded and the run going on:
# 1. push: every branch and tag of SOURCE pushed in one push, the repository's first;
# 2. clone: `git clone --mirror`; on the first run of each server the clone is checked: `git fsck --full` and its refs
#    and every object id (`git rev-list --all --objects`, ids sorted) equal SOURCE's; then a second mirror clone
#    (`clone-again`), what every later clone meets once the server kept what the first made;
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
# SAMPLE=1 adds a profiling pass after the timed runs: one more slates push and clone, each sampled by macOS's `sample`.
# SERVERS in the environment names the servers measured, in the order each run starts them (default "stock slates";
# "stock" alone takes the stock baseline, and SLATES may then be "-"; "stock-http" adds git-http-backend under Apache).
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
  LAST_START=$start
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
# The refs the first push sends, which every clone's refs must equal: none would make that check vacuous.
[ -s "$WORK/source.refs" ] || { echo "FAILED: $SOURCE has no branch or tag to push"; exit 1; }
# What the first push sends: the objects reachable from the branches and tags, not every ref of the source (a clone of
# git/git also has refs no push here sends, and r655's check read 436,736 objects "missing" on every server, stock too).
g -C "$SOURCE" rev-list --objects --branches --tags | cut -d' ' -f1 | sort > "$WORK/source.objects"
echo "objects: $(wc -l < "$WORK/source.objects" | tr -d ' '); git: $(git --version); slates: $("$SLATES" version 2> /dev/null || echo none)"
echo "machine: $(sysctl -n machdep.cpu.brand_string 2> /dev/null), $(sysctl -n hw.ncpu 2> /dev/null) cores; load: $(uptime | sed 's/.*load averages*: //')"

ANCHOR="" DAEMON="" STOCK="" INSTANCE=""
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
trap 'stop_servers; [ -n "${KEEP_LOGS:-}" ] && mkdir -p "$KEEP_LOGS" && cp "$WORK"/*.log "$WORK"/err.* "$WORK"/status.*.json "$WORK"/trace.* "$WORK"/sample-*.txt "$WORK"/results.txt "$KEEP_LOGS"/ 2> /dev/null; rm -rf "$WORK"' EXIT

# The value of the first line of FILE matching the sed expression, or nothing.
first() { sed -n "$2" "$1" | head -1; }

# The counters w-git reads (`git_interop.rs` `counters_of`), its list of 2026-11-24 04:55 (main 01a20182): the upload's
# parts and reuse, the deflate states, the encode, and what was brought into memory, so a slates clone's 127 s against
# stock's 10.7 (r811) and its later fetches' slowdown can be told apart by what the daemon did.
COUNTERS=$(printf 'vcs_store.git_upload.%s ' parts_copied parts_by_entry entries_judged entries_copied headers_rewritten \
  made_whole loose_whole loose_deltas loose_unsearched reused_whole reused_deltas reuse_refused reuse_no_entry \
  reuse_unread reuse_crc_missed reuse_rebuilt backward_reads stops position_lookups position_cache_hits \
  position_cache_conflicts position_cache_unroomed
  printf 'vcs_store.git_deflate.%s ' states_made states_reused states_dropped
  printf 'vcs_store.git_encode.%s ' begun encoded turns behind ahead_stopped reused_whole reused_deltas reuse_refused \
    units_past_budget
  printf 'vcs_store.git_resident.%s ' stopped_brought ahead_brought let_go_behind bring_back_past_patience)

# Slates' daemon at `$1` (before or after) step $2 of run $3: its `status --json` kept beside the logs
# (status.WHEN-STEP-RUN.json), and a line of COUNTERS, each summed over every shard's report of it as `counters_of` sums
# them. Read outside the step's timing, so it costs the step nothing.
counters_at() {
  local file="$WORK/status.$1-$2-$3.json"
  if ! "$SLATES" --instance "$INSTANCE" status --json > "$file" 2> /dev/null; then
    echo "slates counters $1 $2 $3: status refused" | tee -a "$RESULTS"
    return 0
  fi
  # shellcheck disable=SC2086 # the counters' names are words
  python3 -I - "$file" $COUNTERS << 'PY' | sed "s/^/slates counters $1 $2 $3: /" | tee -a "$RESULTS"
import json, sys
names = sys.argv[2:]
sums = dict.fromkeys(names, 0)
def walk(node):
    if isinstance(node, dict):
        if node.get("kind") in sums and isinstance(node.get("count"), int):
            sums[node["kind"]] += node["count"]
        for value in node.values():
            walk(value)
    elif isinstance(node, list):
        for value in node:
            walk(value)
walk(json.load(open(sys.argv[1])))
print(" ".join(f"{name.split('vcs_store.git_')[-1]}={sums[name]}" for name in names))
PY
}

# Slates' first bytes of step $1 of run $2, from its clone's packet trace $3 (`GIT_TRACE_PACKET`, which stops at the
# pack's first bytes): the first packet received (the ref advertisement) and the pack's first bytes, each in seconds
# after the step began (LAST_START), beside the step's total.
first_bytes() {
  python3 -I - "$3" "$LAST_START" << 'PY' | sed "s/^/slates first bytes of $1 $2: /" | tee -a "$RESULTS"
import datetime, re, sys
trace, start = sys.argv[1], float(sys.argv[2])
day = datetime.datetime.fromtimestamp(start).date()
def at(line):
    hours, minutes, seconds = line.split(" ", 1)[0].split(":")
    moment = datetime.datetime.combine(day, datetime.time(int(hours), int(minutes))).timestamp() + float(seconds)
    return moment if moment >= start - 1 else moment + 86400
first = pack = None
try:
    lines = open(trace, errors="replace")
except OSError:
    lines = []
for line in lines:
    if re.search(r"packet: +\S+<", line):
        if first is None:
            first = at(line)
        if "PACK" in line:
            pack = at(line)
            break
show = lambda moment: "none" if moment is None else f"{moment - start:.3f} s"
print(f"first packet {show(first)}, first pack bytes {show(pack)}")
PY
}

# Times clone step $1 of run $2 into $3, `git clone -q` taking the arguments after and the server's URL. For slates, the
# daemon's counters before and after (`counters_at`) and its first bytes (`first_bytes`) too.
clone_timed() {
  local step=$1 run=$2 dest=$3 trace="$WORK/trace.$1-$2"
  shift 3
  if [ "$server" != slates ]; then
    timed "$server $step $run" "${G[@]}" ${AUTH[@]+"${AUTH[@]}"} clone -q "$@" "$URL" "$dest"
    return
  fi
  counters_at before "$step" "$run"
  timed "$server $step $run" env "GIT_TRACE_PACKET=$trace" "${G[@]}" ${AUTH[@]+"${AUTH[@]}"} clone -q "$@" "$URL" "$dest"
  counters_at after "$step" "$run"
  first_bytes "$step" "$run" "$trace"
}

# Starts a fresh slates anchor for run $1 and makes the repository and a token: sets URL, AUTH, ANCHOR, DAEMON and
# INSTANCE.
start_slates() {
  local instance="g309-scale-$$-$1" log="$WORK/anchor-$1.log" exports token streak=0
  INSTANCE=$instance
  mkdir -p "$WORK/store-$1"
  "$SLATES" --instance "$instance" anchor --vcs-http 127.0.0.1:0 --vcs-store "$WORK/store-$1" > /dev/null 2> "$log" &
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
  "$SLATES" --instance "$instance" repository create project --tool git --default-branch "$DEFAULT" > /dev/null \
    || { echo "slates run $1: repository create refused" | tee -a "$RESULTS"; return 1; }
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
  # `git-daemon` itself, `env` execing it in place, so STOCK is the daemon: `git daemon` starts it as a child that
  # killing the wrapper missed, and r655's samples held a stray daemon from the git leg onward.
  env "HOME=$WORK/home" GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_COUNT=0 LC_ALL=C \
    "$(git --exec-path)/git-daemon" --reuseaddr --listen=127.0.0.1 --port="$port" --base-path="$root" --export-all \
    --enable=receive-pack "$root" > /dev/null 2> "$WORK/stock-$1.log" &
  STOCK=$!
  URL="git://127.0.0.1:$port/project.git"
  AUTH=()
  for _ in $(seq 1 100); do g ls-remote "$URL" > /dev/null 2>&1 && break; sleep 0.1; done
  echo "stock run $1: git daemon on 127.0.0.1:$port"
}

# Starts Apache httpd serving a fresh bare repository through `git http-backend` for run $1 (the header's stock-http):
# sets URL, AUTH, STOCK. Prefork, since mod_cgi runs its scripts from the serving process; the server runs as this user
# in the foreground, its configuration, pid and logs in its own directory under WORK.
start_stock_http() {
  local root="$WORK/stock-http-$1" port modules backend
  mkdir -p "$root/logs"
  g init -q --bare -b "$DEFAULT" "$root/project.git"
  g -C "$root/project.git" config uploadpack.allowFilter true
  g -C "$root/project.git" config http.receivepack true
  port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
  modules=${HTTPD_MODULES:-/usr/libexec/apache2}
  backend="$(git --exec-path)/git-http-backend"
  cat > "$root/httpd.conf" <<CONF
ServerRoot "$root"
ServerName 127.0.0.1
Listen 127.0.0.1:$port
PidFile "$root/logs/httpd.pid"
ErrorLog "$root/logs/error.log"
LoadModule mpm_prefork_module $modules/mod_mpm_prefork.so
LoadModule authz_core_module $modules/mod_authz_core.so
LoadModule unixd_module $modules/mod_unixd.so
LoadModule alias_module $modules/mod_alias.so
LoadModule env_module $modules/mod_env.so
LoadModule cgi_module $modules/mod_cgi.so
SetEnv GIT_PROJECT_ROOT "$root"
SetEnv GIT_HTTP_EXPORT_ALL 1
SetEnv GIT_CONFIG_NOSYSTEM 1
SetEnv HOME "$WORK/home"
ScriptAlias /git/ "$backend/"
<Directory "$(dirname "$backend")">
  Require all granted
</Directory>
CONF
  # In a session of its own: prefork signals its whole process group as it shuts down, which took this harness with it.
  perl -MPOSIX -e 'POSIX::setsid(); exec @ARGV or die "exec: $!\n"' "${HTTPD:-/usr/sbin/httpd}" -f "$root/httpd.conf" \
    -DFOREGROUND > /dev/null 2> "$WORK/stock-http-$1.log" &
  STOCK=$!
  URL="http://127.0.0.1:$port/git/project.git"
  AUTH=()
  for _ in $(seq 1 100); do g ls-remote "$URL" > /dev/null 2>&1 && break; sleep 0.1; done
  echo "stock-http run $1: httpd $("${HTTPD:-/usr/sbin/httpd}" -v | sed -n 's/^Server version: //p') with git http-backend on 127.0.0.1:$port"
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
  [ "$server" = slates ] && counters_at after push "$run"
  clone_timed clone "$run" "$mirror" --mirror
  [ "$run" = 1 ] && check "$mirror" "$server"
  # A second clone: what every clone after the first meets, once a server has kept what the first one made (A-718).
  clone_timed clone-again "$run" "$WORK/$server-again-$run" --mirror
  rm -rf "$WORK/$server-again-$run"
  g clone -q --bare "$SOURCE" "$scratch"
  blob=$(echo "one more commit, $server run $run" | g -C "$scratch" hash-object -w --stdin)
  GIT_INDEX_FILE="$WORK/index" g -C "$scratch" read-tree HEAD
  GIT_INDEX_FILE="$WORK/index" g -C "$scratch" update-index --add --cacheinfo "100644,$blob,a-699-scale.txt"
  tree=$(GIT_INDEX_FILE="$WORK/index" g -C "$scratch" write-tree)
  rm -f "$WORK/index"
  new=$(g -C "$scratch" commit-tree "$tree" -p HEAD -m "one more commit")
  timed "$server incremental-push $run" "${G[@]}" ${AUTH[@]+"${AUTH[@]}"} -C "$scratch" push -q "$URL" "$new:refs/heads/$DEFAULT"
  timed "$server incremental-fetch $run" "${G[@]}" ${AUTH[@]+"${AUTH[@]}"} -C "$mirror" fetch -q
  [ "$server" = slates ] && counters_at after incremental-fetch "$run"
  if [ "$(g -C "$mirror" rev-parse "refs/heads/$DEFAULT" 2> /dev/null)" = "$new" ]; then
    echo "$server incremental check fetched" | tee -a "$RESULTS"
  else
    echo "$server incremental check MISSED" | tee -a "$RESULTS"
  fi
  timed "$server shallow $run" "${G[@]}" ${AUTH[@]+"${AUTH[@]}"} clone -q --bare --depth 1 "$URL" "$WORK/$server-shallow-$run"
  [ "$server" = slates ] && counters_at after shallow "$run"
  clone_timed partial "$run" "$WORK/$server-partial-$run" --bare --filter=blob:none
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
      stock-http)
        start_stock_http "$run"
        steps stock-http "$run"
        stop_servers
        rm -rf "$WORK/stock-http-$run"
        ;;
      slates)
        if start_slates "$run"; then steps slates "$run"; else echo "slates run $run: the anchor did not start or its repository was refused" | tee -a "$RESULTS"; fi
        stop_servers
        ;;
      *) echo "unknown server $server" >&2; exit 1 ;;
    esac
  done
done

# Runs `$@` (a slates push or clone, the profiling pass's) with the daemon sampled by macOS's `sample` for 30 s from
# 10 s into it, the call tree kept as sample-$1.txt beside the logs, and the daemon's counters before and after it.
sampled() {
  local step=$1 running
  shift
  counters_at before "$step" profile
  "$@" &
  running=$!
  sleep 10
  if ! sample "$DAEMON" 30 -file "$WORK/sample-$step.txt" > /dev/null 2>&1; then
    echo "profile: sample of the $step refused (macOS's sample, daemon pid $DAEMON)"
  fi
  wait "$running" || echo "profile: the $step itself failed ($?)"
  counters_at after "$step" profile
  echo "profile: the daemon's call tree over the $step in sample-$step.txt"
}

# The profiling pass (SAMPLE=1), apart from the timed runs so it cannot skew them: one more slates server, a push of the
# repository, then a clone of it, each sampled (`sampled`). The push's sample is w-svn's check of A-900 (r244's push
# profile spent ~10,000 samples in 30 s in memmove under ReceivePack::decide_step's lineage lend); the clone's is w-git's
# (a slates clone's 127 s against stock's 10.7, r811).
if [ "${SAMPLE:-}" = 1 ] && [[ " $SERVERS " == *" slates "* ]]; then
  echo "== profile: one more slates push and clone, sampled (not among the timed runs)"
  server=slates
  if start_slates profile; then
    sampled push "${G[@]}" ${AUTH[@]+"${AUTH[@]}"} -C "$SOURCE" push -q "$URL" 'refs/heads/*:refs/heads/*' \
      'refs/tags/*:refs/tags/*'
    sampled clone "${G[@]}" ${AUTH[@]+"${AUTH[@]}"} clone -q --mirror "$URL" "$WORK/profile-mirror"
  fi
  stop_servers
  rm -rf "$WORK/profile-mirror"
fi

echo "== results (seconds; every run shown)"
FAILED_ROWS=""
for step in push clone clone-again incremental-push incremental-fetch shallow partial; do
  grep -qE "^[^ ]+ $step " "$RESULTS" || continue
  for server in $SERVERS; do
    # A step only another server runs (svn's load is stock's, its import slates') is no line for this one.
    grep -q "^$server $step " "$RESULTS" || continue
    printf '%-18s %-10s' "$step" "$server"
    grep "^$server $step " "$RESULTS" | awk '{ printf " %s%s", $4, ($5 == "ok" ? "" : "(" $5 ")") }
      $5 == "ok" && (best == "" || $4 < best) { best = $4 } END { printf "  %s", (best == "" ? "FAILED" : "best " best) }'
    echo
    if ! grep "^$server $step " "$RESULTS" | grep -q " ok$"; then FAILED_ROWS="$FAILED_ROWS $server:$step"; fi
  done
done
echo "load at the end: $(uptime | sed 's/.*load averages*: //')"
# A step a server ran in no run is no row; a step whose every run failed is FAILED, and fails the harness: a leg that
# could not run must never print as a best of "-" (main's r592, every slates git and svn step failed and the round
# exited 0). A server that timed no step at all (its setup refused) fails it too, though it left no row to mark.
for server in $SERVERS; do
  grep -qE "^$server [^ ]+ [0-9]+ [0-9.]+ " "$RESULTS" || FAILED_ROWS="$FAILED_ROWS $server:every-step"
done
# A check that found the served data unlike the source (refs, objects, changesets, tree or log DIFFER; an incremental
# change MISSED) fails the harness too: a check that only prints could let a dropped ref pass (main 01a20182, after
# r655 narrowed git's object check to what the push sends).
CHECKS_FAILED=$(grep -E '^[^ ]+ (incremental )?check .*(DIFFER|MISSED)' "$RESULTS" | sort -u | tr '\n' ';')
if [ -n "$FAILED_ROWS" ] || [ -n "$CHECKS_FAILED" ]; then
  [ -n "$FAILED_ROWS" ] && echo "FAILED: every run failed for$FAILED_ROWS"
  [ -n "$CHECKS_FAILED" ] && echo "FAILED: a check found the served data unlike the source: $CHECKS_FAILED"
  exit 1
fi

