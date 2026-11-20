"""A-655 part 1's document lines, inserted into origin/main's design and GAPS (run in the worktree; re-runnable).
Arguments: the merged tree's commit, its test count, its seconds and its clock."""
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
    flat = " ".join(line[len(prefix):] for line in lines[start:stop + 1])
    if end_text in flat[flat.index(start_text):]:
      break
  else:
    raise SystemExit(f"{end_text!r} not found")
  i = flat.index(start_text)
  j = flat.index(end_text, i) + len(end_text)
  lines[start:stop + 1] = wrap(flat[:i] + new_text + flat[j:], prefix, prefix)
  return "\n".join(lines)


STATUS = (
  "**Status (2026-11-19, A-655, part 1).** Built: the commit's remaining checks and its one entry point (`slates-svn`'s "
  "`commit::drive`). `commit::drive::commit`, which the svn:// connection task calls with `commit`'s parameters "
  "(A-656), decides the principal's access as `svnserve`'s `must_have_access` does: allowed, the trivial "
  "authentication request; an anonymous session that could authenticate for write is asked to (`Begun::Authenticate`) "
  "and calls again once it has, no trivial request then; else \"Authorization failed\" (`E170001`); each lock token's "
  "path must be writable by a named principal (`add_lock_tokens`). In the drive, an added path holding a control "
  "character is refused (`svn_repos__validate_new_path`, `E160005`); `svn_repos_fs_change_node_prop`'s checks refuse "
  "`svn:mergeinfo` holding a NUL or not parsing (A-605's parser), a property `svn:wc:` or `svn:entry:` (`E165002`), an "
  "`svn:` value not UTF-8 or holding a carriage return, and an `svn:date` that is no date as `svn_time_from_cstring` "
  "reads one, each written with every error of Subversion's chain; and `check_authz` asks A-606's rules, compiled for "
  "the principal, at each of `commit.c`'s calls (\"Access denied\", `E220004` and `E220001`). Held to the recorded "
  "commits, which now begin through `commit`, and to fifteen cases of the checks and three of access, each red first. "
  "Owed (part 2): the owner's commit step through A-635's `RepositoryLogs::append`, which a dump's import (A-652 part "
  "3) also drives, a revision at a time with the dump's author, date and log; the mergeinfo parser's own words beneath "
  "`verify_mergeinfo`'s (A-605's messages)."
)
TITLE = (
  "### A-655 part 1 — The commit's remaining checks in `commit.c`'s order and words, and its one entry point for the "
  "svn:// connection task (2026-11-19, g268 for UI goal 93; §4.17.4 items 3.2 and 10, \"5. The wire\", piece 7)"
)
APPLIED = (
  "Applied in the same change to: §4.17.4's status (A-655, part 1), GAPS (the §4.17.4 entry), "
  "`crates/svn/src/commit/drive.rs` and `crates/svn/src/commit/drive/tests.rs`."
)
BULLETS = [
  "**What was there.** A-617's drive refused what was out of date or locked, but took any property, any added path "
  "and any principal, and its beginning assumed a session already allowed to write.",

  "**The decision.** One entry point, `commit(params, &Session, Start, &mut out) -> Result<Begun, ItemError>`: the "
  "`Session` is what `svnserve`'s server baton holds (the base path, the root URL, the green's name, the principal, "
  "`anon-access` and `auth-access`, whether the client could authenticate, the authz rules and the repository's name "
  "in them), the `Start` what the owner gives (the transaction's name, the instant, the room, whether the client has "
  "just authenticated), and `Begun` says what follows: drive every item (`Driving`), authenticate then call again "
  "(`Authenticate`), refused (`Answered`) or closed (`Close`). The checks sit where `commit.c` makes them, before "
  "FSFS's: the new path first in an addition; the authorization at each call; the property's checks before the "
  "node's locks. A failure is written with every error of Subversion's chain (`svn_ra_svn__write_cmd_failure`), the "
  "code's own words for an error made without any. Departures: a date in the format `svn` stopped writing in 2002 "
  "is refused; beneath `verify_mergeinfo`'s words, the parser's own are owed with A-605's messages.",

  "**Evidence** (the Mac Studio d1323898, kiln, 2026-11-19, debug, a fresh clone of the pushed branch, its own target "
  "directory). Red (07:08 UTC): the checks' test failed at its first case, the control character taken; green (07:12 "
  "UTC): the drive's 7 of 7. The entry point red (07:15 UTC, a stub that closed): 9 of 9 drive tests failed, the "
  "replay among them; green (07:16 UTC): 9 of 9. `cargo fmt` from the Studio (sha256 dcf19ad4). On the tree merged "
  f"with main ({MERGED}, {CLOCK} UTC): `cargo test --locked -p slates-svn --lib` {PASSED} of {PASSED} in {SECONDS} s, "
  "rustfmt clean. Clippy, `xtask check` and the doc gate are main's (the run-request queue).",

  "**Owed.** Part 2: the owner's commit step through A-635's `RepositoryLogs::append`, at the position the commit "
  "read (`Moved` answered as `svnserve`'s out-of-date), which a dump's import drives too (A-652 part 3).",
]
GAPS_AFTER = "authenticating for write, the owner's commit step and the connection task."
GAPS_SENTENCE = (
  " A-655 part 1 (2026-11-19, g268, UI goal 93): the commit's remaining checks in `commit.c`'s order and words "
  "(`svn_repos__validate_new_path`, `svn_repos__validate_prop` with `verify_mergeinfo`, `check_authz` through A-606) "
  "and authenticating for write (`must_have_access`, the lock tokens' writes), through one entry point for the "
  "svn:// connection task (`commit::drive::commit`, A-656); `cargo test --locked -p slates-svn --lib` on the Mac "
  "Studio, red first (see A-655 part 1). Owed from it: part 2, the owner's commit step through A-635's "
  "`RepositoryLogs::append`, which a dump's import drives too (A-652 part 3)."
)


def design(text):
  if "**Status (2026-11-19, A-655, part 1).**" not in text:
    lines = text.split("\n")
    starts = [i for i, l in enumerate(lines) if l.startswith("> **Status (2026-11-19, A-617, part 2b).**")]
    if len(starts) != 1:
      raise SystemExit("part 2b's status not found once")
    end = starts[0]
    while end + 1 < len(lines) and lines[end + 1].startswith("> "):
      end += 1
    lines[end + 1:end + 1] = [">"] + wrap(STATUS, "> ", "> ")
    text = "\n".join(lines)
  if "### A-655 part 1 " not in text:
    entry = [TITLE] + wrap(APPLIED, "", "")
    for bullet in BULLETS:
      entry += wrap(bullet, "- ", "  ")
    text = text.rstrip("\n") + "\n\n" + "\n".join(entry) + "\n"
  return text


def gaps(text):
  if "A-655 part 1 (2026-11-19" in text:
    return text
  return replace_in_paragraph(text, "> ", "authenticating for write, the owner's commit step and the", GAPS_AFTER,
                              GAPS_AFTER + GAPS_SENTENCE)


for path, edit in [("docs/wip/SLATES_DESIGN.md", design), ("docs/wip/GAPS.md", gaps)]:
  pathlib.Path(path).write_text(edit(main_text(path)))
print("rebuilt from origin/main with A-655 part 1's lines")
