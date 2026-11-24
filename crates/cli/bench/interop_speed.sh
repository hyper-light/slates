#!/usr/bin/env bash
# Goal 93's "an order of magnitude faster than competing servers, measured and recorded" (§4.17.1's method): each stock
# client against slates and against its stock servers, on one machine, the servers interleaved run by run (stock
# first), best of RUNS with every run shown, the host's load and each server's peak memory and CPU recorded beside the
# times. One command runs the three tools' harnesses, each the contract of its own BENCHMARKS entries:
# - git (`git_scale.sh`): stock `git daemon` (git://, receive-pack on, `uploadpack.allowFilter`), stock
#   `git http-backend` under Apache httpd (smart HTTP, `stock-http`), and slates' HTTP edge;
# - Mercurial (`hg_scale.sh`): stock `hg serve` (HTTP, `web.push_ssl=False`, `web.allow-push=*`) and slates' HTTP edge;
# - Subversion (`svn_scale.sh`): stock `svnserve -d` (svn://) and slates' svn:// listener. `mod_dav_svn` under Apache is
#   measured where the host has it (`HTTPD_MODULES/mod_dav_svn.so`) by no harness yet: its presence is recorded, and
#   where absent the record says so.
#
# Recorded first: the date (UTC), the machine (model, cores, memory, OS), slates' version and commit, every client's and
# server's version, which competitors the host has. While the harnesses run, a sampler records every SAMPLE seconds the
# load average and, for each server kind (slates' anchor and daemon, git daemon, httpd with its CGIs, hg serve,
# svnserve), the summed resident memory and CPU time of its processes; the summary gives each kind's peak resident
# memory and the most CPU time seen. Each harness's own results table follows, as it prints it.
#
# Usage: interop_speed.sh SLATES WORK GIT_SOURCE HG HG_SOURCE SVN_DUMP SVN_SOURCE SVN_REF [RUNS] [BOUND]
#   SLATES      the release `slates` binary (`cargo build --release --locked -p slates-cli --bin slates`)
#   WORK        a scratch directory, made empty here; each harness's logs are kept under WORK/logs
#   GIT_SOURCE  a bare clone of the git dataset (`git clone --bare https://github.com/git/git.git`)
#   HG          the stock `hg` command (e.g. `uvx --from mercurial==7.2 hg` wrapped in a script)
#   HG_SOURCE   the Mercurial dataset (`hg clone -U https://www.mercurial-scm.org/repo/hg`)
#   SVN_DUMP    the Subversion dataset (`git_to_svn_dump.py GIT_SOURCE master 2000 > DUMP`), SVN_SOURCE and SVN_REF the
#               git repository and tip it was made from, for svn_scale's tree check
#   RUNS        runs of each server (default 3); BOUND, the seconds any one step may take (default 3600)
# Environment: SAMPLE (seconds between samples, default 5); HTTPD and HTTPD_MODULES, as git_scale.sh reads them.
set -uo pipefail

SLATES=$1 WORK=$2 GIT_SOURCE=$3 HG=$4 HG_SOURCE=$5 SVN_DUMP=$6 SVN_SOURCE=$7 SVN_REF=$8
RUNS=${9:-3} BOUND=${10:-3600} SAMPLE=${SAMPLE:-5}
HERE=$(cd "$(dirname "$0")" && pwd)
rm -rf "$WORK" && mkdir -p "$WORK/logs" || exit 1
WORK=$(cd "$WORK" && pwd)
OUT="$WORK/interop-speed.txt"
HTTPD=${HTTPD:-/usr/sbin/httpd} HTTPD_MODULES=${HTTPD_MODULES:-/usr/libexec/apache2}
export HTTPD HTTPD_MODULES

say() { echo "$*" | tee -a "$OUT"; }
load() { uptime | sed 's/.*load averages*: //'; }

say "== interop speed, $(date -u '+%Y-%m-%d %H:%M:%S UTC')"
say "machine: $(sysctl -n hw.model 2> /dev/null || uname -m), $(sysctl -n machdep.cpu.brand_string 2> /dev/null || grep -m1 'model name' /proc/cpuinfo | cut -d: -f2), $(sysctl -n hw.ncpu 2> /dev/null || nproc) cores, $(( $(sysctl -n hw.memsize 2> /dev/null || echo 0) / 1073741824 )) GiB; $(sw_vers -productVersion 2> /dev/null || uname -sr)"
say "slates: $("$SLATES" version 2> /dev/null | head -1) at $(git -C "$HERE" rev-parse --short HEAD 2> /dev/null)"
say "clients and servers: $(git --version); $("$HG" version -q 2> /dev/null | head -1); svn $(svn --version --quiet); svnserve $(svnserve --version --quiet)"
if [ -x "$HTTPD" ]; then
  say "httpd: $("$HTTPD" -v | sed -n 's/^Server version: //p') ($HTTPD); git-http-backend: $(git --exec-path)/git-http-backend"
