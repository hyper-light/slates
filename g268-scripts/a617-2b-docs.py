"""A-617 part 2b's document lines, inserted into origin/main's design, GAPS and research notes (run in the worktree).

Re-runnable after any merge: it starts from origin/main's text each time. Arguments: the merged tree's commit, its
test count, its seconds and its clock.
"""
import pathlib, subprocess, sys, textwrap

MERGED, PASSED, SECONDS, CLOCK = sys.argv[1:5]
WIDTH = 120


def main_text(path):
  return subprocess.run(["git", "show", f"origin/main:{path}"], capture_output=True, text=True, check=True).stdout


def wrap(text, first, rest):
  return textwrap.wrap(text, width=WIDTH, initial_indent=first, subsequent_indent=rest, break_long_words=False,
                       break_on_hyphens=False)


def replace_in_paragraph(text, prefix, start_text, end_text, new_text):
  lines = text.split("\n")
  starts = [i for i, line in enumerate(lines) if start_text in line]
  if len(starts) != 1:
    raise SystemExit(f"{start_text!r}: {len(starts)} lines")
  start = starts[0]
  for stop in range(start, start + 16):
    window = lines[start:stop + 1]
    if not all(line.startswith(prefix) for line in window):
      raise SystemExit(f"{start_text!r}: the window left the paragraph at line {stop + 1}")
    flat = " ".join(line[len(prefix):] for line in window)
    if end_text in flat[flat.index(start_text):]:
      break
  else:
    raise SystemExit(f"{end_text!r} not found after {start_text!r}")
  i = flat.index(start_text)
  j = flat.index(end_text, i) + len(end_text)
  lines[start:stop + 1] = wrap(flat[:i] + new_text + flat[j:], prefix, prefix)
  return "\n".join(lines)


STATUS = (
  "**Status (2026-11-19, A-617, part 2b).** Built: the commit's drive (`slates-svn`'s `commit::drive`, piece 7's "
  "commit, its second part). `commit` is answered up to its drive as `serve.c` answers it, the lock tokens joined to "
  "the session's path. Each editor command is checked as `svnserve`'s commit editor checks it, in its order and "
  "words: `open-root` past the youngest; the out-of-date test at `delete-entry`, `open-file` and a directory's first "
  "property change (A-592's); a deletion of an absent path, an add over a present one, a copy onto one unless its "
  "directory was copied, a copy's source outside this repository or absent at its revision, `open-dir` and "
  "`open-file` of absent paths; and FSFS's lock check at each call that changes a path (A-602's, over that one path, "
  "at or beneath it for a deletion, a directory's addition, a copy and a directory's property change). Text deltas "
  "are decoded and applied a window at a time against the file's text in the transaction by the svndiff codec, the "
  "base and resulting MD5 checked (`E200014`), the new text's MD5 and SHA-1 taken as it is written (aws-lc-rs); a "
  "property change is one new list. `close-edit` runs A-592's verdict and hands the accepted work to the owner, who "
  "commits it and writes `svnserve`'s answer. Token faults and a client's failure end the connection; a refusal is "
  "written at once and every item after it dropped until `abort-edit`. The drive keeps the revision's rows, folded "
  "as FSFS folds them, and reads them over the index at its base, borrowing nothing between commands, since the owner "
  "appends other commits while drives run. Held to the seventeen recorded commits replayed in their order (bob's lock "
  "taken with its recorded token, bob's race landing during alice's drive), every answer from `commit` on byte for "
  "byte as `svnserve` wrote it, revisions 1 to 13 and the out-of-date, lock and conflict refusals among them; and to "
  "hostile drives, each refusal in `svnserve`'s number and words, every guard red-checked. Owed (A-617's next part): "
  "`svn_repos__validate_prop`'s property checks, the authorization checks (`check_authz`, A-606's rules), an added "
  "path's validation (`svn_repos__validate_new_path`), a session that must authenticate for write before `commit`, "
  "and the owner's commit step with the connection task that runs the drive."
)

