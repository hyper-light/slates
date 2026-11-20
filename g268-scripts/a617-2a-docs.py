"""A-617 part 2a's document lines, inserted into origin/main's design, GAPS and research notes (run in the worktree).

Re-runnable after any merge: it starts from origin/main's text each time, so a conflict in these three documents is
settled by running it again. The run's figures come from the arguments: the merged tree's commit, its test count and
its time.
"""
import pathlib
import subprocess
import sys
import textwrap

MERGED, PASSED, SECONDS, CLOCK, CENSUS = sys.argv[1:6]

WIDTH = 120


def main_text(path):
  return subprocess.run(["git", "show", f"origin/main:{path}"], capture_output=True, text=True, check=True).stdout


def wrap(text, first, rest):
  return textwrap.wrap(text, width=WIDTH, initial_indent=first, subsequent_indent=rest, break_long_words=False,
                       break_on_hyphens=False)


def replace_in_paragraph(text, prefix, start_text, end_text, new_text):
  """Replaces the span from start_text through end_text, matched across the wraps of lines beginning `prefix`, and
  rewraps only the lines the span touched."""
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
  flat = flat[:i] + new_text + flat[j:]
  lines[start:stop + 1] = wrap(flat, prefix, prefix)
  return "\n".join(lines)


STATUS = (
  "**Status (2026-11-19, A-617, part 2a).** Built: the commit's request and its editor commands, read as `svnserve` "
  "reads them (`slates-svn`'s `commit::edit`, piece 7's commit, its first part). `commit`'s parameters are read as "
  "`serve.c` reads them, `\"clb?l\"` or a client before 1.2's one item, and its lock tokens after the authentication "
  "request as `add_lock_tokens` reads them; each editor command by its handler's prototype in `editorp.c`, as "
  "`marshal.c`'s `vparse_tuple` reads one: items past the prototype ignored, an optional rest absent, another kind "
  "than the prototype's malformed. The refusals carry `svnserve`'s numbers: malformed data (`E210004`) ends the "
  "connection, an unknown command (`E210001`) is a command failure; a NUL inside a C string and a revision past "
  "`svn_revnum_t`, which `svnserve` reads through C, are refused. Each command writes itself back as a stock client "
  "writes it. Held to seventeen commits recorded on the Mac Studio, four of them refused by `svnserve`: stock `svn` "
  "1.15.0 against stock `svnserve` 1.15.0 through a relay (`crates/svn/fixtures/commit`, its `make-fixtures.py`, each "
  "file's SHA-256 in `SHA256SUMS`), every command read and written back byte for byte, every text delta decoded whole "
  "and applied to its file's text; and to `vparse_tuple` transcribed over generated items for every prototype. Owed "
  "(part 2b): the drive, each command checked as it arrives (the tokens, the paths, the out-of-date test, the lock "
  "check) and declared on a trial built a command at a time, the texts through the svndiff codec with their MD5 "
  "checked, `close-edit`'s verdict, and the answers `svnserve` writes: after a failure, the commands read and dropped "
  "until `abort-edit` (`editorp.c` 1149-1208)."
)

ENTRY_TITLE = (
  "### A-617 part 2a — The commit's request and its editor commands read as `svnserve` reads them, held to stock "
  "`svn` 1.15.0's own commits recorded through a relay (2026-11-19, g268 for UI goal 93; §4.17.4 \"5. The wire\", "
  "piece 7)"
)

ENTRY_APPLIED = (
  "Applied in the same change to: §4.17.4's status (A-617, part 2a), GAPS (the §4.17.4 entry), the research notes "
  "(`vcs-servers-subversion.md`: §1's `editorp.c` row; §14, new), `crates/svn/src/commit.rs` (`pub mod edit`, its "
  "doc sentence, one line rewrapped), `crates/svn/src/commit/edit.rs` and `crates/svn/src/commit/edit/tests.rs` (new), "
  "`crates/svn/fixtures/commit/` (new: the recording script, 52 streams of 26 connections, `sessions.txt`, the tools' "
  "versions, `SHA256SUMS`), `crates/svn/src/commit/tests.rs` (main's two clippy findings: A-592's oracle `merge`, "
  "which returns a `Result`, answers a node that is not a directory with a typed stop where it asserted, and the "
  "generator's anonymous share reads `is_multiple_of`; the case checker's revision lookup returns its miss rather "
  "than unwrapping it)."
)

