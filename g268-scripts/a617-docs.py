"""Rebuild GAPS and the design from origin/main plus A-617 part 1's lines (run in the worktree)."""
import pathlib, subprocess, textwrap, sys

ENTRY_FILE = pathlib.Path(sys.argv[1])  # the A-617 part 1 amendment entry, kept outside the tree

def main_text(path):
    return subprocess.run(["git", "show", f"origin/main:{path}"], capture_output=True, text=True, check=True).stdout

def replace_in_paragraph(text, prefix, start_text, end_text, new_text, width):
    """Replaces the span from start_text through end_text, matched across line wraps of lines beginning `prefix`,
    and rewraps only the lines the span touched."""
    lines = text.split("\n")
    starts = [i for i, line in enumerate(lines) if start_text in line]
    if len(starts) != 1:
        raise SystemExit(f"{start_text!r}: {len(starts)} lines")
    start = starts[0]
    for stop in range(start, start + 16):
        window = lines[start:stop + 1]
        if not all(line.startswith(prefix) for line in window):
            raise SystemExit(f"window left the paragraph at {stop}")
        flat = " ".join(line[len(prefix):] for line in window)
        if end_text in flat[flat.index(start_text):]:
            break
    else:
        raise SystemExit(f"{end_text!r} not found")
    i = flat.index(start_text)
    j = flat.index(end_text, i) + len(end_text)
    flat = flat[:i] + new_text + flat[j:]
    wrapped = textwrap.wrap(flat, width=width - len(prefix), break_long_words=False, break_on_hyphens=False)
    lines[start:stop + 1] = [prefix + w for w in wrapped]
    return "\n".join(lines)

FIXED = ("Found by it and fixed by A-617 (2026-11-19, g268): `LockTable::verdict` verified the locks a commit meets in "
 "the byte order of their paths, where FSFS checks the changed paths in `svn_path_compare_paths` order and returns at "
 "the first that fails (`transaction.c` 3537-3539, 3578), so a commit refused by locks at both `/t/a/b` and `/t/a-b` "
 "was answered at `/t/a-b` where `svnserve` answers `/t/a/b`; it now meets them in FSFS's order, and A-602's oracle "
 "accepts only the first failing path's locks (it had accepted any lock in the commit).")
gaps = main_text("docs/wip/GAPS.md")
if "fixed by A-617" not in gaps:
    gaps = replace_in_paragraph(gaps, "> ", "Found by it, owed to A-602", "so it follows when that order does.",
                                FIXED, 120)
pathlib.Path("docs/wip/GAPS.md").write_text(gaps)

ORDER = ("A commit with no principal that meets a lock is `E160034`. The first lock that fails answers, in the order "
 "FSFS's `verify_locks` meets them: the changed paths in Subversion's path order, a path's children right after it, "
 "so `/t/a/b` before `/t/a-b` (A-617).")
design = main_text("docs/wip/SLATES_DESIGN.md")
if "so `/t/a/b` before `/t/a-b` (A-617)" not in design:
    design = replace_in_paragraph(design, "   ", "A commit with no principal that meets", "lock is `E160034`.", ORDER, 120)
entry = ENTRY_FILE.read_text().rstrip("\n") + "\n"
if "### A-617 part 1 " not in design:
    design = design.rstrip("\n") + "\n\n" + entry
pathlib.Path("docs/wip/SLATES_DESIGN.md").write_text(design)
print("rebuilt from origin/main")
