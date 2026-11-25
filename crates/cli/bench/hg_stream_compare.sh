#!/usr/bin/env bash
# Goal 93 perf, the hg stream clone's size: what slates' `hg clone --stream` sends against stock `hg serve`'s from the
# same repository. At 15b4f1061 slates' stream sent 8,454 files and 144 MB where stock's sent 4,284 files and 74.1 MB
# (BENCHMARKS, "the night's rerun"), so this names the class that differs at a scale a laptop runs in minutes.
#
# Both servers are filled as `hg_scale.sh` fills them: a fresh repository each, SOURCE pushed into it over HTTP. Then
# each is stream-cloned (`hg clone -U --stream --debug`), and the record compares:
# - the two clones' store files (`.hg/store`): count and bytes by class (changelog, manifest, filelog index `.i` and
#   data `.d`, the rest), the files only one side has, and the files whose sizes differ most;
# - the stream's own file list, from hg's debug output, and any file a stream sent twice;
# - `hg debugrevlog` of each clone's changelog, manifest and the source's largest filelog: revisions whole and as
#   deltas, and the chunks' kinds (`0x28` zstd, `0x78` zlib, `0x00` a raw delta, `u` uncompressed);
# - which protocol each stream used (v2, a bundle2 "stream2" part; v1, the `stream_out` command);
# - each revlog's header (its first four bytes: the version, and the inline and generaldelta flags), and any that
#   differs from stock's.
#
# Usage: hg_stream_compare.sh SLATES SOURCE WORK
#   SLATES  a `slates` binary (debug serves; the comparison is of what is sent, not how fast)
#   SOURCE  a Mercurial repository; a few hundred changesets do (`hg clone -U -r 400 hg-src small`)
#   WORK    a scratch directory, made empty here and removed at the end; KEEP_LOGS names a directory the record and the
#           debug output are copied into
# HG names the stock `hg` (default `hg` on the PATH).
set -uo pipefail

SLATES=$1
SOURCE=$(cd "$2" && pwd)
WORK=$3
HG_BIN=${HG:-hg}

rm -rf "$WORK"
mkdir -p "$WORK/home" || exit 1
WORK=$(cd "$WORK" && pwd) || exit 1
RECORD="$WORK/record.txt"

H=(env "HOME=$WORK/home" HGRCPATH= HGPLAIN=1 HGUSER="Test <test@example.com>" HGENCODING=utf-8 LC_ALL=C "$HG_BIN")
h() { "${H[@]}" "$@"; }
first() { sed -n "$2" "$1" | head -1; }
say() { echo "$*" | tee -a "$RECORD"; }

ANCHOR="" STOCK=""
stop_servers() {
  [ -n "$ANCHOR" ] && { kill -9 "$ANCHOR" 2> /dev/null; wait "$ANCHOR" 2> /dev/null; }
  [ -n "$STOCK" ] && { kill "$STOCK" 2> /dev/null; wait "$STOCK" 2> /dev/null; }
  ANCHOR="" STOCK=""
}
trap 'stop_servers; [ -n "${KEEP_LOGS:-}" ] && mkdir -p "$KEEP_LOGS" && cp "$WORK"/*.txt "$WORK"/*.log "$KEEP_LOGS"/ 2> /dev/null; rm -rf "$WORK"' EXIT

# Stock `hg serve` over a fresh repository, SOURCE pushed into it: sets URL and AUTH.
start_stock() {
  local root="$WORK/stock-served" port
  h init "$root"
  port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
  h -R "$root" serve --address 127.0.0.1 --port "$port" --config web.push_ssl=False --config 'web.allow-push=*' \
    --errorlog "$WORK/stock-serve.log" > /dev/null 2>&1 &
  STOCK=$!
  URL="http://127.0.0.1:$port/"
  AUTH=()
  for _ in $(seq 1 100); do h identify "$URL" > /dev/null 2>&1 && break; sleep 0.1; done
}

