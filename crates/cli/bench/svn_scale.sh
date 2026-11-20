#!/usr/bin/env bash
# A-699 (UI goal 93): stock Subversion serving a large real repository, every step timed: the baseline slates'
# Subversion site is measured against. `docs/wip/BENCHMARKS.md` records each run of it; the command there is the
# contract.
#
# The dataset: a dump of git/git's newest first-parent commits under trunk/ (`git_to_svn_dump.py`), so Subversion is
# measured over the same commits as git. (Stock `svnadmin load` writes about 0.15 s a revision on the Studio, flushing
# each, so all 24,547 would take an hour a load.)
#
# The servers, a fresh one for every run, in SERVERS' order:
# - stock: `svnserve -d` over svn:// on loopback over the repository the run loaded, anonymous write allowed
#   (`anon-access = write`), as a stock server that takes commits without accounts is configured;
# - slates: `slates anchor --vcs-svn 127.0.0.1:0` from the release build, a Subversion repository made by `slates
#   repository create --tool svn` and filled by `slates repository import` of the dump (timed as "import", every run),
#   and a token from `slates credential token` (run with the anchor's issuer surface), its id and the token the user and
#   password svn signs in with (CRAM-MD5).
#
# The steps, each timed on the wall clock and bounded (`alarm`), a failure recorded and the run going on:
# 1. load: `svnadmin load` of the whole dump into a fresh repository (Subversion's own import of a history), timed
#    once, on the first run; every later run serves its own copy of that repository (`cp -c`, APFS's clone);
# 2. checkout: `svn checkout` of trunk at its youngest revision; on the first run the working copy is checked: every
#    regular file's git blob id and path equal those of SOURCE's tree at REF (links and submodules left out; ids, not a
#    checkout, since git's own checkout rewrites `eol=crlf` files);
# 3. log: `svn log -q` of the whole history, which must name every revision;
# 4. incremental: one new file committed from a copy of the working copy (`commit`), then the working copy updated to
#    it (`update`), which must then hold it;
# 5. update-1000: the working copy taken back 1,000 revisions (untimed), then updated to the youngest.
#
# svn runs with its own empty configuration directory, never prompts, and speaks C. Everything is written under WORK,
# which is made empty and removed at the end; DUMP and SOURCE are only read.
#
# Usage: [SERVERS="stock slates"] [SLATES=slates] svn_scale.sh DUMP SOURCE REF WORK [RUNS] [BOUND]
#   SERVERS    the servers measured, in the order each run starts them (default "stock")
#   SLATES     the release `slates` binary (`cargo build --release --locked -p slates-cli`), for the slates server
#   DUMP       the dataset's dump (`git_to_svn_dump.py SOURCE REF 5000 > DUMP`)
#   SOURCE     the git repository the dump was made from, and REF its tip, to check a checkout against
#   WORK       a scratch directory, made empty here and removed at the end
#   RUNS       the runs (default 3)
#   BOUND      the seconds any one step may take before it is killed and recorded as such (default 3600)
set -uo pipefail

DUMP=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
SOURCE=$(cd "$2" && pwd)
REF=$3
WORK=$4
RUNS=${5:-3}
BOUND=${6:-3600}
SERVERS=${SERVERS:-stock}
SLATES=${SLATES:-slates}
UPDATE_SPAN=1000

rm -rf "$WORK"
mkdir -p "$WORK/home" "$WORK/config" || exit 1
WORK=$(cd "$WORK" && pwd) || exit 1
RESULTS="$WORK/results.txt"
: > "$RESULTS"

# Stock svn, outside every configuration file and user, never prompting, in C's words: a command (`env` then `svn`),
# so a bounded step can exec it.
S=(env "HOME=$WORK/home" LC_ALL=C svn --non-interactive --no-auth-cache --config-dir "$WORK/config")
# The server's credentials, set as it starts (none for stock's anonymous access).
AUTH=()
s() { "${S[@]}" ${AUTH[@]+"${AUTH[@]}"} "$@"; }
A=(env "HOME=$WORK/home" LC_ALL=C svnadmin)

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

