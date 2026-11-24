#!/usr/bin/env bash
# Stock Mercurial against a real slates anchor, one row per command a user runs (UI goal 93), as git-matrix.sh does
# for git: starts `slates anchor` with its HTTP and SSH edges on loopback, makes a Mercurial repository, pushes a real
# repository into it over HTTP (the token as Basic's password), then runs clone, verify, log, annotate, diff,
# bookmarks, tags, named branches, merge, pull, rebase, incoming/outgoing, a stream clone and SSH clone/pull/push
# against it, printing PASS or FAIL and the seconds for each.
#
# Usage: ci/interop/hg-matrix.sh SLATES_BINARY SOURCE_REPOSITORY WORK_DIRECTORY
#   SOURCE_REPOSITORY: a local Mercurial repository (a clone of hg's own, say); PUSH_REV (default every head) names
#   what the first push sends, e.g. PUSH_REV=6.0 for a cut of its history.
#   HG: the command that runs stock hg 7.2 (default `hg`).
#   SSH_VERBOSE: when set, ssh runs with -vv, its trace in each SSH row's log.
#   WORK_DIRECTORY: scratch, made empty first.
# Exit: 0 when every row passes, 1 otherwise. Never writes outside WORK_DIRECTORY (the anchor's instance aside).
set -u
SLATES=$1 SOURCE=$2 WORK=$3
INSTANCE="hg-matrix-$$"
STEP_LIMIT=${STEP_LIMIT:-3600}
read -r -a HG <<<"${HG:-hg}"
rm -rf "$WORK" && mkdir -p "$WORK" || exit 1
cd "$WORK" || exit 1
export HGPLAIN=1 HGRCPATH="" HGUSER="Matrix <matrix@example.com>" HOME="$WORK/home"
mkdir -p "$HOME"

FAILED=0
ROWS=()
# row NAME COMMAND...: runs COMMAND within STEP_LIMIT seconds, records PASS/FAIL and its seconds, keeps its output.
# daemon_rss: the resident KiB of this instance's daemon (0 while none runs), as ps reports it.
daemon_rss() {
  ps -axo rss=,command= 2>/dev/null | grep -F -- "--instance $INSTANCE daemon" | grep -v grep | awk '{s+=$1} END {print s+0}'
}
# RSS_PERIOD: seconds between a row's samples of the daemon's resident memory (default 1). Each row's peak is printed
# with it (rss=), and the whole run's after the summary, so a matrix records the memory each phase takes; where ps
# cannot list the daemon (Git Bash on Windows) every figure reads 0.
RSS_PERIOD=${RSS_PERIOD:-1}
RUN_PEAK_KIB=0
row() {
  local name=$1; shift
  local log="$WORK/logs/$name.log" peak="$WORK/logs/$name.rss" start end code sampler kib
  mkdir -p "$WORK/logs"
  start=$(date +%s)
  daemon_rss >"$peak"
  ( top=0; while :; do now=$(daemon_rss); [ "$now" -gt "$top" ] && { top=$now; echo "$top" >"$peak"; }
    sleep "$RSS_PERIOD"; done ) &
  sampler=$!
  perl -e 'alarm shift; exec @ARGV or exit 127' "$STEP_LIMIT" "$@" >"$log" 2>&1
  code=$?
  end=$(date +%s)
  kill "$sampler" 2>/dev/null; wait "$sampler" 2>/dev/null
  kib=$(cat "$peak" 2>/dev/null || echo 0)
  [ "$kib" -gt "$RUN_PEAK_KIB" ] && RUN_PEAK_KIB=$kib
  if [ "$code" -eq 0 ]; then ROWS+=("PASS $name $((end - start))s rss=$((kib / 1024))MiB"); else
    ROWS+=("FAIL $name $((end - start))s rss=$((kib / 1024))MiB exit=$code: $(tail -3 "$log" | tr '\n' ' ' | cut -c1-300)")
    FAILED=1
  fi
  echo "${ROWS[${#ROWS[@]}-1]}"
}

