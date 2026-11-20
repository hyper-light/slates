import shlex, sys
src = open(sys.argv[1], encoding="utf-8").read().split("\n")
# join continuation lines (a trailing backslash outside quotes ends the line)
lines, buf = [], ""
for line in src:
    if buf:
        buf += " " + line.strip()
    else:
        buf = line
    if buf.rstrip().endswith("\\") and buf.lstrip().startswith("match "):
        stripped = buf.rstrip()
        # a backslash at the very end of a match line is a line continuation
        buf = stripped[:-1]
        continue
    lines.append(buf)
    buf = ""
out = []
for line in lines:
    if not line.startswith("match "):
        continue
    words = shlex.split(line)[1:]
    if len(words) == 6:
        glob, text, pattern = words[0], words[4], words[5]
    elif len(words) == 10:
        glob, text, pattern = words[0], words[8], words[9]
    else:
        raise SystemExit("bad line: " + line)
    def strip(arg):
        return arg[3:] if arg.startswith("XXX/") else arg
    text, pattern = strip(text), strip(pattern)
    if glob not in ("0", "1"):
        raise SystemExit("bad glob column: " + line)
    def lit(s):
        hashes = "#" * (max((len(r) for r in __import__("re").findall('"#*', s)), default=0))
        return "br" + hashes + '"' + s + '"' + hashes
    out.append("  (%s, %s, %s)," % ("true" if glob == "1" else "false", lit(text), lit(pattern)))
print(len(out), file=sys.stderr)
print("\n".join(out))
