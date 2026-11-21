#!/usr/bin/env bash
# Scratch (never committed): builds base and the diag candidate, makes git v2.0.0's stock repository, runs the A/B.
set -eu
W=$HOME/.cache/w-bugs-remote; mkdir -p "$W"
echo "host $(hostname) cpus $(sysctl -n hw.ncpu) $(uptime)"
SRC_GIT=${SRC_GIT:?a checkout holding 1de9e5e6f, read only}
for d in base-src diag-src; do rm -rf "$W/$d" && mkdir "$W/$d" && git -C "$SRC_GIT" archive 1de9e5e6f | tar -x -C "$W/$d"; done
(cd "$W/diag-src" && patch -s -p1 < "$W/payload/wb-cand.patch") || { echo "patch failed"; exit 1; }
t0=$(date +%s); (cd "$W/diag-src" && CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=$W/target cargo build -q --release -p slates-cli) && cp "$W/target/release/slates" "$W/slates-diag"; echo "diag built $(( $(date +%s)-t0 )) s"
t0=$(date +%s); (cd "$W/base-src" && CARGO_INCREMENTAL=0 CARGO_TARGET_DIR=$W/target cargo build -q --release -p slates-cli) && cp "$W/target/release/slates" "$W/slates-base"; echo "base built $(( $(date +%s)-t0 )) s"
[ -d "$W/gitgit.git" ] || git clone -q --bare https://github.com/git/git.git "$W/gitgit.git"
rm -rf "$W/stock-v2.git"; git init -q --bare "$W/stock-v2.git"
git -C "$W/gitgit.git" push -q "$W/stock-v2.git" 'v2.0.0^{commit}:refs/heads/master'
git -C "$W/stock-v2.git" symbolic-ref HEAD refs/heads/master; git -C "$W/stock-v2.git" repack -adq
set +e
TRIALS=${TRIALS:-5} bash "$W/payload/bench-ab.sh" "$W/stock-v2.git" "$W/gitgit.git" "$W/ab" base="$W/slates-base" diag="$W/slates-diag"
echo "---- DIAG"
for f in base diag; do echo "== $f"; grep -h "DIAG" "$W/ab/$f-anchor.log" | sed -E 's/[0-9]+ ms/N ms/' | sort | uniq -c | sort -rn | head -20; grep -h "DIAG" "$W/ab/$f-anchor.log" | sort -t' ' -k4 -n | tail -15; done
echo "---- spins"
grep -h "is spinning\|killing it" "$W/ab/"*-anchor.log | cut -c1-220
