#!/usr/bin/env bash
# A-699 (UI goal 93): stock Subversion serving a large real repository, every step timed: the baseline slates'
# Subversion site is measured against. `docs/wip/BENCHMARKS.md` records each run of it; the command there is the
# contract.
#
# The dataset: a dump of git/git's first-parent history under trunk/ (`git_to_svn_dump.py`), so Subversion is measured
# over the same commits as git.
#
# The server, a fresh one for every run: `svnserve -d` over svn:// on loopback over the repository the run loaded,
# anonymous write allowed (`anon-access = write`), as a stock server that takes commits without accounts is configured.
#
# The steps, each timed on the wall clock and bounded (`alarm`), a failure recorded and the run going on:
# 1. load: `svnadmin load` of the whole dump into a fresh repository (Subversion's own import of a history);
# 2. checkout: `svn checkout` of trunk at its youngest revision; on the first run the working copy is checked: its
#    files equal REFERENCE's (`diff -r`, `.svn` left out);
# 3. log: `svn log -q` of the whole history, which must name every revision;
# 4. incremental: one new file committed from a copy of the working copy (`commit`), then the working copy updated to
#    it (`update`), which must then hold it;
# 5. update-1000: the working copy taken back 1,000 revisions (untimed), then updated to the youngest.
#
# svn runs with its own empty configuration directory, never prompts, and speaks C. Everything is written under WORK,
# which is made empty and removed at the end; DUMP and REFERENCE are only read.
#
# Usage: svn_scale.sh DUMP REFERENCE WORK [RUNS] [BOUND]
#   DUMP       the dataset's dump (`git_to_svn_dump.py src.git master > dump`)
#   REFERENCE  the dataset's youngest tree (`git archive master | tar -x -C REFERENCE`, its submodule directories
#              removed), to check a checkout against
#   WORK       a scratch directory, made empty here and removed at the end
#   RUNS       the runs (default 3)
#   BOUND      the seconds any one step may take before it is killed and recorded as such (default 3600)
set -uo pipefail

DUMP=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
REFERENCE=$(cd "$2" && pwd)
WORK=$3
RUNS=${4:-3}
BOUND=${5:-3600}
UPDATE_SPAN=1000

rm -rf "$WORK"
mkdir -p "$WORK/home" "$WORK/config" || exit 1
WORK=$(cd "$WORK" && pwd) || exit 1
RESULTS="$WORK/results.txt"
: > "$RESULTS"

# Stock svn, outside every configuration file and user, never prompting, in C's words: a command (`env` then `svn`),
# so a bounded step can exec it.
S=(env "HOME=$WORK/home" LC_ALL=C svn --non-interactive --config-dir "$WORK/config")
s() { "${S[@]}" "$@"; }
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

REVISIONS=$(grep -ac '^Revision-number: ' "$DUMP")
echo "dataset: $DUMP, $(du -h "$DUMP" | cut -f1), $REVISIONS revisions; reference tree $(find "$REFERENCE" -type f | wc -l | tr -d ' ') files"
echo "svn: $(svn --version --quiet); svnserve: $(svnserve --version --quiet)"
echo "machine: $(sysctl -n machdep.cpu.brand_string 2> /dev/null), $(sysctl -n hw.ncpu 2> /dev/null) cores; load: $(uptime | sed 's/.*load averages*: //')"

STOCK=""
stop_servers() {
  if [ -n "$STOCK" ]; then kill "$STOCK" 2> /dev/null || true; wait "$STOCK" 2> /dev/null || true; fi
  STOCK=""
}
trap 'stop_servers; rm -rf "$WORK"' EXIT

# Loads the dump into a fresh repository for run $1 (timed) and starts `svnserve` over it: sets URL and STOCK.
start_stock() {
  local root="$WORK/stock-$1" port
  mkdir -p "$root"
  "${A[@]}" create "$root/project"
  timed "stock load $1" "${A[@]}" load -q --bypass-prop-validation --file "$DUMP" "$root/project"
  printf '[general]\nanon-access = write\n' > "$root/project/conf/svnserve.conf"
  port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
  svnserve -d --foreground --listen-host 127.0.0.1 --listen-port "$port" -r "$root" \
    --log-file "$WORK/stock-$1.log" > /dev/null 2>&1 &
  STOCK=$!
  URL="svn://127.0.0.1:$port/project"
  for _ in $(seq 1 100); do s info "$URL" > /dev/null 2>&1 && break; sleep 0.1; done
  echo "stock run $1: svnserve on 127.0.0.1:$port"
}

# Checks working copy $1 of server $2 against REFERENCE.
check() {
  local copy=$1 server=$2
  if [ ! -d "$copy" ]; then
    echo "$server check skipped: no working copy" | tee -a "$RESULTS"
    return 0
  fi
  if diff -rq -x .svn "$REFERENCE" "$copy" > "$WORK/$server.diff" 2>&1; then
    echo "$server check tree identical ($(find "$REFERENCE" -type f | wc -l | tr -d ' ') files)" | tee -a "$RESULTS"
  else
    echo "$server check tree DIFFER: $(wc -l < "$WORK/$server.diff" | tr -d ' ') lines" | tee -a "$RESULTS"
    head -5 "$WORK/$server.diff" | sed 's/^/    /'
  fi
}

# Runs the steps against the server started for run $2 ($1 its name).
steps() {
  local server=$1 run=$2 copy="$WORK/$1-wc-$2" other="$WORK/$1-other-$2" youngest named
  timed "$server checkout $run" "${S[@]}" checkout -q "$URL/trunk" "$copy"
  [ "$run" = 1 ] && check "$copy" "$server"
  timed "$server log $run" "${S[@]}" log -q "$URL"
  youngest=$(s info --show-item revision "$URL")
  named=$(grep -c '^r[0-9]' "$WORK/out.$server-log-$run")
  if [ "$named" = "$youngest" ]; then
    echo "$server log check named $named revisions" | tee -a "$RESULTS"
  else
    echo "$server log check MISSED: $named named of $youngest" | tee -a "$RESULTS"
  fi
  cp -R "$copy" "$other"
  echo "one more revision, $server run $run" > "$other/a-699-scale.txt"
  s add -q "$other/a-699-scale.txt"
  timed "$server commit $run" "${S[@]}" commit -q -m "one more revision" "$other"
  timed "$server update $run" "${S[@]}" update -q "$copy"
  if [ -f "$copy/a-699-scale.txt" ]; then
    echo "$server incremental check updated" | tee -a "$RESULTS"
  else
    echo "$server incremental check MISSED" | tee -a "$RESULTS"
  fi
  s update -q -r "$((youngest - UPDATE_SPAN))" "$copy"
  timed "$server update-$UPDATE_SPAN $run" "${S[@]}" update -q "$copy"
  rm -rf "$copy" "$other"
}

for run in $(seq 1 "$RUNS"); do
  start_stock "$run"
  steps stock "$run"
  stop_servers
  rm -rf "$WORK/stock-$run"
done

echo "== results (seconds; every run shown)"
for step in load checkout log commit update "update-$UPDATE_SPAN"; do
  printf '%-18s %-6s' "$step" stock
  grep "^stock $step " "$RESULTS" | awk '{ printf " %s%s", $3, ($4 == "ok" ? "" : "(" $4 ")") }
    $4 == "ok" && (best == "" || $3 < best) { best = $3 } END { printf "  best %s", (best == "" ? "-" : best) }'
  echo
done
echo "load at the end: $(uptime | sed 's/.*load averages*: //')"