ENTRY_BULLETS = [
  "**What was there.** Nothing read `commit` or the editor commands after it: the item codec (A-531) parses items, and "
  "the handshake (A-599) and the read commands (A-613) answer their own. A-592's out-of-date test and verdict waited "
  "for a drive.",

  "**The decision.** One typed value per command, `EditCommand`: a variant for each name `editorp.c`'s table "
  "dispatches and for `failure`, its fields borrowed from the parsed item. `EditCommand::read` reads one as "
  "`vparse_tuple` reads its handler's prototype; `to_value` writes it back as `marshal.c`'s `svn_ra_svn__write_cmd_*` "
  "write it. `CommitRequest::read` reads `commit`'s parameters as `serve.c` does, and `lock_tokens` reads the tokens "
  "as `add_lock_tokens` does, later and as a command failure. The refusals are typed: `EditRefusal::Malformed` with "
  "its `Malformation` (`E210004`, the connection ends), `EditRefusal::UnknownCommand` (`E210001`, a command failure) "
  "and `LockTokensRefusal` (`E210004`, a command failure). Two are stricter than `svnserve`, each for an input no stock "
  "client sends: a NUL inside a C string (`svnserve` reads `a\\0b` as `a`) and a revision past `svn_revnum_t` "
  "(`svnserve` casts it to a negative number, which reads as no revision). Which command may come when, and every "
  "check of the commit editor, are the drive's (part 2b).",

  "**The recordings.** On the Mac Studio, stock `svn`, `svnserve` and `svnadmin` 1.15.0 (r1937767): "
  "`make-fixtures.py` makes a repository and its working copies in a directory of its own, runs 34 steps as two "
  "users, and its `--tunnel` role relays each `svn+fixture://` connection to `svnserve -t`, logging every byte each "
  "way. 25 sessions reached the server over 26 connections, 17 of them commits. Four commits were refused as stock "
  "refuses them: `E160028` twice (a file out of date, and a property change on a stale root), `E160039` (another "
  "user's lock) and `E160024` (a conflict, made by holding the client's stream at `close-edit` while another commit "
  "landed). `sessions.txt` keeps each step's command, user and exit, and the refusals' messages; `SHA256SUMS` each "
  "file's digest.",

  "**Evidence** (the Mac Studio d1323898, kiln, 2026-11-19, debug, a fresh clone of the pushed branch, its own target "
  "directory). Failing first (05:17 UTC): with the readers stubbed, 9 of the new tests failed. Green (05:18 UTC, "
  "eee90bd82): `cargo test --locked -p slates-svn --lib` 245 of 245 in 2.08 s; the prototype oracle's census: 136 "
  "generated items read alike, 93 refused alike, 27 refused by the stricter rules alone. The module's line citations "
  "were checked against the sources at `d434dc14` before landing, and 23 of them corrected. On the tree merged with "
  f"main ({MERGED}, {CLOCK} UTC): {PASSED} of {PASSED} in {SECONDS} s, the census {CENSUS}; `cargo fmt -p slates-svn -- "
  "--check` clean. "
  "Clippy, `xtask check` and the doc gate are main's (the run-request queue).",

  "**Owed.** Part 2b, the drive (§4.17.4's status).",
]

GAPS_IDS_OLD = "A-606 and A-593, D-28;"
GAPS_TITLE = ("its HTTP bodies, its authorization and its dump codec built, and its commit's editor commands read "
              "(2026-11-18, A-500, A-531, A-553, A-602, A-605, A-592, A-599, A-609, A-606, A-593 and A-617, D-28;")
GAPS_AFTER = "lock in the commit)."
GAPS_SENTENCE = (
  " A-617 part 2a (2026-11-19, g268, UI goal 93): the commit's request and its editor commands read as `svnserve` "
  "reads them (`commit::edit`), malformed data (`E210004`, the connection's end) and an unknown command (`E210001`) "
  "refused as it refuses them; held to seventeen commits of stock `svn` 1.15.0 against stock `svnserve` 1.15.0 "
  "recorded through a relay (`crates/svn/fixtures/commit`, SHA-256 checked), each command read and written back byte "
  "for byte and each text delta applied, and to `vparse_tuple` transcribed over generated items; `cargo test --locked "
  "-p slates-svn --lib` on the Mac Studio, red first (see A-617 part 2a). Owed from it (part 2b): the drive, each "
  "command checked as it arrives and declared on a trial built a command at a time, `close-edit`'s verdict and the "
  "answers `svnserve` writes."
)

