set -u
export CARGO_BUILD_JOBS=4
R="$HOME/.cache/g261-a636-red"
export CARGO_TARGET_DIR="$R/target"
rm -rf "$R"; mkdir -p "$R"
git clone --quiet --depth 1 --branch agents/g261-a636-tmp-1 https://github.com/hyper-light/slates.git "$R/src" || { echo CLONE_FAILED; rm -rf "$R"; exit 10; }
cd "$R/src" && echo "head $(git rev-parse HEAD)"
cargo test -p slates-db --lib ledger > "$R/out.txt" 2>&1; echo "exit $?"
grep -E "^test .* \.\.\. |^test result|panicked at" "$R/out.txt" | head -60
grep -E "^(error|warning: unused)" -A8 "$R/out.txt" | head -100
cd "$HOME" && rm -rf "$R" && echo "removed $R"
