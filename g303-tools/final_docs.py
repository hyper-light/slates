"""The landing commit's docs: each A-650 part 1 passage brought to the tree that lands (the run on it, A-625 part 3's
edge, A-662's stores built). Each edit is a whole paragraph: joined, its replacements each required exactly once,
rewrapped at 120 columns without breaking a code span. Usage: final_docs.py HG_COUNT SERVER_COUNT RUN."""
import sys

HG, SERVER, RUN = sys.argv[1], sys.argv[2], sys.argv[3]
WIDTH = 120

def tokens(text):
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
    sys.exit("an unclosed code span")
  return words

def wrap(text, first, rest):
  out, line = [], ""
  for word in tokens(text):
    lead = first if not out else rest
    candidate = (line + " " + word) if line else word
    if line and len(lead) + len(candidate) > WIDTH:
      out.append(lead + line)
      line = word
    else:
      line = candidate
  out.append((first if not out else rest) + line)
  return out

def edit(path, start_marker, ends, first, rest, replacements, after=None):
  lines = open(path, encoding="utf-8").read().split("\n")
  begin = 0
  if after is not None:
    begin = next(i for i, line in enumerate(lines) if line.startswith(after))
  starts = [i for i in range(begin, len(lines)) if lines[i].startswith(start_marker)]
  if len(starts) != 1:
    sys.exit(f"{path}: {start_marker!r} starts {len(starts)} paragraphs after {after!r}")
  start = starts[0]
  end = next(i for i in range(start + 1, len(lines)) if ends(lines[i]))
  text = " ".join(line[len(first if i == start else rest):].strip() for i, line in enumerate(lines[start:end], start))
  for old, new in replacements:
    if text.count(old) != 1:
      sys.exit(f"{path}: {old!r} found {text.count(old)} times")
    text = text.replace(old, new)
  out = wrap(text, first, rest)
  lines[start:end] = out
  open(path, "w", encoding="utf-8").write("\n".join(lines))
  print(f"{path}: {start_marker[:40]!r} lines {start + 1}-{end} became {len(out)}, widest {max(len(x) for x in out)}")

DESIGN, GAPS = "docs/wip/SLATES_DESIGN.md", "docs/wip/GAPS.md"
quote_end = lambda line: line in ("", ">") or line.startswith("> **")
gaps_end = lambda line: not line.startswith("> ") or line.startswith("> **") or line.startswith("> Owed, in order")
bullet_end = lambda line: line.startswith("- **") or line.startswith("### ") or line == ""

edit(DESIGN, "> **Status (2026-11-19, A-650 part 1: HTTP version 1 served).**", quote_end, "> ", "> ", [
  ("proves credentials as git's site does,",
   "proves credentials as git's site does (by A-625 part 3's record), moves a proven request for a repository another "
   "shard holds there (A-625 part 3),"),
  ("are `503` until piece 12 and A-662.", "are `503` until piece 12 and until the site is handed A-662's stores."),
  ("Proven by `slates-hg --lib` 293 of 293, `slates-http-edge --lib` 32 of 32 and `slates-server --lib vcs_http` 36 of "
   "36 on macOS arm64 (Mac Studio, kiln r6, r33 and r48, 2026-11-19; the other platforms owed):",
   f"Proven by `slates-hg --lib` {HG} of {HG} (this piece's 9 among them), `slates-http-edge --lib` 32 of 32 and "
   f"`slates-server --lib vcs_http` {SERVER} of {SERVER} on macOS arm64 (Mac Studio, kiln {RUN} on the tree that lands "
   "and r6 for the edge, 2026-11-19; the other platforms owed):"),
  ("the site's statuses in hgweb's order;", "the site's statuses in hgweb's order; a proven request moved to its "
   "repository's shard;"),
])
edit(GAPS, "> **A-650 part 1 (2026-11-19, g303): a reply's fields named as its reply begins.**", gaps_end, "> ", "> ", [
  ("`slates-server --lib vcs_http` 36 of 36,", f"`slates-server --lib vcs_http` {SERVER} of {SERVER},"),
  ("(Mac Studio, kiln r6 and r48, 2026-11-19)", f"(Mac Studio, kiln r6 and {RUN}, 2026-11-19)"),
])
edit(GAPS, "> **Built (A-650 part 1):**", gaps_end, "> ", "> ", [
  ("hgweb's route, git's credentials,", "hgweb's route, git's credentials (A-625 part 3's record), a proven request "
   "moved to its repository's shard,"),
  ("`slates-hg --lib` 293 of 293, the 13 captured exchanges among them, and `slates-server --lib vcs_http` 36 of 36 on "
   "macOS arm64 (Mac Studio, kiln r33 and r48, 2026-11-19);",
   f"`slates-hg --lib` {HG} of {HG}, the 13 captured exchanges among them, and `slates-server --lib vcs_http` {SERVER} "
   f"of {SERVER} on macOS arm64 (Mac Studio, kiln {RUN}, 2026-11-19);"),
])
AMEND = "### A-650 (part 1)"
edit(DESIGN, "- **Decided.**", bullet_end, "- ", "  ", [
  ("never judged without the rule.",
   "never judged without the rule. On A-625 part 3's edge the site proves credentials by git's `Authenticator::prove` "
   "over the record (a record that does not answer is `503`, its other refusals `401`, as git's site answers them) "
   "and moves a proven request for a repository another shard holds there (`Answer::Move`) before it looks the "
   "repository up."),
], after=AMEND)
edit(DESIGN, "- **Tests**", bullet_end, "- ", "  ", [
  ("(macOS arm64, Mac Studio, 2026-11-19: `cargo test --locked -p slates-hg --lib` 293 of 293, kiln r33; "
   "`-p slates-http-edge --lib` 32 of 32, r6; `-p slates-server --lib vcs_http` 36 of 36 on the merged tree, r48: "
   "git's site 10, Mercurial's 6, the edge's serving 11 and the waits 9; `cargo fmt --check` clean on the three crates.",
   f"(macOS arm64, Mac Studio, 2026-11-19, on the tree that lands, kiln {RUN}: `cargo test --locked -p slates-hg --lib` "
   f"{HG} of {HG}, this piece's 9 among them; `-p slates-server --lib vcs_http` {SERVER} of {SERVER}: git's site 12, "
   "Mercurial's 7, the daemon's edge 1, the edge's serving 11 and the waits 9; `cargo fmt --check` clean on the three crates; "
   "`-p slates-http-edge --lib` 32 of 32, r6, its inputs unchanged since."),
  ("the refusals' `Allow` and challenge;", "the refusals' `Allow` and challenge; a proven request for a repository "
   "another shard holds moved there, and a credential record that did not answer `503` (A-625 part 3's edge);"),
], after=AMEND)
edit(DESIGN, "- **Owed.**", bullet_end, "- ", "  ", [
  ("The stores the site reads (A-662, g258), and with them the `503` path's removal;",
   "The site handed A-662's stores (their home on each owner shard is built), and with them the `503` path's removal; "
   "the site's refusals counted by kind, as git's are since A-625 part 3;"),
], after=AMEND)
