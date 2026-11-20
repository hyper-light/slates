"""Reflow over-long Rust comment paragraphs to a width, refusing any change but line breaks.

A paragraph is a run of comment lines with one indent and one marker (`//!`, `///`, `//`),
ended by a marker-only line, a list item's start, a fence, or a line of another shape. Only a
paragraph holding a line past the width is rewrapped; a list item keeps a hanging indent.
"""
import re
import sys

WIDTH = 100
LINE = re.compile(r"^(\s*)(//!|///|//)( ?)(.*)$")
ITEM = re.compile(r"^(- |\* |\d+\. )")


def parse(line):
    match = LINE.match(line)
    if not match or line.lstrip().startswith("////"):
        return None
    indent, marker, _space, body = match.groups()
    return indent, marker, body


def wrap(prefix, hang, words):
    lines, current = [], prefix
    first = True
    for word in words:
        lead = prefix if first else prefix + hang
        if current.strip() == lead.strip() and current == lead:
            candidate = current + word
        else:
            candidate = current + " " + word
        if len(candidate) > WIDTH and current != lead:
            lines.append(current)
            first = False
            current = prefix + hang + word
        else:
            current = candidate
    lines.append(current)
    return lines


def reflow(text):
    lines = text.split("\n")
    out, index, fenced = [], 0, False
    while index < len(lines):
        parsed = parse(lines[index])
        if parsed is None:
            out.append(lines[index])
            index += 1
            continue
        indent, marker, body = parsed
        if body.startswith("```"):
            fenced = not fenced
        if fenced or body.startswith("```") or body == "" or body.startswith(("#", "|", ">")):
            out.append(lines[index])
            index += 1
            continue
        item = ITEM.match(body)
        hang = " " * len(item.group(1)) if item else ""
        para = [lines[index]]
        words = body.split()
        index += 1
        while index < len(lines):
            nxt = parse(lines[index])
            if nxt is None or nxt[0] != indent or nxt[1] != marker:
                break
            nbody = nxt[2]
            if nbody == "" or nbody.startswith(("```", "#", "|", ">")) or ITEM.match(nbody):
                break
            if hang and not nbody.startswith(hang):
                break
            para.append(lines[index])
            words.extend(nbody.split())
            index += 1
        if all(len(line) <= WIDTH for line in para):
            out.extend(para)
            continue
        out.extend(wrap(f"{indent}{marker} ", hang, words))
    return "\n".join(out)


def comment_words(text):
    words = []
    for line in text.split("\n"):
        parsed = parse(line)
        if parsed:
            words.append(parsed[1])
            words.extend(parsed[2].split())
        else:
            words.append(("CODE", line))
    return [w for w in words if w not in ("//!", "///", "//")]


for path in sys.argv[1:]:
    before = open(path).read()
    after = reflow(before)
    if comment_words(before) != comment_words(after):
        sys.exit(f"{path}: words changed; nothing written")
    long = [n for n, l in enumerate(after.split("\n"), 1) if len(l) > WIDTH and parse(l)]
    if after != before:
        open(path, "w").write(after)
    print(f"{path}: {'rewrapped' if after != before else 'unchanged'}; long comment lines left {long}")
