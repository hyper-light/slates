"""Resolve GAPS' §4.17.3 conflict: main's A-657 Built lines verbatim; A-650 part 1's Built text and its open items;
the Owed list with (4) from A-650's side and (5) onward from main's; the shared tail rewrapped with it."""
import re, sys

PATH = "docs/wip/GAPS.md"
WIDTH = 120
lines = open(PATH, encoding="utf-8").read().split("\n")
start = lines.index("<<<<<<< HEAD")
middle = lines.index("=======", start)
end = lines.index(">>>>>>> origin/main", middle)
head_side, main_side = lines[start + 1:middle], lines[middle + 1:end]
tail_end = next(i for i in range(end + 1, len(lines)) if not lines[i].startswith("> ") or lines[i].startswith("> **"))
tail = lines[end + 1:tail_end]

def strip(line):
  if not line.startswith("> "):
    sys.exit(f"no blockquote prefix: {line[:50]!r}")
  return line[2:].strip()

def wrap(text):
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
  out, line = [], ""
  for word in words:
    candidate = (line + " " + word) if line else word
    if line and 2 + len(candidate) > WIDTH:
      out.append("> " + line)
      line = word
    else:
      line = candidate
  out.append("> " + line)
  return out

owed_at = next(i for i, line in enumerate(main_side) if line.startswith("> Owed, in order"))
if not main_side[0].startswith("> **Built (A-657"):
  sys.exit("main's side does not open with A-657's Built line")
a657 = main_side[:owed_at]
owed_main = " ".join(strip(line) for line in main_side[owed_at:])
head_text = " ".join(strip(line) for line in head_side)
mine_built = head_text[:head_text.index("`414`.") + len("`414`.")]
my_four = head_text[head_text.index("(4, rest) read serving:"):head_text.index("(5, rest) pushes:")]
owed = (owed_main[:owed_main.index("(4, rest)")] + my_four + owed_main[owed_main.index("(5, rest) pushes:"):]
        + " " + " ".join(strip(line) for line in tail))
merged = a657 + wrap(mine_built) + wrap(owed)
lines[start:tail_end] = merged
open(PATH, "w", encoding="utf-8").write("\n".join(lines))
print(f"replaced lines {start + 1}-{tail_end} with {len(merged)}; widest {max(len(x) for x in merged)}")
print("A-657 lines kept verbatim:", len(a657))
print("my (4):", my_four[:200], "...")
