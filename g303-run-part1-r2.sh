set -u
# g303, A-650 part 1, rerun after the 2026-11-19 relaunch, on jupiter (Windows x86_64, MSYS bash), one cargo at a time.
# Red: agents/g303-a650-red 14a9877af (25 guards removed from 6209c2deb). Green: agents/g303-a650-hg-http e6d8b6dd5
# (6209c2deb merged with main e3234e618, the tree that lands). ONE target dir (~/.cache/g303-target): the green tree is
# written in place after the red builds, so every file it changes is newer than the red build and cargo rebuilds it;
# each green step must show "Compiling" for its crate, or its result is the red build's.
export PATH="/c/Users/corph/.cargo/bin:$PATH"
export CARGO_BUILD_JOBS=4 AWS_LC_SYS_PREBUILT_NASM=1
C=$HOME/.cache
W=$C/g303-a650p1
L=$C/g303-logs
export CARGO_TARGET_DIR=$C/g303-target
RED=14a9877afe24707666dac6ce5a7a05d9ca9ba78f
GREEN=e6d8b6dd5d2428cdacb7ba303ee10f4050fe23be
rm -rf "$W" "$L" "$C/g303-target-red" "$C/g303-target-green"
mkdir -p "$L"
date -u
df -h "$C" 2>/dev/null | tail -1

cloned=no
for attempt in 1 2 3; do
  if git -c http.version=HTTP/1.1 -c http.sslBackend=openssl -c core.autocrlf=false clone -q --depth 1 \
    --branch agents/g303-a650-red https://github.com/hyper-light/slates.git "$W"; then
    cloned=yes
    break
  fi
  echo "clone attempt $attempt failed"
  rm -rf "$W"
  sleep 15
done
[ "$cloned" = yes ] || { echo "CLONE FAILED"; exit 2; }
cd "$W" || exit 2
git config core.autocrlf false
fetched=no
for attempt in 1 2 3; do
  if git -c http.version=HTTP/1.1 -c http.sslBackend=openssl fetch -q --depth 1 origin \
    agents/g303-a650-hg-http; then
    fetched=yes
    break
  fi
  echo "fetch attempt $attempt failed"
  sleep 15
done
[ "$fetched" = yes ] || { echo "FETCH FAILED"; exit 2; }
if [ "$(git rev-parse HEAD)" != "$RED" ] || [ "$(git rev-parse FETCH_HEAD)" != "$GREEN" ]; then
  echo "UNEXPECTED HEADS: red $(git rev-parse HEAD) green $(git rev-parse FETCH_HEAD)"
  exit 2
fi
fixtures() {
  if (cd crates/hg/fixtures/serve-http && sha256sum --quiet -c SHA256SUMS); then
    echo "fixtures/serve-http ($1): every file equals SHA256SUMS ($(wc -l < crates/hg/fixtures/serve-http/SHA256SUMS) files)"
  else
    echo "FIXTURE BYTES DIFFER ($1)"
  fi
}
fixtures red
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
  echo "    compiled: $(command grep -oE '^ +Compiling slates-(hg|http-edge|server|merge) ' "$log" | tr -s ' ' | tr '\n' ',')"
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

step red-hg cargo test --locked -p slates-hg --lib
failures red-hg
echo "--- red-hg: the bound tests' panics"
sections red-hg 'compression|charged' 60
step red-edge cargo test --locked -p slates-http-edge --lib
failures red-edge
step red-server cargo test --locked -p slates-server --lib vcs_http -- --test-threads=4
failures red-server

git diff --name-status --no-renames HEAD FETCH_HEAD > "$L/restore.txt"
echo "restoring $(wc -l < "$L/restore.txt") paths to green:"
cut -c1-120 "$L/restore.txt"
while IFS="$(printf '\t')" read -r status path; do
  case "$status" in
    D) rm -f -- "$path" ;;
    *) mkdir -p -- "$(dirname -- "$path")" && git show "FETCH_HEAD:$path" > "$path" ;;
  esac
done < "$L/restore.txt"
if [ -z "$(git diff --name-only FETCH_HEAD)" ] && [ -z "$(git ls-files --others --exclude-standard)" ]; then
  echo "tree restored: equals green $GREEN"
else
  echo "TREE DIFFERS FROM GREEN"
  git diff --stat FETCH_HEAD
  exit 3
fi
fixtures green

step green-hg cargo test --locked -p slates-hg --lib
failures green-hg
step green-edge cargo test --locked -p slates-http-edge --lib
failures green-edge
step green-server cargo test --locked -p slates-server --lib -- --test-threads=4
failures green-server
for p in slates-hg slates-http-edge slates-server; do
  cargo fmt -p "$p" -- --check > "$L/fmt-$p.log" 2>&1
  echo "=== fmt $p rc=$?"
  head -20 "$L/fmt-$p.log"
done
cd "$C" && rm -rf "$W"
echo "clone deleted; target kept at $CARGO_TARGET_DIR ($(du -sh "$CARGO_TARGET_DIR" 2>/dev/null | cut -f1)); logs in $L"
ls -la "$L"
date -u