# A slates anchor with a fresh Mercurial repository and a token, SOURCE then pushed into it: sets URL and AUTH.
start_slates() {
  local instance="hg-stream-compare-$$" log="$WORK/anchor.log" exports token streak=0 port
  mkdir -p "$WORK/store"
  "$SLATES" --instance "$instance" anchor --vcs-http 127.0.0.1:0 --vcs-store "$WORK/store" > /dev/null 2> "$log" &
  ANCHOR=$!
  for _ in $(seq 1 1200); do
    if grep -q "^slates daemon: instance $instance serving on" "$log" \
      && grep -q '^slates anchor: issuer surface: export ' "$log"; then break; fi
    kill -0 "$ANCHOR" 2> /dev/null || { cat "$log"; return 1; }
    sleep 0.1
  done
  port=$(first "$log" 's/^slates daemon: the version-control HTTP edge serves cleartext on 127\.0\.0\.1:\([0-9]*\).*/\1/p')
  exports=$(first "$log" 's/^slates anchor: issuer surface: export //p')
  for _ in $(seq 1 1200); do
    if "$SLATES" --instance "$instance" volume list > /dev/null 2>&1; then streak=$((streak + 1)); else streak=0; fi
    [ "$streak" -ge 10 ] && break
    sleep 0.1
  done
  "$SLATES" --instance "$instance" bootstrap root > /dev/null
  "$SLATES" --instance "$instance" repository create project --tool hg > /dev/null || return 1
  # shellcheck disable=SC2086 # the surface's NAME=VALUE pairs are words
  token=$(env $exports "$SLATES" --instance "$instance" credential token | sed -n 's/^token: //p')
  URL="http://127.0.0.1:$port/project"
  AUTH=(--config "auth.slates.prefix=http://127.0.0.1:$port/" --config auth.slates.username=slates
    --config "auth.slates.password=$token")
}

# Pushes SOURCE to the server just started, then stream-clones it into WORK/$1-clone with hg's debug output kept as
# WORK/$1-stream.log; the clone's store is listed (path and bytes) as WORK/$1-store.txt.
fill_and_stream() {
  local server=$1
  h ${AUTH[@]+"${AUTH[@]}"} -R "$SOURCE" push -q "$URL" > "$WORK/$server-push.log" 2>&1 \
    || { say "$server: the push failed: $(tail -3 "$WORK/$server-push.log")"; return 1; }
  h ${AUTH[@]+"${AUTH[@]}"} clone -U --stream --debug "$URL" "$WORK/$server-clone" > "$WORK/$server-stream.log" 2>&1 \
    || { say "$server: the stream clone failed: $(tail -3 "$WORK/$server-stream.log")"; return 1; }
  (cd "$WORK/$server-clone/.hg/store" && find . -type f -exec stat -f '%z %N' {} + 2> /dev/null \
    || find . -type f -printf '%s %p\n') | sed 's| \./| |' | sort -k2 > "$WORK/$server-store.txt"
  say "$server: $(grep -E '^([0-9]+ files to transfer|stream-cloned |transferred )' "$WORK/$server-stream.log" | paste -sd ';' -)"
  say "$server protocol: $(if grep -q 'bundle2-input-part: "stream2"' "$WORK/$server-stream.log"; then echo 'stream v2 (bundle2 stream2)';
    elif grep -q 'sending stream_out command' "$WORK/$server-stream.log"; then echo 'stream v1 (stream_out)'; else echo unknown; fi)"
  # Each revlog's storage: how many revisions whole and how many deltas, and its chunks' kinds (hg's chunk type byte:
  # `x` zlib, `u` uncompressed, `(` zstd), for the changelog, the manifest and the source's largest filelog.
  local sample
  sample=$(cd "$SOURCE/.hg/store/data" && find . -name '*.i' -exec stat -f '%z %N' {} + 2> /dev/null | sort -rn \
    | head -1 | sed 's|^[0-9]* \./||; s|\.i$||' | sed 's|_\([a-z]\)|\1|g')
  for kind in -c -m "$sample"; do
    say "$server debugrevlog ${kind}: $(h -R "$WORK/$server-clone" debugrevlog "$kind" 2> /dev/null \
      | grep -E '^ *(revisions|full|deltas|revision size|chunks|0x[0-9a-f]+ )' | tr -s ' ' | paste -sd ';' -)"
  done
}

