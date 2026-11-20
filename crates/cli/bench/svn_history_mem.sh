#!/usr/bin/env bash
# A-728 part 3 (UI goal 93): the live heap a Subversion import leaves in the daemon, by allocation stack, and how much
# of it the merge engine's content history holds. w-perf's method of A-699 piece 3 (BENCHMARKS.md, "Where the svn
# server's time and memory go"): the anchor under MallocStackLogging=1, which the daemon inherits; `slates repository
# import` of a dump; then `malloc_history -allBySize` of the daemon. macOS only (footprint, malloc_history).
#
# Usage: svn_history_mem.sh SLATES DUMP LABEL
#   SLATES  a release `slates` binary (`cargo build --release --locked -p slates-cli`)
#   DUMP    a dump (`git_to_svn_dump.py src.git master 1000 > git-1000.dump`)
#   LABEL   names this run's files, written in the current directory: anchor-LABEL.log, mh-LABEL.txt
# Prints the import's answer, then the live bytes of the stacks through `slates_merge::history` (the history's chunks),
# through `slates_merge::dims` (file blocks and maps, where allocated there), and through `Green::commit` as a whole.
set -u
S=$1; D=$2; L=$3; I=svn-history-mem-$L
MallocStackLogging=1 "$S" --instance "$I" anchor --vcs-svn 127.0.0.1:0 > /dev/null 2> "anchor-$L.log" & A=$!
for _ in $(seq 1 600); do grep -q 'serving on' "anchor-$L.log" && break; sleep 0.1; done
DP=$(sed -n 's/^slates anchor: instance .* up; daemon pid \([0-9]*\);.*/\1/p' "anchor-$L.log" | head -1)
n=0; for _ in $(seq 1 600); do "$S" --instance "$I" volume list > /dev/null 2>&1 && n=$((n+1)) || n=0; [ $n -ge 10 ] && break; sleep 0.1; done
"$S" --instance "$I" bootstrap root > /dev/null
"$S" --instance "$I" repository create project --tool svn > /dev/null
"$S" --instance "$I" repository import project "$D" | tr '\n' ' '; echo
malloc_history "$DP" -allBySize > "mh-$L.txt" 2> /dev/null
kill -9 "$A" "$DP" 2>/dev/null
grep -m1 '^Physical footprint:' "mh-$L.txt"
python3 -I - "mh-$L.txt" <<'PY'
import re, sys
rows = {"history (slates_merge::history)": 0, "dims (slates_merge::dims)": 0, "Green::commit, all": 0}
for line in open(sys.argv[1], errors="replace"):
    m = re.match(r"(\d+) calls? for (\d+) bytes: (.*)", line)
    if not m:
        continue
    size, stack = int(m.group(2)), m.group(3)
    if "12slates_merge7history" in stack:
        rows["history (slates_merge::history)"] += size
    elif "12slates_merge4dims" in stack:
        rows["dims (slates_merge::dims)"] += size
    if "Green6commit" in stack:
        rows["Green::commit, all"] += size
for name, size in rows.items():
    print(f"{name}: {size / 1048576:.1f} MiB")
PY
