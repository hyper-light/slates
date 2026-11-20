#!/usr/bin/env bash
# Stock Subversion against a real slates anchor, one row per command a user runs (UI goal 93), as git-matrix.sh and
# hg-matrix.sh do for git and hg: starts `slates anchor` with its svn://, HTTP and SSH edges on loopback, makes a
# Subversion repository, imports a real history into it (`slates repository import`, a dump file), then runs info,
# ls, cat, checkout, log, blame, diff, export, update, status, commits (add, edit, delete, properties), server-side
# copies (a branch and a tag), switch, merge, lock and unlock over one route, and checkout and commit over the others,
# printing PASS or FAIL and the seconds for each.
#
# Usage: ci/interop/svn-matrix.sh SLATES_BINARY DUMP WORK_DIRECTORY
#   DUMP: a Subversion dump file whose history lives under trunk/ (crates/cli/bench/git_to_svn_dump.py makes one from
#   git/git: `git_to_svn_dump.py GIT_DIR master 2000 > DUMP`); absolute or relative.
#   ROUTE: the route the main rows run over, svn (default) or http.
#   WORK_DIRECTORY: scratch, made empty first.
# Exit: 0 when every row passes, 1 otherwise. Never writes outside WORK_DIRECTORY (the anchor's instance aside).
set -u
SLATES=$1 DUMP=$(cd "$(dirname "$2")" && pwd)/$(basename "$2") WORK=$3
INSTANCE="svn-matrix-$$"
STEP_LIMIT=${STEP_LIMIT:-3600}
ROUTE=${ROUTE:-svn}
rm -rf "$WORK" && mkdir -p "$WORK" || exit 1
cd "$WORK" || exit 1
export LC_ALL=C HOME="$WORK/home"
mkdir -p "$HOME" "$WORK/config"

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

"$SLATES" --instance "$INSTANCE" anchor --quick --shards 2 --vcs-svn 127.0.0.1:0 --vcs-http 127.0.0.1:0 \
  --vcs-ssh 127.0.0.1:0 >/dev/null 2>"$WORK/anchor.log" &
ANCHOR=$!
trap 'kill $ANCHOR 2>/dev/null; wait $ANCHOR 2>/dev/null' EXIT
for _ in $(seq 600); do
  grep -q "instance $INSTANCE serving on" anchor.log && grep -q 'issuer surface: export' anchor.log && break
  kill -0 $ANCHOR 2>/dev/null || { echo "the anchor ended"; tail -20 anchor.log; exit 1; }
  sleep 0.1
