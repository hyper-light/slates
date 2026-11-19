#!/bin/bash
# Records stock Subversion's `svnlook` outputs and its hooks' inputs under `svnserve`, for A-623's golden vectors
# (§4.17.4 piece 11). Run on a machine with Subversion 1.15 (the Mac Studio's brew build in /opt/homebrew/bin):
#
#   crates/svn/fixtures/svnlook/make-svnlook-fixtures.sh crates/svn/fixtures/svnlook
#
# It builds one repository through `svnserve -d` with `svnmucc` (the history below), with nine hooks that record what
# they are given, takes and gives locks, changes revision properties, sets every revision's `svn:date` to a fixed
# value, and runs `svnlook` over the result. It writes into OUT:
# - look/<case>.args: the case's `svnlook` arguments after the subcommand and the repository, NUL-separated;
#   look/<case>.env: the environment assignments it ran under, NUL-separated; look/<case>.out: its standard output,
#   byte for byte; look/<case>.err and look/<case>.exit when it failed.
# - hooks/<n>-<hook>.args: a hook's arguments after its own path, NUL-separated; .stdin: its standard input; .env: its
#   environment from `env -0`; .cwd: its working directory; pre-commit's .look-<subcommand>.out: `svnlook -t` over its
#   transaction. hooks/000-control.env: what /bin/sh itself adds, the same hook run under `env -i`.
# - meta/: the repository's path as hooks receive it, the versions, the fixed dates.
# - SHA256SUMS over every file but this script.
# Scratch lives in a directory under $HOME/.cache, removed at exit with the server it started.
set -euo pipefail

OUT=$(cd "$1" && pwd)
BIN=${SVN_BIN:-/opt/homebrew/bin}
WORK=$(mktemp -d "$HOME/.cache/slates-svnlook-fixtures.XXXXXX")
cleanup() {
  if [ -f "$WORK/svnserve.pid" ]; then kill "$(cat "$WORK/svnserve.pid")" 2>/dev/null || true; fi
  rm -rf "$WORK"
}
trap cleanup EXIT

export LC_ALL=en_US.UTF-8 TZ=UTC
rm -rf "$OUT/look" "$OUT/hooks" "$OUT/meta"
mkdir -p "$OUT/look" "$OUT/hooks" "$OUT/meta" "$WORK/repos" "$WORK/files"

REPO="$WORK/repos/r"
"$BIN/svnadmin" create "$REPO"
"$BIN/svnadmin" setuuid "$REPO" 6f4d1a2e-a623-4000-8000-00000000a623

cat > "$REPO/conf/svnserve.conf" <<'EOF'
[general]
anon-access = none
auth-access = write
password-db = passwd
realm = slates-fixtures
hooks-env = hooks-env
EOF
cat > "$REPO/conf/passwd" <<'EOF'
[users]
jrandom = rayjandom
jconstant = rayjandom
EOF
cat > "$REPO/conf/hooks-env" <<'EOF'
[default]
LANG = en_US.UTF-8
SLATES_FIXTURE = default section
[pre-commit]
SLATES_FIXTURE = pre-commit section
EOF

HOOKS="$OUT/hooks"
echo 0 > "$WORK/counter"
# One hook script per point: it records its arguments, standard input, environment and working directory, with absolute
# paths only (a hook's environment has no PATH unless hooks-env gives one); pre-commit also reads its transaction.
make_hook() {
  local name=$1 extra=${2:-}
  cat > "$REPO/hooks/$name" <<EOF
#!/bin/sh
n=\$(/bin/cat "$WORK/counter"); n=\$((n + 1)); echo "\$n" > "$WORK/counter"
p=\$(printf '%03d-$name' "\$n")
for a in "\$@"; do printf '%s\\0' "\$a"; done > "$HOOKS/\$p.args"
/bin/cat > "$HOOKS/\$p.stdin"
/usr/bin/env -0 > "$HOOKS/\$p.env"
/bin/pwd > "$HOOKS/\$p.cwd"
$extra
exit 0
EOF
  chmod +x "$REPO/hooks/$name"
}
LOOK_TXN='for s in author changed date dirs-changed info log tree; do
  "'"$BIN"'/svnlook" "$s" -t "$2" "$1" > "'"$HOOKS"'/$p.look-$s.out" 2> "'"$HOOKS"'/$p.look-$s.err" || echo $? > "'"$HOOKS"'/$p.look-$s.exit"