RESEARCH_ROW_AFTER = "| `subversion/libsvn_ra_svn/client.c` | `3c3b9263dd7223a9` (A-599) | C |"
RESEARCH_ROW = ("| `subversion/libsvn_ra_svn/editorp.c` (read 2026-11-19 for A-617; 1.15.0's is the same bytes) | "
                "`610cffcd14b17ecf` | C |")

RESEARCH_TITLE = "## 14. The commit's request and its editor commands (A-617; §4.17.4 \"5. The wire\", piece 7)"
RESEARCH_LEAD = (
  "Read 2026-11-19 by g268 at §1's revision: `libsvn_ra_svn/editorp.c` (§1's row), `marshal.c`, `svnserve/serve.c`, "
  "`libsvn_ra_svn/client.c`, `libsvn_repos/commit.c` and the protocol (§1's hashes). Recorded the same day on the Mac "
  "Studio: stock `svn` 1.15.0 (r1937767) against stock `svnserve` 1.15.0 over a tunnel whose relay logged every byte "
  "each way (`crates/svn/fixtures/commit`, its `make-fixtures.py`; each file's SHA-256 in `SHA256SUMS`): 25 sessions "
  "over 26 connections, 17 of them commits."
)
RESEARCH_BULLETS = [
  "**The request** [B: `protocol` §3.1.1; C: `serve.c` `commit` 1470-1583, `add_lock_tokens` 1360-1409]: `( "
  "logmsg:string ? ( ( lock-path:string lock-token:string ) ... ) keep-locks:bool ? rev-props:proplist )`, read as "
  "`\"clb?l\"`, or as `\"c\"` when it holds one item (1495-1510: a client before 1.2 sends no tokens, keeps its locks "
  "and sends no revision properties). The tokens are read after the authentication request, each a list of two "
  "strings, and a malformed one is a command failure, \"Lock tokens aren't a list of lists\" (1521-1524). The log "
  "message is used only when no revision properties came (1526-1534), and the author is always the session's "
  "principal (1536-1541). Recorded: stock 1.15.0 sends `svn:log`, `svn:txn-client-compat-version` and "
  "`svn:txn-user-agent`; a token's path relative to the session's URL (`a.txt`); keep-locks false in `svn commit`'s "
  "sessions and true in `svn copy`'s between URLs (`commit-with-lock`, `copy-url`).",

  "**The drive** [C: `editorp.c` `svn_ra_svn_drive_editor2` 1073-1212, the handlers 532-935]: each command `( "
  "name:word params:list )`, read `\"wl\"` (1110), its parameters with its handler's prototype, from `open-root`'s "
  "`\"(?r)s\"` (555) to `close-file`'s `\"s(?c)\"` (856). A name no handler has is \"Unknown editor command\" "
  "(`SVN_ERR_RA_SVN_UNKNOWN_CMD`, 210001, 1127-1132), a command error; an error of the item reader (malformed data, "
  "210004) leaves the loop and ends the connection. On a command error the server aborts the edit, writes the failure "
  "(1149-1174), then reads and drops commands until `abort-edit` or `success`, a disconnection being no error "
  "(1178-1208).",

  "**Reading a prototype** [C: `marshal.c` `vparse_tuple` 1608-1718]: items past the prototype are ignored; `?` lets "
  "the rest be absent; a nested list's own `?` lets it be empty; another kind than the prototype names is "
  "\"Malformed network data\". A `c` item is used as a C string, so `svnserve` reads `a\\0b` as `a`; an `r` item is "
  "cast to `svn_revnum_t` (1643-1644), so a number past `LONG_MAX` turns negative and reads as no revision. slates "
  "refuses both (`commit::edit`'s module documentation); no stock client sends either. A word is at most 24 bytes "
  "(`MAX_WORD_LENGTH` 25, 66; \"Word is too long\").",

  "**What the stock client sends** (the recordings): no `target-rev`; `open-root` first, then the tree in its path "
  "order, every directory closed, the root's too, before any file's text; then a file at a time, `apply-textdelta`, "
  "its `textdelta-chunk`s, `textdelta-end` and `close-file` with the text's MD5; then `close-edit`. So a file's token "
  "outlives its directory's `close-dir`. Tokens are `d<n>` and `c<n>` from one counter. The deltas are svndiff 2 to "
  "`svnserve`, which announces `accepts-svndiff2` (§2), each file's chunks one svndiff stream.",

  "**The answers** (the recordings; C: `serve.c` `commit` 1512-1583, `editorp.c` 888-906): the authentication request "
  "(`must_have_access`, 1517-1519), trivial in a session already authenticated, `( success ( ( ) 0: ) )`, and the "
  "request's `( success ( ) )` (1555); nothing during an accepted drive; at `close-edit`, the editor's `close_edit` "
  "(the verdict) run as a command (893) and `( success ( ) )` (905), a second trivial authentication request (1562), "
  "then `( new-rev:number ( date:string ) ( author:string ) ( post-commit-err:string ) )` (1576), the last list empty "
  "here. A refused commit's client stream still ends at `close-edit`, with no `abort-edit`: the client sends the "
  "whole drive before it reads, and the server's one `( failure ( ( code message file line ) ) )` is its last item: "
  "`E160028` \"File '/trunk/project/a.txt' is out of date\" (an `open-file` on a stale base) and \"Directory "
  "'/trunk/project' is out of date\" (a stale root's property), `E160039` \"User 'alice' does not own lock on path "
  "'/trunk/project/a.txt' (currently locked by 'bob')\", `E160024` \"Conflict at '/trunk/project/sub/deeper/d.txt'\" "
  "(another commit landed while the client's stream was held at `close-edit`).",

  "**A sibling noted, not changed.** §6's `check_out_of_date` (176-196) is at 179-198 of the `commit.c` §1 names "
  "(`1727532bffd25017`), where A-592's code cites it.",
]


