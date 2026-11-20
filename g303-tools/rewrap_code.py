"""Re-wrap lines [first, last] (1-based, inclusive) at `width`, keeping `prefix` on line one and `rest` on the others,
as rewrap.py does, but never breaking inside a backtick code span: a span's spaces are kept within its word."""
import sys

path, first, last, width, prefix, rest = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), sys.argv[5], sys.argv[6]
with open(path, encoding="utf-8") as handle:
  lines = handle.read().split("\n")
chunk = lines[first - 1:last]
text_parts = []
for index, line in enumerate(chunk):
  lead = prefix if index == 0 else rest
  if not line.startswith(lead):
    sys.exit(f"line {first + index} lacks its prefix {lead!r}: {line[:60]!r}")
  text_parts.append(line[len(lead):].strip())
text = " ".join(text_parts)
words, current, inside = [], "", False
for character in text:
  if character == "`":
    inside = not inside
  if character == " " and not inside:
    if current:
      words.append(current)
    current = ""
  else:
    current += character
if current:
  words.append(current)
if inside:
  sys.exit("an unclosed code span in the range")
out, line = [], ""
for word in words:
  lead = prefix if not out else rest
  candidate = (line + " " + word) if line else word
  if line and len(lead) + len(candidate) > width:
    out.append(lead + line)
    line = word
  else:
    line = candidate
out.append((prefix if not out else rest) + line)
lines[first - 1:last] = out
with open(path, "w", encoding="utf-8") as handle:
  handle.write("\n".join(lines))
print(f"{path}: lines {first}-{last} ({last - first + 1}) became {len(out)}, widest {max(len(x) for x in out)}")
