"""Resolve one amendment-log conflict at a file's end: main's side first, then ours, one blank line between.

Usage: python3 -I resolve_amendment_end.py <path>
Refuses (exit 2) unless the file holds exactly one conflict.
"""
import sys

path = sys.argv[1]
with open(path, encoding="utf-8") as handle:
    lines = handle.read().split("\n")

starts = [index for index, line in enumerate(lines) if line.startswith("<<<<<<< ")]
middles = [index for index, line in enumerate(lines) if line == "======="]
ends = [index for index, line in enumerate(lines) if line.startswith(">>>>>>> ")]
if not (len(starts) == len(middles) == len(ends) == 1):
    print(f"refused: {len(starts)} starts, {len(middles)} middles, {len(ends)} ends")
    sys.exit(2)
start, middle, end = starts[0], middles[0], ends[0]
if not start < middle < end:
    print("refused: markers out of order")
    sys.exit(2)

ours = lines[start + 1 : middle]
theirs = lines[middle + 1 : end]
while ours and ours[-1] == "":
    ours.pop()
while theirs and theirs[-1] == "":
    theirs.pop()

resolved = lines[:start] + theirs + [""] + ours + lines[end + 1 :]
with open(path, "w", encoding="utf-8") as handle:
    handle.write("\n".join(resolved))
print(f"resolved: main's {len(theirs)} lines, then ours {len(ours)} lines")