def design(text):
  if "**Status (2026-11-19, A-617, part 2a).**" not in text:
    lines = text.split("\n")
    roles = [i for i, line in enumerate(lines) if line.startswith("**Role.** slates serves stock Subversion clients")]
    if len(roles) != 1 or lines[roles[0] - 1] != "" or not lines[roles[0] - 2].startswith("> "):
      raise SystemExit("§4.17.4's status run not found where expected")
    lines[roles[0] - 1:roles[0] - 1] = [">"] + wrap(STATUS, "> ", "> ")
    text = "\n".join(lines)
  if "### A-617 part 2a " not in text:
    entry = [ENTRY_TITLE] + wrap(ENTRY_APPLIED, "", "")
    for bullet in ENTRY_BULLETS:
      entry += wrap(bullet, "- ", "  ")
    text = text.rstrip("\n") + "\n\n" + "\n".join(entry) + "\n"
  return text


def gaps(text):
  if "A-617 part 2a (2026-11-19" in text:
    return text
  text = replace_in_paragraph(text, "> ", "its HTTP bodies, its authorization", GAPS_IDS_OLD, GAPS_TITLE)
  return replace_in_paragraph(text, "> ", "lock in the commit). A-602 (2026-11-19", GAPS_AFTER, GAPS_AFTER + GAPS_SENTENCE)


def research(text):
  if RESEARCH_ROW not in text:
    if text.count(RESEARCH_ROW_AFTER) != 1:
      raise SystemExit("the research notes' client.c row not found once")
    text = text.replace(RESEARCH_ROW_AFTER, RESEARCH_ROW_AFTER + "\n" + RESEARCH_ROW)
  if RESEARCH_TITLE not in text:
    section = [RESEARCH_TITLE, ""] + wrap(RESEARCH_LEAD, "", "") + [""]
    for bullet in RESEARCH_BULLETS:
      section += wrap(bullet, "- ", "  ")
    text = text.rstrip("\n") + "\n\n" + "\n".join(section) + "\n"
  return text


for path, edit in [("docs/wip/SLATES_DESIGN.md", design), ("docs/wip/GAPS.md", gaps),
                   ("docs/wip/research/vcs-servers-subversion.md", research)]:
  pathlib.Path(path).write_text(edit(main_text(path)))
print("rebuilt from origin/main with A-617 part 2a's lines")
