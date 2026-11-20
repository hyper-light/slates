"""Reflows the comment paragraphs of the files named (wholly the caller's own files) to 120 characters."""
import re
import sys

WIDTH = 120
COMMENT = re.compile(r"^(\s*)(//!|///|//)( (.*))?$")


def flush(prefix, words, out):
    line = prefix
    for word in words:
        candidate = word if line == prefix else line + " " + word
        if line != prefix and len(candidate if line == prefix else line + " " + word) > WIDTH:
            out.append(line)
            line = prefix + word
        else:
            line = prefix + word if line == prefix else line + " " + word
    if line != prefix:
        out.append(line)


def reflow(lines):
    out, words, prefix = [], [], None
    for line in lines:
        match = COMMENT.match(line)
        if match is None:
            if words:
                flush(prefix, words, out)
                words = []
            prefix = None
            out.append(line)
            continue
        indent, marker, _, body = match.groups()
        this_prefix = indent + marker + " "
        if body is None or body.strip() == "" or (prefix is not None and this_prefix != prefix):
            if words:
                flush(prefix, words, out)
                words = []
        if body is None or body.strip() == "":
            out.append(indent + marker)
            prefix = None
            continue
        prefix = this_prefix
        words.extend(body.split())
    if words:
        flush(prefix, words, out)
    return out


for path in sys.argv[1:]:
    with open(path, encoding="utf-8") as handle:
        lines = handle.read().split("\n")
    trailing = lines[-1] == ""
    body = lines[:-1] if trailing else lines
    result = reflow(body)
    with open(path, "w", encoding="utf-8") as handle:
        handle.write("\n".join(result) + ("\n" if trailing else ""))