say "== hg stream clone, stock against slates, $(date -u '+%Y-%m-%d %H:%M:%S UTC')"
say "source: $SOURCE, $(h -R "$SOURCE" log -r 'all()' -T x | wc -c | tr -d ' ') changesets; $(h version -q | head -1); slates: $("$SLATES" version 2> /dev/null | head -1)"
start_stock && fill_and_stream stock
stop_servers
start_slates && fill_and_stream slates
stop_servers

python3 -I - "$WORK" << 'PY' | tee -a "$RECORD"
import collections, os, re, sys
work = sys.argv[1]
def store(server):
    files = {}
    try:
        for line in open(os.path.join(work, f"{server}-store.txt")):
            size, path = line.rstrip("\n").split(" ", 1)
            files[path] = int(size)
    except OSError:
        pass
    return files
def kind(path):
    if path.startswith("00changelog"): return "changelog"
    if path.startswith("00manifest"): return "manifest"
    if path.startswith("data/") and path.endswith(".i"): return "filelog .i"
    if path.startswith("data/") and path.endswith(".d"): return "filelog .d"
    return "other"
def streamed(server):
    names = []
    try:
        for line in open(os.path.join(work, f"{server}-stream.log")):
            # stream v2 (a bundle2 "stream2" part) logs "adding [s] NAME (SIZE)"; v1 (`stream_out`) "adding NAME (SIZE)".
            found = re.match(r"adding (?:\[[a-z]\] )?(\S+) \((.+)\)", line.strip())
            if found:
                names.append(found.group(1))
    except OSError:
        pass
    return names
stock, slates = store("stock"), store("slates")
print("== the clones' stores, by class: files and bytes, stock | slates")
classes = collections.OrderedDict((k, [0, 0, 0, 0]) for k in ("changelog", "manifest", "filelog .i", "filelog .d", "other"))
for files, at in ((stock, 0), (slates, 2)):
    for path, size in files.items():
        classes[kind(path)][at] += 1
        classes[kind(path)][at + 1] += size
for name, (sf, sb, lf, lb) in classes.items():
    print(f"{name:11} {sf:6} files {sb:12} bytes | {lf:6} files {lb:12} bytes")
print(f"{'all':11} {len(stock):6} files {sum(stock.values()):12} bytes | {len(slates):6} files {sum(slates.values()):12} bytes")
only_slates = sorted(set(slates) - set(stock))
only_stock = sorted(set(stock) - set(slates))
print(f"== files only slates' clone has: {len(only_slates)}; only stock's: {len(only_stock)}")
for path in only_slates[:15]: print(f"  slates only: {path} ({slates[path]} bytes)")
for path in only_stock[:15]: print(f"  stock only: {path} ({stock[path]} bytes)")
common = [(slates[p] - stock[p], p) for p in set(stock) & set(slates)]
print("== the files in both whose sizes differ most (slates minus stock, bytes)")
for diff, path in sorted(common, reverse=True)[:15]:
    if diff: print(f"  {diff:+12} {path} (stock {stock[path]}, slates {slates[path]})")
def headers(server):
    found = {}
    for path in store(server):
        if path.endswith(".i"):
            try:
                with open(os.path.join(work, f"{server}-clone", ".hg", "store", path), "rb") as index:
                    found[path] = index.read(4).hex()
            except OSError:
                pass
    return found
stock_headers, slates_headers = headers("stock"), headers("slates")
differ = sorted(p for p in set(stock_headers) & set(slates_headers) if stock_headers[p] != slates_headers[p])
for server, found in (("stock", stock_headers), ("slates", slates_headers)):
    print(f"== {server}'s revlog headers: " + ", ".join(f"{h} x{c}" for h, c in sorted(collections.Counter(found.values()).items())))
print(f"== revlog headers that differ from stock's: {len(differ)}")
for path in differ[:15]: print(f"  {path}: stock {stock_headers[path]}, slates {slates_headers[path]}")
for server in ("stock", "slates"):
    names = streamed(server)
    twice = [n for n, c in collections.Counter(names).items() if c > 1]
    print(f"== {server}'s stream named {len(names)} files in hg's debug output; sent more than once: {len(twice)}" + (f" (e.g. {twice[:5]})" if twice else ""))
PY
say "record: $RECORD"