ENTRY_TITLE = (
  "### A-617 part 2b — The commit's drive: each editor command checked as `svnserve`'s commit editor checks it, in its "
  "order and words, the transaction read over the index at its base, held to stock `svn` 1.15.0's commits replayed in "
  "their order (2026-11-19, g268 for UI goal 93; §4.17.4 items 3.2 to 3.4, \"5. The wire\", piece 7)"
)
ENTRY_APPLIED = (
  "Applied in the same change to: §4.17.4's status (A-617, part 2b; A-592's, whose owed drive is built), GAPS (the "
  "§4.17.4 entry), the research notes (§1's `svn_fs_util.h` and `checksum.c` rows; §14's drive), "
  "`crates/svn/src/commit.rs` (`pub mod drive`, its doc sentence), `crates/svn/src/commit/drive.rs` and "
  "`crates/svn/src/commit/drive/tests.rs` (new)."
)
ENTRY_BULLETS = [
  "**What was there.** Part 2a read the commit's request and each editor command. A-592's out-of-date test and "
  "verdict waited for a drive to ask them.",

  "**The decision.** `Drive::command` takes one item and writes what `svnserve` writes for it: `begin` answers "
  "`commit` up to its drive, `write_commit_info` answers an accepted work once the owner committed it. The green is a "
  "seam (`Green`: texts and property lists by identity, and the identities the work's new ones get). Refusals are "
  "`DriveRefusal`, each with `svnserve`'s number and words, an error that has no message given the code's own "
  "(`svn_err_best_message`: \"Invalid character in hex checksum\", \"LZ4 decompression failed\"). The transaction is "
  "the drive's rows read over the index at its base, not A-592's `Trial`: a trial borrows the index for the whole "
  "drive, and the owner must append other commits during it (the recorded race does), so the drive borrows nothing "
  "between commands and the verdict builds the full trial once. Departures, each for an input no stock client sends: "
  "a path that is not UTF-8 is refused; a copy source continuing the root URL without a `/` is from another "
  "repository; \"File already exists\" names the green where FSFS names its directory on disk; a file closed with its "
  "delta open keeps its text; the room the work's texts and lists may hold is the caller's.",

  "**Evidence** (the Mac Studio d1323898, kiln, 2026-11-19, debug, a fresh clone of the pushed branch, its own target "
  "directory). First build (06:12 UTC): one error, a closure's lifetime, made a function. First run (06:13 UTC): the "
  "replay of every recorded commit passed; one hostile case was wrong in the test (the import's `gone/` stands at "
  "revision 3) and was fixed there. Red (06:17 to 06:19 UTC), each guard removed in the Studio's copy: `open-file`'s "
  "out-of-date test, 3 of 5 tests failed; the drop after a refusal, 3 of 5; the lock check at each call, the lock "
  "test alone (the verdict refuses the same bytes at `close-edit`, so the replay cannot tell them apart); the read "
  "through a copy, the copy test alone (no recorded commit opens a path beneath a copy). Green (99674c480, 06:20 UTC, "
  "load 7.1): `cargo test --locked -p slates-svn --lib` 321 of 321 in 9.27 s, `cargo fmt -p slates-svn -- --check` "
  f"clean. On the tree merged with main ({MERGED}, {CLOCK} UTC): {PASSED} of {PASSED} in {SECONDS} s, rustfmt clean. "
  "Clippy, `xtask check` and the doc gate are main's (the run-request queue).",

  "**Owed.** `svn_repos__validate_prop`, the authorization checks (A-606), `svn_repos__validate_new_path`, "
  "authenticating for write before `commit`, the owner's commit step and the connection task (A-617's next part).",
]

GAPS_2A_START = "(see A-617 part 2a). Owed from it (part 2b)"
GAPS_2A_END = "answers `svnserve` writes."
GAPS_2B = (
  "(see A-617 part 2a). A-617 part 2b (2026-11-19, g268, UI goal 93): the drive (`commit::drive`), each command "
  "checked as it arrives in `svnserve`'s order and words, FSFS's lock check at each call, text deltas applied a "
  "window at a time with their MD5 checked, `close-edit`'s verdict and the early-error rule, its rows read over the "
  "index at its base with nothing borrowed between commands; held to the seventeen recorded commits replayed in their "
  "order, every answer byte for byte (revisions 1 to 13, the out-of-date, lock and conflict refusals among them), and "
  "to hostile drives, each guard red-checked; `cargo test --locked -p slates-svn --lib` on the Mac Studio (see A-617 "
  "part 2b). Owed from it: `svn_repos__validate_prop`'s checks, authorization (A-606), "
  "`svn_repos__validate_new_path`, authenticating for write, the owner's commit step and the connection task."
)
GAPS_592_START = "on the Mac Studio (see A-592). Owed from"
GAPS_592_END = "built a command at a time (piece 7)."
GAPS_592 = (
  "on the Mac Studio (see A-592). Built since (A-617 part 2b): the drive asking the out-of-date test, and the lock "
  "check FSFS also makes at each editor call, as each command arrives."
)
DESIGN_592_START = "every conflict kind and lock refusal among them. Owed with the server"
DESIGN_592_END = "over a transaction built a command at a time."
DESIGN_592 = (
  "every conflict kind and lock refusal among them. The drive asking the out-of-date test, and the lock check FSFS "
  "also makes at each editor call, as each command arrives, are A-617's (part 2b)."
)

