set -u
# g303, A-650 part 1: red then green on jupiter (Windows x86_64, MSYS bash), one cargo at a time, own target per arm.
export PATH="/c/Users/corph/.cargo/bin:$PATH"
export CARGO_BUILD_JOBS=4 AWS_LC_SYS_PREBUILT_NASM=1
C=$HOME/.cache
W=$C/g303-a650p1
L=$C/g303-logs
RED=14a9877afe24707666dac6ce5a7a05d9ca9ba78f
GREEN=6209c2deb94ee016f07946f87c21b84781b65b07
rm -rf "$W" "$L" "$C/g303-target-red" "$C/g303-target-green"
mkdir -p "$L"
cloned=no
for attempt in 1 2 3; do
  if git -c http.version=HTTP/1.1 -c http.sslBackend=openssl -c core.autocrlf=false clone -q --depth 2 \
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
if [ "$(git rev-parse HEAD)" != "$RED" ] || [ "$(git rev-parse HEAD~1)" != "$GREEN" ]; then
  echo "UNEXPECTED HEADS: $(git rev-parse HEAD HEAD~1 | tr '\n' ' ')"
  exit 2
fi
if (cd crates/hg/fixtures/serve-http && sha256sum --quiet -c SHA256SUMS); then
  echo "fixtures/serve-http: every file equals SHA256SUMS ($(wc -l < crates/hg/fixtures/serve-http/SHA256SUMS) files)"
else
  echo "FIXTURE BYTES DIFFER"
fi
rustc --version
cargo --version
uptime 2>/dev/null || true

step() {
  name=$1
  shift
  log=$L/$name.log
  start=$(date +%s)
  "$@" > "$log" 2>&1
  rc=$?
  echo "=== $name rc=$rc $(( $(date +%s) - start ))s: $*"
  command grep -E '^test result:|^error(\[E[0-9]+\])?:|could not compile' "$log" | head -20
  echo "    compiled ours: $(command grep -cE '^ +Compiling slates-(hg|http-edge|server) ' "$log")"
}

failures() {
  sed -n '/^failures:$/,/^test result:/p' "$L/$1.log" | command grep -E '^    [A-Za-z_:0-9]+$' \
    | sed 's/^ */  FAILED /'
}

export CARGO_TARGET_DIR=$C/g303-target-red
step red-hg cargo test --locked -p slates-hg --lib
failures red-hg
step red-edge cargo test --locked -p slates-http-edge --lib
failures red-edge
step red-server cargo test --locked -p slates-server --lib vcs_http -- --test-threads=4
failures red-server
rm -rf "$C/g303-target-red"

for f in $(git diff --name-only HEAD~1 HEAD); do
  git show "HEAD~1:$f" > "$f"
done
if [ -z "$(git diff --name-only HEAD~1)" ]; then
  echo "tree restored: equals green $GREEN"
else
  echo "TREE DIFFERS FROM GREEN"
  git diff --stat HEAD~1
  exit 3
fi

export CARGO_TARGET_DIR=$C/g303-target-green
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
rm -rf "$C/g303-target-green" "$W"
echo "logs kept in $L (delete after reading)"
ls -la "$L"
