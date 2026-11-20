set -u
export CARGO_BUILD_JOBS=4
R="$HOME/.cache/g261-a636-green"
export CARGO_TARGET_DIR="$R/target"
rm -rf "$R"; mkdir -p "$R"
git clone --quiet --depth 1 --branch agents/g261-a636-tmp-2 https://github.com/hyper-light/slates.git "$R/src" || { echo CLONE_FAILED; rm -rf "$R"; exit 10; }
cd "$R/src" && echo "head $(git rev-parse HEAD) tree $(git rev-parse 'HEAD^{tree}')"
cargo fmt -p slates-db
if git diff --quiet; then echo "FMT clean"; else echo "FMT diff"; git diff --stat; echo B64BEGIN; git diff | gzip -9 | base64; echo B64END; git diff > "$R/fmt.patch"; git apply -R "$R/fmt.patch"; fi
cargo test -p slates-db --lib ledger -- --nocapture > "$R/out.txt" 2>&1; echo "exit $?"
grep -E "^test .* \.\.\. |^test result|panicked at|^census" "$R/out.txt" | head -60
grep -E "^error" -A10 "$R/out.txt" | head -120
grep -E "^warning" -A6 "$R/out.txt" | head -40
cargo test -p slates-db --lib > "$R/all.txt" 2>&1; echo "all exit $?"; grep -E "^test result|FAILED|panicked at" "$R/all.txt" | head -20
cd "$HOME" && rm -rf "$R" && echo "removed $R"
