set -u
# g303 on jupiter (Windows x86_64, MSYS bash), 2026-11-19: A-650 part 1 green, then A-674 red and green, then A-673
# red and green, in ONE clone and ONE target dir (~/.cache/g303-target), one cargo at a time. Between steps the tree
# becomes the next commit by writing each path that differs (git show), so every changed file is newer than the last
# build and cargo rebuilds what it touches; each step prints which of our crates it compiled.
export PATH="/c/Users/corph/.cargo/bin:$PATH"
export CARGO_BUILD_JOBS=4 AWS_LC_SYS_PREBUILT_NASM=1
C=$HOME/.cache
W=$C/g303-a650w
L=$C/g303-logs
export CARGO_TARGET_DIR=$C/g303-target
P1=b4020cc4c50efe6ce2248456e0d787f296765c7f
B_RED=2570f7b52ee4e04423597014f8898a579fc97cf7
B_GREEN=7a6e9095a8e8fa29fde0218dd76fc47ca9d9c5e3
Q_RED=5b2f4cced47cc06d15b06d8e74fc0ac35ceccbd3
Q_GREEN=d30cc46af466653951b42e4d807a33f6ce704eea
rm -rf "$W" "$L"
mkdir -p "$L"
date -u
df -h "$C" 2>/dev/null | tail -1

retry() {
  for attempt in 1 2 3; do
    if "$@"; then
      return 0
    fi
    echo "attempt $attempt failed: $*"
    sleep 15
  done
  return 1
}
cloned() {
  rm -rf "$W"
  git -c http.version=HTTP/1.1 -c http.sslBackend=openssl -c core.autocrlf=false clone -q --depth 1 \
    --branch agents/g303-a650-p1 https://github.com/hyper-light/slates.git "$W"
}
retry cloned || { echo "CLONE FAILED"; exit 2; }
cd "$W" || exit 2
git config core.autocrlf false
fetched() {
  git -c http.version=HTTP/1.1 -c http.sslBackend=openssl fetch -q --depth 3 origin \
    agents/g303-a650-zlib-bound agents/g303-a650-query-414
}
retry fetched || { echo "FETCH FAILED"; exit 2; }
[ "$(git rev-parse HEAD)" = "$P1" ] || { echo "UNEXPECTED HEAD $(git rev-parse HEAD)"; exit 2; }
for commit in $B_RED $B_GREEN $Q_RED $Q_GREEN; do
  git cat-file -e "$commit^{commit}" || { echo "MISSING $commit"; exit 2; }
done
if (cd crates/hg/fixtures/serve-http && sha256sum --quiet -c SHA256SUMS); then
  echo "fixtures/serve-http: every file equals SHA256SUMS ($(wc -l < crates/hg/fixtures/serve-http/SHA256SUMS) files)"
else
  echo "FIXTURE BYTES DIFFER"
fi
rustc --version
cargo --version

step() {
  name=$1
  shift
  log=$L/$name.log
  start=$(date +%s)
  "$@" > "$log" 2>&1
  rc=$?
  echo "=== $name rc=$rc $(( $(date +%s) - start ))s: $*"
  command grep -E '^test result:|^error(\[E[0-9]+\])?:|could not compile' "$log" | head -20
  echo "    compiled: $(command grep -oE '^ +Compiling slates-(hg|http-edge|server) ' "$log" | tr -s ' ' | tr '\n' ',')"
}
failures() {
  sed -n '/^failures:$/,/^test result:/p' "$L/$1.log" | command grep -E '^    [A-Za-z_:0-9]+$' \
    | sed 's/^ */  FAILED /'
}
# The panic text of each failing test whose name matches $2, at most $3 lines of at most 3000 characters.
sections() {
  awk -v pattern="$2" '
    /^---- .* stdout ----$/ { show = ($0 ~ pattern) }
    /^failures:$/ { show = 0 }
    show { print }
  ' "$L/$1.log" | cut -c1-3000 | head -n "$3"
}
fmt() {
  cargo fmt -p "$1" -- --check > "$L/fmt-$2-$1.log" 2>&1
  echo "=== fmt $2 $1 rc=$?"
  head -20 "$L/fmt-$2-$1.log"
}
CURRENT=$P1
become() {
  target=$1
  git diff --name-status --no-renames "$CURRENT" "$target" > "$L/become-$2.txt"
  while IFS="$(printf '\t')" read -r status path; do
    case "$status" in
      D) rm -f -- "$path" ;;
      *) mkdir -p -- "$(dirname -- "$path")" && git show "$target:$path" > "$path" ;;
    esac
  done < "$L/become-$2.txt"
  if [ -z "$(git diff --name-only "$target")" ] && [ -z "$(git ls-files --others --exclude-standard)" ]; then
    echo "--- tree is $2 ($target), $(wc -l < "$L/become-$2.txt") paths written: $(cut -f2 "$L/become-$2.txt" | tr '\n' ' ')"
    CURRENT=$target
  else
    echo "TREE DIFFERS FROM $2"
    git diff --stat "$target"
    exit 3
  fi
}

step p1-hg cargo test --locked -p slates-hg --lib
failures p1-hg
step p1-edge cargo test --locked -p slates-http-edge --lib
failures p1-edge
step p1-server cargo test --locked -p slates-server --lib vcs_http -- --test-threads=4
failures p1-server
for p in slates-hg slates-http-edge slates-server; do
  fmt "$p" p1
done
echo "=== P1 DONE $(date -u)"

become "$Q_RED" a674-red
step a674-red cargo test --locked -p slates-server --lib vcs_http -- --test-threads=4
failures a674-red
sections a674-red 'requests_past' 20
become "$Q_GREEN" a674-green
step a674-green cargo test --locked -p slates-server --lib vcs_http -- --test-threads=4
failures a674-green
fmt slates-server a674
echo "=== A-674 DONE $(date -u)"

become "$B_RED" a673-red
step a673-red cargo test --locked -p slates-hg --lib compression
failures a673-red
sections a673-red 'compression' 30
become "$B_GREEN" a673-green
step a673-green cargo test --locked -p slates-hg --lib
failures a673-green
fmt slates-hg a673
echo "=== A-673 DONE $(date -u)"

cd "$C" && rm -rf "$W"
echo "clone deleted; target kept at $CARGO_TARGET_DIR ($(du -sh "$CARGO_TARGET_DIR" 2>/dev/null | cut -f1)); logs in $L"
df -h "$C" 2>/dev/null | tail -1
date -u