RESEARCH_AFTER = "| `subversion/libsvn_ra_svn/editorp.c` (read 2026-11-19 for A-617; 1.15.0's is the same bytes) | `610cffcd14b17ecf` | C |"
RESEARCH_ROWS = [
  "| `subversion/include/private/svn_fs_util.h` (read 2026-11-19 for A-617) | `b1f1221b87609113` | C |",
  "| `subversion/libsvn_subr/checksum.c` (read 2026-11-19 for A-617) | `907299438410f21b` | C |",
]
RESEARCH_BULLETS = [
  "**The drive's checks** [C: `commit.c` 413-882; `tree.c`; `svn_fs_util.h` 82-107; `checksum.c` 688-694; `lock.c` "
  "605-630]: `open-root` past the youngest is \"No such revision %ld (HEAD is %ld)\" (428-431), the transaction begun "
  "on the youngest; `delete-entry` checks the path in the transaction, then (absent) \"'%s' is out of date\", then "
  "the out-of-date test when it names a revision (487-509); an add with a copy refuses a present target unless its "
  "directory was copied, then a source outside the repository (\"Source url '%s' is from different repository\"), "
  "then the source's revision and path (323-366); an add without one is FSFS's \"File already exists: filesystem "
  "'%s', transaction '%s', path '%s'\"; `open-dir` of an absent path is \"Path '%s' not present\", the relpath as the "
  "client named it (550-554); `open-file` reads the created revision, \"File not found: transaction '%s', path '%s'\" "
  "for an absent one, then the out-of-date test (627-634); a directory's out-of-date test runs at its first property "
  "change only (710-727); `close-file` compares the text's MD5, \"Checksum mismatch for resulting fulltext\\n(%s)\" in "
  "`svn_checksum_mismatch_err`'s layout (679-694); `close-edit` without `open-root` is `E165002` (811-813).",

  "**Locks at each call** [C: `tree.c`, with `SVN_FS_TXN_CHECK_LOCKS`]: `fs_delete_node`, `fs_make_dir` and "
  "`copy_helper` check every lock at or beneath the path, `fs_make_file` and `fs_apply_textdelta` the path's own, "
  "`fs_change_node_prop` at or beneath a directory and a file's own; a commit against another's lock is refused at the "
  "first such call (`verify_lock`'s order: no user, then the owner, then the token), the same words the verdict would "
  "give at `close-edit`.",

  "**Words without a message** [C: `marshal.c` `svn_ra_svn__write_cmd_failure`]: an error created without a message "
  "is written with its code's own (`svn_err_best_message`): `svn_checksum_parse_hex`'s refusal is \"Invalid character "
  "in hex checksum\" and an undecodable LZ4 block \"LZ4 decompression failed\" (`svn_error_codes.h` 221, 1498). A "
  "checksum is read as its first 32 hexadecimal digits, the rest ignored, and the all-zero digest is no checksum at "
  "all.",
]


def design(text):
  if "**Status (2026-11-19, A-617, part 2b).**" not in text:
    lines = text.split("\n")
    starts = [i for i, line in enumerate(lines) if line.startswith("> **Status (2026-11-19, A-617, part 2a).**")]
    if len(starts) != 1:
      raise SystemExit("part 2a's status not found once")
    end = starts[0]
    while end + 1 < len(lines) and lines[end + 1].startswith("> "):
      end += 1
    lines[end + 1:end + 1] = [">"] + wrap(STATUS, "> ", "> ")
    text = "\n".join(lines)
  if "The drive is part 2b's, below." not in text:
    text = replace_in_paragraph(text, "> ", "Owed (part 2b): the drive", "(`editorp.c` 1149-1208).",
                                "The drive is part 2b's, below.")
  if "are A-617's (part 2b)." not in text:
    text = replace_in_paragraph(text, "> ", DESIGN_592_START, DESIGN_592_END, DESIGN_592)
  if "### A-617 part 2b " not in text:
    entry = [ENTRY_TITLE] + wrap(ENTRY_APPLIED, "", "")
    for bullet in ENTRY_BULLETS:
      entry += wrap(bullet, "- ", "  ")
    text = text.rstrip("\n") + "\n\n" + "\n".join(entry) + "\n"
  return text


def gaps(text):
  if "A-617 part 2b (2026-11-19" not in text:
    text = replace_in_paragraph(text, "> ", GAPS_2A_START, GAPS_2A_END, GAPS_2B)
  if "Built since (A-617 part 2b)" not in text:
    text = replace_in_paragraph(text, "> ", GAPS_592_START, GAPS_592_END, GAPS_592)
  return text


def research(text):
  if RESEARCH_ROWS[0] not in text:
    if text.count(RESEARCH_AFTER) != 1:
      raise SystemExit("editorp.c's row not found once")
    text = text.replace(RESEARCH_AFTER, "\n".join([RESEARCH_AFTER] + RESEARCH_ROWS))
  if "**The drive's checks**" not in text:
    bullets = []
    for bullet in RESEARCH_BULLETS:
      bullets += wrap(bullet, "- ", "  ")
    text = text.rstrip("\n") + "\n" + "\n".join(bullets) + "\n"
  return text


for path, edit in [("docs/wip/SLATES_DESIGN.md", design), ("docs/wip/GAPS.md", gaps),
                   ("docs/wip/research/vcs-servers-subversion.md", research)]:
  pathlib.Path(path).write_text(edit(main_text(path)))
print("rebuilt from origin/main with A-617 part 2b's lines")