done
"'"$BIN"'/svnlook" changed --copy-info -t "$2" "$1" > "'"$HOOKS"'/$p.look-changed-copy-info.out" 2>&1 || true
"'"$BIN"'/svnlook" propget --revprop -t "$2" "$1" svn:date > "'"$HOOKS"'/$p.look-raw-date.out" 2>&1 || true'
for hook in start-commit post-commit pre-revprop-change post-revprop-change pre-lock post-lock pre-unlock post-unlock; do
  make_hook "$hook"
done
make_hook pre-commit "$LOOK_TXN"
# The control: the same script under an empty environment shows what /bin/sh adds on its own.
sed -e "s|\$(printf '%03d-start-commit' \"\$n\")|000-control|" "$REPO/hooks/start-commit" > "$WORK/control"
chmod +x "$WORK/control"
(cd / && /usr/bin/env -i "$WORK/control" < /dev/null)
echo 0 > "$WORK/counter"

PORT=$(/usr/bin/python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
"$BIN/svnserve" -d --listen-host 127.0.0.1 --listen-port "$PORT" -r "$WORK/repos" --pid-file "$WORK/svnserve.pid"
URL="svn://127.0.0.1:$PORT/r"
AUTH=(--non-interactive --no-auth-cache --username jrandom --password rayjandom)
OTHER=(--non-interactive --no-auth-cache --username jconstant --password rayjandom)
for _ in $(seq 1 100); do
  if "$BIN/svn" info "${AUTH[@]}" "$URL" > /dev/null 2>&1; then break; fi
  sleep 0.1
done

F="$WORK/files"
printf 'Hello.\nThis is the README.\n' > "$F/README.1"
printf 'Hello.\nThis is the README, edited.\nA third line.\n' > "$F/README.2"
printf 'Hello from branch b1.\nThis is the README.\n' > "$F/README.3"
printf '#include <stdio.h>\n\nint main(void) {\n  puts("hi");\n  return 0;\n}\n' > "$F/main.c"
printf 'int util(void) { return 1; }\n' > "$F/util.c"
printf '\000\001\002\003binary\377\376' > "$F/data.bin"
printf '\000\001\002\003binary, changed\377' > "$F/data.bin.2"
: > "$F/empty"
printf 'no newline at the end' > "$F/noeol"
printf 'no newline at the end\nnow a second line\n' > "$F/noeol.2"
printf 'Caf\303\251 cr\303\250me\n' > "$F/cafe"
printf 'First line of the comment.\nSecond line.' > "$F/lock-comment"

MUCC=("$BIN/svnmucc" "${AUTH[@]}" -U "$URL")
# r1: the layout, two files, a directory property and a file property.
"${MUCC[@]}" -m "Initial layout." mkdir trunk mkdir trunk/src mkdir branches mkdir tags \
  put "$F/README.1" trunk/README put "$F/main.c" trunk/src/main.c \
  propset svn:ignore '*.o' trunk propset color red trunk/README
# r2: a text change, an added file, a binary file; a custom revision property.
"${MUCC[@]}" --with-revprop custom=value -m "Edit the README; add util.c and a binary." \
  put "$F/README.2" trunk/README put "$F/util.c" trunk/src/util.c \
  put "$F/data.bin" trunk/data.bin propset svn:mime-type application/octet-stream trunk/data.bin
# r3: a branch copied from r2, its README changed in the same revision.
"${MUCC[@]}" -m "Branch b1." cp 2 trunk branches/b1 put "$F/README.3" branches/b1/README
# r4: a deletion, a property changed and one deleted, an empty file, a file without a final newline.
"${MUCC[@]}" -m "Remove util.c; touch properties; add two files." rm trunk/src/util.c \
  propset color blue trunk/README propdel svn:ignore trunk put "$F/empty" trunk/empty put "$F/noeol" trunk/noeol
# r5: a replacement with history and a tag.
"${MUCC[@]}" -m "Replace the README from b1; tag v1." rm trunk/README cp 4 branches/b1/README trunk/README \
  cp 4 trunk tags/v1
# r6: non-ASCII names and a non-ASCII log, by another user.
"$BIN/svnmucc" "${OTHER[@]}" -U "$URL" -m "Non-ASCII: café crème." put "$F/cafe" "trunk/café.txt" \
  mkdir "trunk/with space"
# r7: a final newline added, a binary changed, a directory deleted.
"${MUCC[@]}" -m "Change noeol and the binary; drop the tag." put "$F/noeol.2" trunk/noeol \
  put "$F/data.bin.2" trunk/data.bin rm tags/v1

# Locks: take, read, steal, break; and take again and give back.
"$BIN/svn" lock "${AUTH[@]}" -F "$F/lock-comment" "$URL/trunk/src/main.c" > /dev/null
"$BIN/svnlook" lock "$REPO" trunk/src/main.c > "$OUT/look/lock-held.out"
"$BIN/svnlook" lock "$REPO" /trunk/src/main.c > "$OUT/look/lock-held-absolute.out"
"$BIN/svn" lock "${OTHER[@]}" --force -m "Stolen." "$URL/trunk/src/main.c" > /dev/null
"$BIN/svnlook" lock "$REPO" trunk/src/main.c > "$OUT/look/lock-stolen.out"
"$BIN/svn" unlock "${AUTH[@]}" --force "$URL/trunk/src/main.c" > /dev/null
"$BIN/svnlook" lock "$REPO" trunk/src/main.c > "$OUT/look/lock-none.out"
"$BIN/svn" lock "${AUTH[@]}" "$URL/trunk/noeol" > /dev/null
"$BIN/svnlook" lock "$REPO" trunk/noeol > "$OUT/look/lock-no-comment.out"
"$BIN/svn" unlock "${AUTH[@]}" "$URL/trunk/noeol" > /dev/null
printf 'trunk/src/main.c\0' > "$OUT/look/lock-held.args"
printf '/trunk/src/main.c\0' > "$OUT/look/lock-held-absolute.args"
printf 'trunk/src/main.c\0' > "$OUT/look/lock-stolen.args"
printf 'trunk/src/main.c\0' > "$OUT/look/lock-none.args"
printf 'trunk/noeol\0' > "$OUT/look/lock-no-comment.args"
"$BIN/svnadmin" lslocks "$REPO" > "$OUT/meta/lslocks-after.txt"

# Revision properties: one modified, one deleted, one added.
"$BIN/svn" propset "${AUTH[@]}" --revprop -r 2 svn:log "Edited log message." "$URL" > /dev/null
"$BIN/svn" propdel "${AUTH[@]}" --revprop -r 2 custom "$URL" > /dev/null
"$BIN/svn" propset "${AUTH[@]}" --revprop -r 1 slates:note "added later" "$URL" > /dev/null

# Fixed dates, so every output that names one is the same on every run.
YOUNGEST=$("$BIN/svnlook" youngest "$REPO")
for r in $(seq 0 "$YOUNGEST"); do
  printf '2026-11-19T05:%02d:%02d.%06dZ' "$r" "$((r * 7 % 60))" "$((r * 123457 % 1000000))" > "$WORK/date"
  "$BIN/svnadmin" setrevprop "$REPO" -r "$r" svn:date "$WORK/date"
  cp "$WORK/date" "$OUT/meta/date-r$r.txt"
done

# One svnlook case: NAME, then environment assignments (K=V) up to `--`, then the subcommand and its arguments.
look() {
  local name=$1; shift
  local env=()
  while [ "$1" != "--" ]; do env+=("$1"); shift; done
  shift
  local sub=$1; shift
  if [ "${#env[@]}" -gt 0 ]; then printf '%s\0' "${env[@]}" > "$OUT/look/$name.env"; fi
  printf '%s\0' "$sub" > "$OUT/look/$name.sub"
  if [ "$#" -gt 0 ]; then printf '%s\0' "$@" > "$OUT/look/$name.args"; else : > "$OUT/look/$name.args"; fi
  set +e
  /usr/bin/env ${env[@]+"${env[@]}"} "$BIN/svnlook" "$sub" "$REPO" "$@" > "$OUT/look/$name.out" 2> "$WORK/err"
  local code=$?
  set -e
  if [ "$code" -ne 0 ]; then echo "$code" > "$OUT/look/$name.exit"; cp "$WORK/err" "$OUT/look/$name.err"; fi
}

look youngest -- youngest
look youngest-no-newline -- youngest --no-newline
look uuid -- uuid
for r in $(seq 0 "$YOUNGEST"); do
  look "author-r$r" -- author -r "$r"
  look "date-r$r" -- date -r "$r"
  look "log-r$r" -- log -r "$r"
  look "info-r$r" -- info -r "$r"
  look "changed-r$r" -- changed -r "$r"
  look "changed-copy-info-r$r" -- changed --copy-info -r "$r"
  look "dirs-changed-r$r" -- dirs-changed -r "$r"
  look "diff-r$r" -- diff -r "$r"
done
look date-r1-kolkata TZ=Asia/Kolkata -- date -r 1
look date-r1-st-johns TZ=America/St_Johns -- date -r 1
look info-r6-c-locale LC_ALL=C -- info -r 6
look changed-r6-c-locale LC_ALL=C -- changed -r 6
look diff-r3-copy-from -- diff -r 3 --diff-copy-from
look diff-r5-no-added-deleted -- diff -r 5 --no-diff-deleted --no-diff-added
look diff-r4-ignore-properties -- diff -r 4 --ignore-properties
look diff-r4-properties-only -- diff -r 4 --properties-only
look diff-r7-ignore-space -- diff -r 7 -x -w
look tree -- tree
look tree-r3 -- tree -r 3
look tree-full-paths -- tree --full-paths
look tree-non-recursive -- tree -N
look tree-trunk -- tree trunk
look tree-trunk-non-recursive -- tree -N trunk
look tree-file -- tree trunk/README
look tree-show-ids -- tree --show-ids -r 2
look history-readme -- history trunk/README
look history-readme-r4 -- history -r 4 trunk/README
look history-b1-readme -- history branches/b1/README
look history-limit -- history -l 2 trunk/README
look history-show-ids -- history --show-ids trunk/README
look history-root -- history
look propget-mime -- propget svn:mime-type trunk/data.bin
look propget-color-r1 -- propget -r 1 color trunk/README
look propget-missing -- propget color trunk/noeol
look propget-revprop-log -- propget --revprop -r 2 svn:log
look propget-revprop-note -- propget --revprop -r 1 slates:note
look proplist-readme -- proplist trunk/README
look proplist-readme-verbose -- proplist -v trunk/README
look proplist-data -- proplist trunk/data.bin
look proplist-trunk-r1-verbose -- proplist -v -r 1 trunk
look proplist-revprop-r1 -- proplist --revprop -r 1
look proplist-revprop-r6-verbose -- proplist -v --revprop -r 6
for i in 1 2 3 4 5 6 7 8; do look "proplist-revprop-r1-run$i" -- proplist --revprop -r 1; done
look cat-readme -- cat trunk/README
look cat-noeol-r4 -- cat -r 4 trunk/noeol
look cat-cafe -- cat "trunk/café.txt"
look cat-binary -- cat trunk/data.bin
look cat-directory -- cat trunk
look cat-missing -- cat trunk/nothing
look filesize-binary -- filesize trunk/data.bin
look filesize-empty -- filesize trunk/empty

printf '%s' "$REPO" > "$OUT/meta/repository-path.txt"
printf '%s' "$YOUNGEST" > "$OUT/meta/youngest.txt"
"$BIN/svnlook" --version --quiet > "$OUT/meta/svnlook-version.txt"
"$BIN/svnserve" --version --quiet > "$OUT/meta/svnserve-version.txt"
uname -srm > "$OUT/meta/uname.txt"

cd "$OUT"
find look hooks meta -type f | LC_ALL=C sort | while read -r f; do shasum -a 256 "$f"; done > SHA256SUMS