git -C "$SOURCE" ls-tree -r --format='%(objectmode) %(objectname) %(path)' "$REF" | sed -n 's/^100[0-7]* //p' \
  | LC_ALL=C sort > "$WORK/source.files"
echo "dataset: $DUMP, $(du -h "$DUMP" | cut -f1); $SOURCE at $(git -C "$SOURCE" rev-parse "$REF"), $(wc -l < "$WORK/source.files" | tr -d ' ') regular files"
echo "svn: $(svn --version --quiet); slates: $("$SLATES" version 2> /dev/null || echo none); svnserve: $(svnserve --version --quiet)"
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
trap 'stop_servers; rm -rf "$WORK"' EXIT

# Loads the dump (on the first run, timed), copies the loaded repository for run $1 and starts `svnserve` over it: sets URL and STOCK.
start_stock() {
  local root="$WORK/stock-$1" port
  mkdir -p "$root"
  if [ ! -d "$WORK/loaded" ]; then
    "${A[@]}" create "$WORK/loaded"
    timed "stock load $1" "${A[@]}" load -q --bypass-prop-validation --file "$DUMP" "$WORK/loaded"
  fi
  cp -Rc "$WORK/loaded" "$root/project"
  printf '[general]\nanon-access = write\n' > "$root/project/conf/svnserve.conf"
  port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
  svnserve -d --foreground --listen-host 127.0.0.1 --listen-port "$port" -r "$root" \
    --log-file "$WORK/stock-$1.log" > /dev/null 2>&1 &
  STOCK=$!
  URL="svn://127.0.0.1:$port/project"
  AUTH=()
  for _ in $(seq 1 100); do s info "$URL" > /dev/null 2>&1 && break; sleep 0.1; done
  echo "stock run $1: svnserve on 127.0.0.1:$port"
}
# The daemon's resident set in MiB, or "-" where it is gone.
rss_of() { ps -o rss= -p "$1" 2> /dev/null | awk '{ printf "%.0f", $1 / 1024 }' || echo -; }

# The value of the first line of FILE matching the sed expression, or nothing.
first() { sed -n "$2" "$1" | head -1; }

# Starts a fresh slates anchor for run $1, makes the repository, imports the dump (timed) and mints a token: sets URL,
# AUTH, ANCHOR, DAEMON.
start_slates() {
  local instance="w-perf-svn-$$-$1" log="$WORK/anchor-$1.log" exports minted streak=0 port
  "$SLATES" --instance "$instance" anchor --vcs-svn 127.0.0.1:0 > /dev/null 2> "$log" &
  ANCHOR=$!
  for _ in $(seq 1 1200); do
    if grep -q "^slates daemon: instance $instance serving on" "$log" \
      && grep -q '^slates anchor: issuer surface: export ' "$log"; then break; fi
    kill -0 "$ANCHOR" 2> /dev/null || { cat "$log"; return 1; }
    sleep 0.1
  done
  DAEMON=$(first "$log" 's/^slates anchor: instance .* up; daemon pid \([0-9]*\);.*/\1/p')
  port=$(first "$log" 's|^slates daemon: the svn:// listener serves on 127\.0\.0\.1:\([0-9]*\).*|\1|p')
  exports=$(first "$log" 's/^slates anchor: issuer surface: export //p')
  echo "slates run $1: $(first "$log" 's/^\(slates daemon: instance .* serving on .*\)/\1/p'), svn:// on $port"
  for _ in $(seq 1 1200); do
    if "$SLATES" --instance "$instance" volume list > /dev/null 2>&1; then streak=$((streak + 1)); else streak=0; fi
    [ "$streak" -ge 10 ] && break
    sleep 0.1
  done
  "$SLATES" --instance "$instance" bootstrap root > /dev/null
  "$SLATES" --instance "$instance" repository create project --tool svn > /dev/null
  timed "slates import $1" "$SLATES" --instance "$instance" repository import project "$DUMP"
  echo "slates daemon rss after import: $(rss_of "$DAEMON") MiB" | tee -a "$RESULTS"
  # shellcheck disable=SC2086 # the surface's NAME=VALUE pairs are words
  minted=$(env $exports "$SLATES" --instance "$instance" credential token)
  URL="svn://127.0.0.1:$port/project"
  AUTH=(--username "$(echo "$minted" | sed -n 's/^id: //p')" --password "$(echo "$minted" | sed -n 's/^token: //p')")
}

