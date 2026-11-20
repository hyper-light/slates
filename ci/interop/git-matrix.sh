#!/usr/bin/env bash
# Stock git against a real slates anchor, one row per command a user runs (UI goal 93, "interop with stock git, hg and
# svn clients proven"). Starts `slates anchor` with its HTTP and SSH edges on loopback, makes a git repository, pushes
# a real repository's branches and tags into it, then runs clone, log, blame, diff, branch, tag, merge, rebase, fetch,
# pull, shallow and partial clones and an SSH clone against it, printing PASS or FAIL and the seconds for each.
#
# Usage: ci/interop/git-matrix.sh SLATES_BINARY SOURCE_REPOSITORY WORK_DIRECTORY
#   SOURCE_REPOSITORY: a local git repository (a mirror of git/git, say); PUSH_REFS (default every branch and tag)
#   names what the first pushes send, e.g. PUSH_REFS='v1.0.0^{commit}:refs/heads/master' for a cut of its history.
#   WORK_DIRECTORY: scratch, made empty first.
# Exit: 0 when every row passes, 1 otherwise. Never writes outside WORK_DIRECTORY (the anchor's instance aside).
set -u
SLATES=$1 SOURCE=$2 WORK=$3
INSTANCE="git-matrix-$$"
STEP_LIMIT=${STEP_LIMIT:-3600}
# PUSH_REFS: the refspecs the first pushes send (default every branch and tag).
read -r -a PUSH_REFS <<<"${PUSH_REFS:-refs/heads/*:refs/heads/* refs/tags/*:refs/tags/*}"
rm -rf "$WORK" && mkdir -p "$WORK" || exit 1
cd "$WORK" || exit 1
export GIT_TERMINAL_PROMPT=0 GIT_CONFIG_NOSYSTEM=1 HOME="$WORK/home"
mkdir -p "$HOME"
git config --global user.name Matrix && git config --global user.email matrix@example.com
git config --global init.defaultBranch main

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
s repository create project --tool git --default-branch "$(git -C "$SOURCE" symbolic-ref --short HEAD)" >/dev/null || { echo "repository create failed"; exit 1; }
TOKEN=$(env $ISSUER "$SLATES" --instance "$INSTANCE" credential token | sed -n 's/^token: //p')
ssh-keygen -q -t ed25519 -N '' -f "$WORK/key" || exit 1
env $ISSUER "$SLATES" --instance "$INSTANCE" credential key "$(cat "$WORK/key.pub")" >/dev/null || exit 1
ssh-keyscan -p "$SSH" -t ed25519 127.0.0.1 >"$WORK/known_hosts" 2>/dev/null
[ "$(ssh-keygen -l -E sha256 -f "$WORK/known_hosts" | awk '{print $2}')" = "$FINGERPRINT" ] ||
  { echo "the SSH edge's key is not the one the daemon named"; exit 1; }
URL="http://git:$TOKEN@127.0.0.1:$HTTP/project"
SSHURL="ssh://git@127.0.0.1:$SSH/project"
export GIT_SSH_COMMAND="ssh -F /dev/null -i $WORK/key -o IdentitiesOnly=yes -o BatchMode=yes \
  -o StrictHostKeyChecking=yes -o UserKnownHostsFile=$WORK/known_hosts -o GlobalKnownHostsFile=/dev/null"
echo "git $(git --version); source $SOURCE $(git -C "$SOURCE" rev-list --all --count) commits; http $HTTP ssh $SSH"

row push-all git -C "$SOURCE" push "$URL" "${PUSH_REFS[@]}"
row push-all-ssh git -C "$SOURCE" push "$SSHURL" "${PUSH_REFS[@]}"
row ls-remote git ls-remote "$URL"
row clone git clone -q "$URL" a
row fsck git -C a fsck --no-progress
row same-heads bash -c "[ \"\$(git -C a rev-parse HEAD)\" = \"\$(git -C a ls-remote origin HEAD | cut -f1)\" ]"
row log git -C a log -n 1000 --stat --format=%H
row log-graph git -C a log --graph --oneline -n 2000 --all
row blame git -C a blame -- "$(git -C a ls-files | grep -m1 -E '\.(c|rs|py|go)$')"
row diff git -C a diff HEAD~50 HEAD --stat
row show git -C a show --format=fuller HEAD
row checkout-tag bash -c "cd a && git checkout -q \$(git describe --tags --abbrev=0) && git checkout -q -"
row branch-push bash -c "cd a && git switch -q -c matrix-topic && echo topic >matrix.txt && git add matrix.txt \
  && git commit -qm topic && git push -q origin matrix-topic"
row tag-push bash -c "cd a && git tag -a matrix-v1 -m matrix && git push -q origin matrix-v1"
row clone-b git clone -q "$URL" b
row merge-push bash -c "cd b && git fetch -q && echo main >matrix-main.txt && git add . && git commit -qm main \
  && git merge -q --no-edit origin/matrix-topic && git push -q origin HEAD"
row pull bash -c "cd a && git switch -q - && git pull -q --no-rebase origin \$(git -C ../b branch --show-current)"
row rebase-push bash -c "cd a && git switch -q matrix-topic && echo more >>matrix.txt && git commit -qam more \
  && git rebase -q origin/\$(git -C ../b branch --show-current) && git push -q --force-with-lease origin matrix-topic"
row fetch-prune bash -c "cd b && git push -q origin --delete matrix-topic && cd ../a && git fetch -q --prune \
  && ! git rev-parse -q --verify refs/remotes/origin/matrix-topic"
row tag-delete git -C b push -q origin --delete matrix-v1
row shallow-clone git clone -q --depth 1 "$URL" c
row deepen git -C c fetch -q --deepen 100
row partial-clone git clone -q --filter=blob:none "$URL" d
row partial-blame git -C d blame -- "$(git -C d ls-files | grep -m1 -E '\.(c|rs|py|go)$')"
row ssh-clone git clone -q "$SSHURL" e
row ssh-push bash -c "cd e && echo ssh >ssh.txt && git add ssh.txt && git commit -qm ssh && git push -q origin HEAD"
row ssh-fetch git -C a fetch -q "$SSHURL"

echo "---- summary ($(printf '%s\n' "${ROWS[@]}" | grep -c '^PASS') of ${#ROWS[@]} passed)"
printf '%s\n' "${ROWS[@]}"
exit $FAILED