done
SVNPORT=$(sed -n 's/.*svn:\/\/ listener serves on 127.0.0.1:\([0-9]*\).*/\1/p' anchor.log | head -1)
HTTP=$(sed -n 's/.*HTTP edge serves cleartext on 127.0.0.1:\([0-9]*\).*/\1/p' anchor.log | head -1)
SSHLINE=$(grep 'SSH edge serves on 127.0.0.1:' anchor.log | head -1)
SSH=$(echo "$SSHLINE" | sed -n 's/.*127.0.0.1:\([0-9]*\) with.*/\1/p')
FINGERPRINT=${SSHLINE##* }
ISSUER=$(sed -n 's/.*issuer surface: export //p' anchor.log | head -1)
[ -n "$SVNPORT" ] && [ -n "$HTTP" ] && [ -n "$SSH" ] && [ -n "$ISSUER" ] ||
  { echo "no ports or issuer"; tail -20 anchor.log; exit 1; }
s() { "$SLATES" --instance "$INSTANCE" "$@"; }
for _ in $(seq 100); do s volume list >/dev/null 2>&1 && s volume list >/dev/null 2>&1 && break; sleep 0.1; done
s bootstrap root >/dev/null || { echo "bootstrap root failed"; exit 1; }
s repository create project --tool svn >/dev/null || { echo "repository create failed"; exit 1; }
MINTED=$(env $ISSUER "$SLATES" --instance "$INSTANCE" credential token)
TOKEN_ID=$(echo "$MINTED" | sed -n 's/^id: //p')
TOKEN=$(echo "$MINTED" | sed -n 's/^token: //p')
ssh-keygen -q -t ed25519 -N '' -f "$WORK/key" || exit 1
env $ISSUER "$SLATES" --instance "$INSTANCE" credential key "$(cat "$WORK/key.pub")" >/dev/null || exit 1
ssh-keyscan -p "$SSH" -t ed25519 127.0.0.1 >"$WORK/known_hosts" 2>/dev/null
[ "$(ssh-keygen -l -E sha256 -f "$WORK/known_hosts" | awk '{print $2}')" = "$FINGERPRINT" ] ||
  { echo "the SSH edge's key is not the one the daemon named"; exit 1; }
TUNNEL="config:tunnels:ssh=ssh -F /dev/null -p $SSH -i $WORK/key -o IdentitiesOnly=yes -o BatchMode=yes \
-o StrictHostKeyChecking=yes -o UserKnownHostsFile=$WORK/known_hosts -o GlobalKnownHostsFile=/dev/null --"
SVNURL="svn://127.0.0.1:$SVNPORT/project"
HTTPURL="http://127.0.0.1:$HTTP/project"
SSHURL="svn+ssh://slates@127.0.0.1/project"
export SVN_CONFIG_DIR="$WORK/config" TOKEN_ID TOKEN TUNNEL
# One function a route, exported so each row's `bash -c` has them; `$R` names the main rows' route.
sv() { svn --config-dir "$SVN_CONFIG_DIR" --non-interactive --no-auth-cache --username "$TOKEN_ID" --password "$TOKEN" "$@"; }
sh_() { svn --config-dir "$SVN_CONFIG_DIR" --non-interactive --no-auth-cache --username slates --password "$TOKEN" "$@"; }
ss() { svn --config-dir "$SVN_CONFIG_DIR" --non-interactive --no-auth-cache --config-option "$TUNNEL" "$@"; }
export -f sv sh_ ss
if [ "$ROUTE" = http ]; then R=sh_ URL=$HTTPURL; else R=sv URL=$SVNURL; fi
echo "svn $(svn --version --quiet); dump $DUMP; route $ROUTE $URL; svn:// $SVNPORT http $HTTP ssh $SSH"

row import "$SLATES" --instance "$INSTANCE" repository import project "$DUMP"
# Replay (A-703): stock svnrdump and svnsync over svn://, before any row changes the history. EXPECT_RDUMP and
# EXPECT_SYNC, when set, are the dumps stock svnserve's answers made of the same history (w-svn's fixtures,
# crates/svn/fixtures/replay), each compared byte for byte; the mirror takes MIRROR_UUID, and its svn:sync-from-url is
# set to SYNC_FROM_URL (the fixture's source URL, whose port no other run can share) before it is dumped.
MIRROR_UUID=${MIRROR_UUID:-00000000-0000-4000-8000-00000000a703}
export EXPECT_RDUMP="${EXPECT_RDUMP:-}" EXPECT_SYNC="${EXPECT_SYNC:-}" SYNC_FROM_URL="${SYNC_FROM_URL:-}"
export SVNURL MIRROR_UUID
row svnrdump-dump bash -c "svnrdump dump --config-dir \$SVN_CONFIG_DIR --non-interactive --no-auth-cache \
  --username \$TOKEN_ID --password \$TOKEN -q \$SVNURL > $WORK/rdump.dump \
  && { [ -z \"\$EXPECT_RDUMP\" ] || cmp $WORK/rdump.dump \"\$EXPECT_RDUMP\"; }"
row svnsync bash -c "svnadmin create $WORK/mirror && svnadmin setuuid $WORK/mirror \$MIRROR_UUID \
  && printf '#!/bin/sh\\nexit 0\\n' > $WORK/mirror/hooks/pre-revprop-change \
  && chmod +x $WORK/mirror/hooks/pre-revprop-change \
  && svnsync initialize --config-dir \$SVN_CONFIG_DIR --non-interactive --no-auth-cache \
     --source-username \$TOKEN_ID --source-password \$TOKEN file://$WORK/mirror \$SVNURL \
  && svnsync synchronize --config-dir \$SVN_CONFIG_DIR --non-interactive --no-auth-cache \
     --source-username \$TOKEN_ID --source-password \$TOKEN file://$WORK/mirror \
  && { [ -z \"\$SYNC_FROM_URL\" ] || svn propset --config-dir \$SVN_CONFIG_DIR -q --revprop -r0 \
     svn:sync-from-url \"\$SYNC_FROM_URL\" file://$WORK/mirror; } \
  && svnadmin dump --quiet $WORK/mirror > $WORK/sync.dump \
  && { [ -z \"\$EXPECT_SYNC\" ] || cmp $WORK/sync.dump \"\$EXPECT_SYNC\"; }"
# Revision properties (A-702 part 2): an allowing pre-revprop-change hook set with the owner's policy verb, then
# revision 1's svn:log changed and read back over svn:// and over svn+ssh.
printf '#!/bin/sh\nexit 0\n' > "$WORK/allow-revprop-change" && chmod +x "$WORK/allow-revprop-change"
row revprop-hook "$SLATES" --instance "$INSTANCE" repository policy project \
  --hook "pre-revprop-change=$WORK/allow-revprop-change"
row revprop-svn bash -c "sv propset -q --revprop -r1 svn:log matrix-svn \$0 \
  && [ \"\$(sv propget --revprop -r1 svn:log \$0)\" = matrix-svn ]" "$SVNURL"
row revprop-ssh bash -c "ss propset -q --revprop -r1 svn:log matrix-ssh \$0 \
  && [ \"\$(ss propget --revprop -r1 svn:log \$0)\" = matrix-ssh ]" "$SSHURL"
row info bash -c "$R info $URL/trunk"
row ls bash -c "$R ls $URL/trunk"
row cat bash -c "f=\$($R ls $URL/trunk | grep -v /\$ | head -1) && $R cat $URL/trunk/\$f"
row checkout bash -c "$R checkout -q $URL/trunk $WORK/a"
row log bash -c "$R log -q $URL"
row log-verbose bash -c "$R log -v -l 100 $URL/trunk"
row blame bash -c "cd $WORK/a && f=\$(ls -p | grep -v / | head -1) && [ -n \"\$f\" ] && $R blame \$f"
row diff bash -c "r=\$($R info --show-item last-changed-revision $URL/trunk) && $R diff -c \$r $URL/trunk"
row export bash -c "$R export -q $URL/trunk $WORK/exported"
row update-back bash -c "cd $WORK/a && $R update -q -r PREV && $R update -q"
row commit-add bash -c "cd $WORK/a && echo matrix >matrix.txt && $R add -q matrix.txt && $R commit -q -m add"
row commit-edit bash -c "cd $WORK/a && echo more >>matrix.txt && $R commit -q -m edit"
row status bash -c "cd $WORK/a && [ -z \"\$($R status -q)\" ]"
row propset bash -c "cd $WORK/a && $R propset -q matrix:prop value matrix.txt && $R commit -q -m prop \
  && [ \"\$($R propget matrix:prop $URL/trunk/matrix.txt)\" = value ]"
row mkdir-url bash -c "for d in branches tags; do $R ls $URL/\$d >/dev/null 2>&1 || $R mkdir -q -m dirs $URL/\$d || exit 1; done"
row copy-branch bash -c "$R copy -q -m branch $URL/trunk $URL/branches/matrix"
row copy-tag bash -c "$R copy -q -m tag $URL/trunk $URL/tags/matrix-v1"
row switch bash -c "cd $WORK/a && $R switch -q $URL/branches/matrix && echo b >>matrix.txt && $R commit -q -m on-branch"
row merge bash -c "cd $WORK/a && $R switch -q $URL/trunk && $R merge -q $URL/branches/matrix && $R commit -q -m merged"
row lock-unlock bash -c "cd $WORK/a && $R lock -q matrix.txt && $R unlock -q matrix.txt"
row delete bash -c "cd $WORK/a && $R delete -q matrix.txt && $R commit -q -m delete"
row revert bash -c "cd $WORK/a && echo x >>Makefile && $R revert -q Makefile && [ -z \"\$($R status -q)\" ]"
row http-checkout bash -c "sh_ checkout -q $HTTPURL/trunk $WORK/h"
row http-commit bash -c "cd $WORK/h && echo h >http.txt && sh_ add -q http.txt && sh_ commit -q -m http"
row ssh-checkout bash -c "ss checkout -q $SSHURL/trunk $WORK/s"
row ssh-commit bash -c "cd $WORK/s && echo s >ssh.txt && ss add -q ssh.txt && ss commit -q -m ssh"

echo "---- summary ($(printf '%s\n' "${ROWS[@]}" | grep -c '^PASS') of ${#ROWS[@]} passed)"
echo "daemon peak RSS over every row: $((RUN_PEAK_KIB / 1024)) MiB"
printf '%s\n' "${ROWS[@]}"
exit $FAILED
