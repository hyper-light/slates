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
row() {
  local name=$1; shift
  local log="$WORK/logs/$name.log" start end code
  mkdir -p "$WORK/logs"
  start=$(date +%s)
  perl -e 'alarm shift; exec @ARGV or exit 127' "$STEP_LIMIT" "$@" >"$log" 2>&1
  code=$?
  end=$(date +%s)
  if [ "$code" -eq 0 ]; then ROWS+=("PASS $name $((end - start))s"); else
    ROWS+=("FAIL $name $((end - start))s exit=$code: $(tail -3 "$log" | tr '\n' ' ' | cut -c1-300)")
    FAILED=1
  fi
  echo "${ROWS[${#ROWS[@]}-1]}"
}

"$SLATES" --instance "$INSTANCE" anchor --quick --shards 2 --vcs-http 127.0.0.1:0 --vcs-ssh 127.0.0.1:0 \
  >/dev/null 2>"$WORK/anchor.log" &
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
ssh = ssh -F /dev/null -i $WORK/key -o IdentitiesOnly=yes -o BatchMode=yes -o StrictHostKeyChecking=yes -o UserKnownHostsFile=$WORK/known_hosts -o GlobalKnownHostsFile=/dev/null
[extensions]
rebase =
EOF
export HGRCPATH="$WORK/hgrc"
h() { "${HG[@]}" "$@"; }
PUSH=(--new-branch -f)
[ -n "${PUSH_REV:-}" ] && PUSH+=(-r "$PUSH_REV")
echo "hg $(h --version | head -1); source $SOURCE $(h -R "$SOURCE" log -r 'all()' -T x | wc -c | tr -d ' ') changesets; http $HTTP ssh $SSH"

row push-all "${HG[@]}" -R "$SOURCE" push "${PUSH[@]}" "$URL"
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
row ssh-push bash -c "cd e && echo ssh >ssh.txt && ${HG[*]} commit -qAm ssh && ${HG[*]} push -r . $SSHURL"

echo "---- summary ($(printf '%s\n' "${ROWS[@]}" | grep -c '^PASS') of ${#ROWS[@]} passed)"
printf '%s\n' "${ROWS[@]}"
exit $FAILED
