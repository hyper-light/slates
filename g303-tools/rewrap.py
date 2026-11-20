"""Re-wrap lines [first, last] (1-based, inclusive) of a file at `width` columns, keeping `prefix` on each line
(the first line's own prefix for line one, `rest` for the others). Words are never split; nothing else is touched."""
import sys

path, first, last, width, prefix, rest = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), sys.argv[5], sys.argv[6]
with open(path, encoding="utf-8") as handle:
    lines = handle.read().split("\n")
chunk = lines[first - 1:last]
words = []
for index, line in enumerate(chunk):
    lead = prefix if index == 0 else rest
    if not line.startswith(lead):
        sys.exit(f"line {first + index} lacks its prefix {lead!r}: {line[:60]!r}")
    words.extend(line[len(lead):].split())
out, current = [], prefix
for word in words:
    candidate = current + ("" if current in (prefix, rest) else " ") + word
    if len(candidate) > width and current not in (prefix, rest):
        out.append(current)
        current = rest + word
    else:
        current = candidate
out.append(current)
lines[first - 1:last] = out
with open(path, "w", encoding="utf-8") as handle:
    handle.write("\n".join(lines))
print(f"{path}: lines {first}-{last} ({last - first + 1}) became {len(out)}, widest {max(len(line) for line in out)}")