else
  say "httpd: absent ($HTTPD): git-http-backend not measured"
fi
if [ -f "$HTTPD_MODULES/mod_dav_svn.so" ]; then
  say "mod_dav_svn: present ($HTTPD_MODULES/mod_dav_svn.so), measured by no harness yet"
else
  say "mod_dav_svn: absent from $HTTPD_MODULES: not measured"
fi
say "runs: $RUNS of each server, interleaved, stock first; bound $BOUND s a step; load at the start: $(load)"

# The sampler: every SAMPLE seconds, the load and each server kind's summed resident memory (KiB) and CPU time
# (seconds), one line a sample, until the harnesses end.
kind_of() {
  case $1 in
    *"slates --instance"*" anchor"*) echo slates-anchor ;;
    *"slates --instance"*" daemon"*) echo slates-daemon ;;
    *"git daemon"* | *"git-daemon"*) echo git-daemon ;;
    *httpd*"stock-http-"* | *git-http-backend*) echo httpd-git ;;
    *"hg serve"*) echo hg-serve ;;
    *svnserve*) echo svnserve ;;
    *) echo - ;;
  esac
}
# Seconds in ps's CPU time, `[[dd-]hh:]mm:ss[.ss]`.
seconds_of() {
  echo "$1" | awk '{ t = $1; d = 0; if (index(t, "-")) { split(t, dp, "-"); d = dp[1]; t = dp[2] }
    n = split(t, p, ":"); s = 0; for (i = 1; i <= n; i++) s = s * 60 + p[i]; printf "%.2f\n", s + d * 86400 }'
}
sampler() {
  while :; do
    ps -axo rss=,time=,command= 2> /dev/null | while read -r rss time command; do
      kind=$(kind_of "$command")
      [ "$kind" = - ] && continue
      echo "$kind $rss $(seconds_of "$time")"
    done | awk -v load="$(load | tr ' ' ',')" -v at="$(date +%s)" \
      '{ rss[$1] += $2; cpu[$1] += $3 } END { printf "%s load=%s", at, load; for (k in rss) printf " %s=%d,%.0f", k, rss[k], cpu[k]; print "" }'
    sleep "$SAMPLE"
  done
}
sampler >> "$WORK/samples.txt" &
SAMPLER=$!
trap 'kill "$SAMPLER" 2> /dev/null' EXIT

run() {
  local tool=$1
  shift
  say "== $tool, load before: $(load)"
  KEEP_LOGS="$WORK/logs/$tool" "$@" 2>&1 | tee -a "$OUT" | grep -E '^== results|^(push|clone|incremental|shallow|partial|stream|checkout|log|commit|update|import)' || true
  say "== $tool, load after: $(load)"
}
run git env SERVERS="stock stock-http slates" bash "$HERE/git_scale.sh" "$SLATES" "$GIT_SOURCE" "$WORK/git" "$RUNS" "$BOUND"
run hg env SERVERS="stock slates" SLATES="$SLATES" bash "$HERE/hg_scale.sh" "$HG" "$HG_SOURCE" "$WORK/hg" "$RUNS" "$BOUND"
run svn env SERVERS="stock slates" SLATES="$SLATES" bash "$HERE/svn_scale.sh" "$SVN_DUMP" "$SVN_SOURCE" "$SVN_REF" "$WORK/svn" "$RUNS" "$BOUND"

kill "$SAMPLER" 2> /dev/null
say "== each server kind's peak resident memory (MiB) and most CPU time seen (s), over every sample"
awk '{ for (i = 3; i <= NF; i++) { split($i, kv, "="); split(kv[2], v, ","); if (v[1] > rss[kv[1]]) rss[kv[1]] = v[1]; if (v[2] > cpu[kv[1]]) cpu[kv[1]] = v[2] } }
  END { for (k in rss) printf "%-14s peak %6.0f MiB  cpu %6.0f s\n", k, rss[k] / 1024, cpu[k] }' "$WORK/samples.txt" | sort | tee -a "$OUT"
say "== load, sampled every ${SAMPLE} s: $(awk '{ split($2, l, "[=,]"); if (l[2] > max) max = l[2] } END { print "highest 1-minute " max }' "$WORK/samples.txt"); at the end: $(load)"
say "record: $OUT; samples: $WORK/samples.txt; logs: $WORK/logs"