# The store directory (A-767's standing grant): a repository's history is kept on the node's changeset store under it,
# and the daemon refuses `repository create` without one (main 5b929b8d2: "start the daemon with --vcs-store DIR").
mkdir -p "$WORK/store"
"$SLATES" --instance "$INSTANCE" anchor --quick --shards 2 --vcs-http 127.0.0.1:0 --vcs-ssh 127.0.0.1:0 \
  --vcs-store "$WORK/store" >/dev/null 2>"$WORK/anchor.log" &
ANCHOR=$!
trap 'kill $ANCHOR 2>/dev/null; wait $ANCHOR 2>/dev/null' EXIT
for _ in $(seq 600); do
  grep -q "instance $INSTANCE serving on" anchor.log && grep -q 'issuer surface: export' anchor.log && break
  kill -0 $ANCHOR 2>/dev/null || { echo "the anchor ended"; tail -20 anchor.log; exit 1; }
  sleep 0.1
done
HTTP=$(sed -n 's/.*HTTP edge serves cleartext on 127.0.0.1:\([0-9]*\).*/\1/p' anchor.log | head -1)
SSHLINE=$(grep 'SSH edge serves on 127.0.0.1:' anchor.log | head -1)
SSH=$(echo "$SSHLINE" | sed -n 's/.*127.0.0.1:\([0-9]*\) with.*/\1/p')
FINGERPRINT=${SSHLINE##* }
ISSUER=$(sed -n 's/.*issuer surface: export //p' anchor.log | head -1)
[ -n "$HTTP" ] && [ -n "$SSH" ] && [ -n "$ISSUER" ] || { echo "no ports or issuer"; tail -20 anchor.log; exit 1; }
s() { "$SLATES" --instance "$INSTANCE" "$@"; }
for _ in $(seq 100); do s volume list >/dev/null 2>&1 && s volume list >/dev/null 2>&1 && break; sleep 0.1; done
s bootstrap root >/dev/null || { echo "bootstrap root failed"; exit 1; }
s repository create project --tool hg >/dev/null || { echo "repository create failed"; exit 1; }
TOKEN=$(env $ISSUER "$SLATES" --instance "$INSTANCE" credential token | sed -n 's/^token: //p')
ssh-keygen -q -t ed25519 -N '' -f "$WORK/key" || exit 1
env $ISSUER "$SLATES" --instance "$INSTANCE" credential key "$(cat "$WORK/key.pub")" >/dev/null || exit 1
ssh-keyscan -p "$SSH" -t ed25519 127.0.0.1 >"$WORK/known_hosts" 2>/dev/null
[ "$(ssh-keygen -l -E sha256 -f "$WORK/known_hosts" | awk '{print $2}')" = "$FINGERPRINT" ] ||
  { echo "the SSH edge's key is not the one the daemon named"; exit 1; }
URL="http://127.0.0.1:$HTTP/project"
SSHURL="ssh://slates@127.0.0.1:$SSH/project"
cat >"$WORK/hgrc" <<EOF
[auth]
slates.prefix = http://127.0.0.1:$HTTP/
slates.username = slates
slates.password = $TOKEN
[ui]
ssh = ssh ${SSH_VERBOSE:+-vv} -F /dev/null -i $WORK/key -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=yes -o UserKnownHostsFile=$WORK/known_hosts -o GlobalKnownHostsFile=/dev/null
[extensions]
rebase =
EOF
export HGRCPATH="$WORK/hgrc"
h() { "${HG[@]}" "$@"; }
PUSH=(--new-branch -f)
[ -n "${PUSH_REV:-}" ] && PUSH+=(-r "$PUSH_REV")
echo "hg $(h --version | head -1); source $SOURCE $(h -R "$SOURCE" log -r 'all()' -T x | wc -c | tr -d ' ') changesets; http $HTTP ssh $SSH"

# DIAG (diag branch only): PUSH_CHUNK pushes the history a PUSH_CHUNK of revisions at a time (`-r REV -f`, ancestors
# with it) then the rest, as hg_scale.sh does, for a server whose one push of it is refused; the repository is the same.
# A chunk refused in words that ask for it ("push again") is pushed again, up to six times, a second more each time.
if [ -n "${PUSH_CHUNK:-}" ]; then
  PUSH_ERR=$WORK/push-chunk.err
  export PUSH_CHUNK SOURCE URL PUSH_ERR
  export HG_WORDS="${HG[*]}"
  row push-all bash -c 'tip=$($HG_WORDS -R "$SOURCE" log -r tip -T "{rev}"); tries=0; for rev in $(seq "$PUSH_CHUNK" "$PUSH_CHUNK" "$tip") tip; do for try in 1 2 3 4 5 6; do $HG_WORDS -R "$SOURCE" push -q --new-branch -f -r "$rev" "$URL" 2>"$PUSH_ERR"; c=$?; [ $c -le 1 ] && break; grep -q "push again" "$PUSH_ERR" || break; tries=$((tries+1)); sleep $try; done; [ $c -le 1 ] || { cat "$PUSH_ERR"; echo "chunk at $rev exit $c after $try tries"; exit $c; }; done; echo "chunks pushed; retried on push again: $tries"'
else
row push-all "${HG[@]}" -R "$SOURCE" push "${PUSH[@]}" "$URL"
# DIAG (diag branch only; w-hg's ask, main 01a20182 2026-11-22 11:16): what holds the daemon's memory right after
# push-all, before any later row can spin and restart it.
s status --json --memory >"$WORK/status-after-push.json" 2>"$WORK/status-after-push.err" ||
  echo "DIAG status after push failed: $(head -3 "$WORK/status-after-push.err")"
# DIAG (diag branch only; main 01a20182 arena-12): the daemon's resident memory by mapping right after push-all, its
# smaps_rollup and the mappings holding the most, summed by name ([anon] for a nameless one), to name what no gauge counts.
DPID=$(ps -axo pid=,command= 2>/dev/null | grep -F -- "--instance $INSTANCE daemon" | grep -v grep | awk '{print $1}' | head -1)
if [ -n "$DPID" ] && [ -r "/proc/$DPID/smaps" ]; then
  echo "DIAG smaps_rollup after push-all (pid $DPID): $(tr -s ' ' < "/proc/$DPID/smaps_rollup" | tr '\n' ';' | cut -c1-900)"
  awk '/^[0-9a-f]+-[0-9a-f]+ / {name = (NF >= 6 ? $6 : "[anon]")} /^Rss:/ {rss[name] += $2} END {for (n in rss) print rss[n], n}' \
    "/proc/$DPID/smaps" | sort -rn | head -15 | while read -r kib name; do echo "DIAG resident after push-all: $kib KiB $name"; done
fi
# DIAG (diag branch only; main 01a20182 arena-13): the same readings after the daemon sat idle past several sweep
# passes (a pass runs a sweep's interval, the liveness budget of about 1 s, after what owes it), so a freed content page
# not yet purged is told apart from one kept short or deferred (w-shared's three mechanisms, the content arena gauges).
sleep 10
s status --json --memory >"$WORK/status-after-idle.json" 2>"$WORK/status-after-idle.err" ||
  echo "DIAG status after idle failed: $(head -3 "$WORK/status-after-idle.err")"
if [ -n "$DPID" ] && [ -r "/proc/$DPID/smaps" ]; then
  echo "DIAG smaps_rollup after idle (pid $DPID): $(tr -s ' ' < "/proc/$DPID/smaps_rollup" | tr '\n' ';' | cut -c1-900)"
  awk '/^[0-9a-f]+-[0-9a-f]+ / {name = (NF >= 6 ? $6 : "[anon]")} /^Rss:/ {rss[name] += $2} END {for (n in rss) print rss[n], n}' \
    "/proc/$DPID/smaps" | sort -rn | head -15 | while read -r kib name; do echo "DIAG resident after idle: $kib KiB $name"; done
fi
# DIAG (diag branch only): stop here when asked, so the read after push-all doesn't wait on rows that still spin.
if [ "${DIAG_STOP_AFTER_PUSH:-0}" = 1 ]; then
  printf '%s\n' "---- stopped after push-all (DIAG_STOP_AFTER_PUSH)" "${ROWS[@]}"
  echo "daemon peak RSS over every row: $((RUN_PEAK_KIB / 1024)) MiB"
  s status --json --memory >"$WORK/status.json" 2>/dev/null || true
  exit 0
fi
fi
row identify "${HG[@]}" identify "$URL"
row clone "${HG[@]}" clone -q "$URL" a
row verify "${HG[@]}" -R a verify -q
row same-tip bash -c "[ \"\$(${HG[*]} -R a identify -r tip -i)\" = \"\$(${HG[*]} identify -r tip $URL)\" ]"
row log "${HG[@]}" -R a log -l 1000 --stat -T '{node}\n'
row log-graph "${HG[@]}" -R a log -G -l 2000 -T '{rev}\n'
row annotate bash -c "cd a && ${HG[*]} annotate \$(${HG[*]} files 'glob:**.py' | head -1)"
row diff "${HG[@]}" -R a diff -r 'tip~50' -r tip --stat
row bookmark-push bash -c "cd a && ${HG[*]} bookmark matrix-bm && { ${HG[*]} push -B matrix-bm; [ \$? -le 1 ]; } \
  && ${HG[*]} debugpushkey $URL bookmarks | grep -q '^matrix-bm'"
row tag-push bash -c "cd a && ${HG[*]} tag matrix-v1 && ${HG[*]} push"
row branch-push bash -c "cd a && ${HG[*]} branch matrix-branch && echo b >matrix.txt && ${HG[*]} commit -qAm branch \
  && ${HG[*]} push --new-branch -r ."
row clone-b "${HG[@]}" clone -q "$URL" b
row merge-push bash -c "cd b && ${HG[*]} update -q default && echo m >matrix-main.txt && ${HG[*]} commit -qAm main \
  && ${HG[*]} merge -q matrix-branch && ${HG[*]} commit -qm merge && ${HG[*]} push -r ."
row pull-update bash -c "cd a && ${HG[*]} pull -q -u"
row rebase-push bash -c "cd b && ${HG[*]} update -q matrix-branch && echo r >>matrix.txt && ${HG[*]} commit -qm r \
  && ${HG[*]} update -q default && echo d >>matrix-main.txt && ${HG[*]} commit -qm d \
  && ${HG[*]} rebase -q -s 'max(branch(matrix-branch))' -d default && ${HG[*]} push -r ."
row incoming bash -c "cd a && ${HG[*]} incoming -q >/dev/null; [ \$? -le 1 ]"
row outgoing bash -c "cd b && ${HG[*]} outgoing -q >/dev/null; [ \$? -le 1 ]"
row phase-public bash -c "cd a && ${HG[*]} pull -q && [ \"\$(${HG[*]} phase -r tip | cut -d' ' -f2)\" = public ]"
row clone-rev "${HG[@]}" clone -q -r 'default' "$URL" c
row stream-clone "${HG[@]}" clone -q --stream "$URL" d
row ssh-clone "${HG[@]}" clone -q "$SSHURL" e
row ssh-pull "${HG[@]}" -R a pull -q "$SSHURL"
# Its own working copy, cloned from `a` on disk (no server), so it pushes over SSH whether or not ssh-clone's `e` was
# made: on main 01a20182's run 45037561866 every leg's ssh-clone was red and ssh-push failed at 0 s on `cd: e`.
row ssh-push bash -c "${HG[*]} clone -q a f && cd f && echo ssh >ssh.txt && ${HG[*]} commit -qAm ssh \
  && ${HG[*]} push -r . $SSHURL"

# DIAG (diag branch only): the daemon's status, its shards' counters with it, while the anchor still runs.
s status --json --memory >"$WORK/status.json" 2>"$WORK/status.err" || echo "DIAG status failed: $(head -3 "$WORK/status.err")"
echo "---- summary ($(printf '%s\n' "${ROWS[@]}" | grep -c '^PASS') of ${#ROWS[@]} passed)"
echo "daemon peak RSS over every row: $((RUN_PEAK_KIB / 1024)) MiB"
printf '%s\n' "${ROWS[@]}"
exit $FAILED