# Checks working copy $1 of server $2 against SOURCE's tree at REF: each regular file's blob id and path.
check() {
  local copy=$1 server=$2
  if [ ! -d "$copy" ]; then
    echo "$server check skipped: no working copy" | tee -a "$RESULTS"
    return 0
  fi
  (cd "$copy" && find . -name .svn -prune -o -type f -print | sed 's|^\./||' | LC_ALL=C sort > "$WORK/$server.paths" \
    && git hash-object --no-filters --stdin-paths < "$WORK/$server.paths" | paste -d' ' - "$WORK/$server.paths" \
    | LC_ALL=C sort > "$WORK/$server.files")
  if cmp -s "$WORK/source.files" "$WORK/$server.files"; then
    echo "$server check tree identical ($(wc -l < "$WORK/source.files" | tr -d ' ') files)" | tee -a "$RESULTS"
  else
    echo "$server check tree DIFFER: $(comm -3 "$WORK/source.files" "$WORK/$server.files" | wc -l | tr -d ' ') lines" \
      | tee -a "$RESULTS"
    comm -3 "$WORK/source.files" "$WORK/$server.files" | head -5 | sed 's/^/    /'
  fi
}

# Runs the steps against the server started for run $2 ($1 its name).
steps() {
  local server=$1 run=$2 copy="$WORK/$1-wc-$2" other="$WORK/$1-other-$2" youngest named
  timed "$server checkout $run" "${S[@]}" ${AUTH[@]+"${AUTH[@]}"} checkout -q "$URL/trunk" "$copy"
  [ "$run" = 1 ] && check "$copy" "$server"
  timed "$server log $run" "${S[@]}" ${AUTH[@]+"${AUTH[@]}"} log -q "$URL"
  youngest=$(s info --show-item revision "$URL")
  named=$(grep -c '^r[0-9]' "$WORK/out.$server-log-$run")
  if [ "$named" = "$youngest" ]; then
    echo "$server check log named $named revisions" | tee -a "$RESULTS"
  else
    echo "$server check log MISSED: $named named of $youngest" | tee -a "$RESULTS"
  fi
  cp -R "$copy" "$other"
  echo "one more revision, $server run $run" > "$other/a-699-scale.txt"
  s add -q "$other/a-699-scale.txt"
  timed "$server commit $run" "${S[@]}" ${AUTH[@]+"${AUTH[@]}"} commit -q -m "one more revision" "$other"
  timed "$server update $run" "${S[@]}" ${AUTH[@]+"${AUTH[@]}"} update -q "$copy"
  if [ -f "$copy/a-699-scale.txt" ]; then
    echo "$server incremental check updated" | tee -a "$RESULTS"
  else
    echo "$server incremental check MISSED" | tee -a "$RESULTS"
  fi
  s update -q -r "$((youngest - UPDATE_SPAN))" "$copy"
  timed "$server update-$UPDATE_SPAN $run" "${S[@]}" ${AUTH[@]+"${AUTH[@]}"} update -q "$copy"
  rm -rf "$copy" "$other"
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
for step in load import checkout log commit update "update-$UPDATE_SPAN"; do
  for server in $SERVERS; do
  printf '%-18s %-6s' "$step" "$server"
  grep "^$server $step " "$RESULTS" | awk '{ printf " %s%s", $4, ($5 == "ok" ? "" : "(" $5 ")") }
    $5 == "ok" && (best == "" || $4 < best) { best = $4 } END { printf "  best %s", (best == "" ? "-" : best) }'
  echo
  done
done
echo "load at the end: $(uptime | sed 's/.*load averages*: //')"
