"""A-655 part 2a's document lines, inserted into origin/main's design, GAPS and research notes (run in the worktree;
re-runnable). Arguments: the red runs' words, the green run's words and the merged tree's commit."""
import pathlib, subprocess, sys, textwrap

RED, GREEN, MERGED = sys.argv[1:4]
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
  "**Status (2026-11-19, A-655, part 2a).** Built: what a version records, and the one step that commits it "
  "(`slates-svn`'s `commit::revision` and `migration::Repository`; item 3.5). A `Revision` is the youngest revision "
  "it was judged against or made on, the transaction's name and changed-path rows, the revision properties, the texts "
  "and property lists its rows name, and the lock entries its verdict decided. A client's accepted work is made into "
  "one at the owner's instant (`Revision::of_work`: `svn:date` that instant as `svn_time_to_cstring` writes it, in "
  "place of any the client sent, as FSFS's `write_final_revprop` dates it; the ephemeral `svn:txn-` properties dropped "
  "after pre-commit, as `svn_repos_fs_commit_txn` drops them); an imported dump revision keeps the stream's properties "
  "exactly. The step is `Repository::check`, which changes nothing (the head the youngest, else \"Transaction out of "
  "date\", `E160028`; the lock entries taken; every text and list held or brought; the room; the index's plan of the "
  "rows, `Index::plan`), then `Repository::apply` (the plan committed, `Index::commit_planned`, refused with nothing "
  "changed once a revision was appended since): an owner checks before its log records a revision and applies after, "
  "and a recovery reaches the same records. A-628's import commits each revision through it; a repository's records "
  "hold its lock table and serve the drive as its green. The drive waits for start-commit at `open-root`, its revision "
  "properties checked first (`svn_repos_fs_change_txn_props`), and for pre-commit at `close-edit` before the verdict "
  "(A-661's call points: `Step::Hook`, `Due`, `Drive::resume`), the hook reading the transaction through "
  "`Drive::pending`, a hook's refusal the failure of the command that waited. Held to the recorded commits, each committed through the step at the instant `svnserve` dated it (every "
  "answer byte for byte, bob's race landing in alice's pre-commit, each revision's properties `svnserve`'s), to the "
  "step's refusals each leaving the records whole, and to the same revisions committed again into a fresh repository "
  "reaching the same records. Owed (part 2b, over A-662's per-repository map): the server's step, the green's next "
  "version built from the rows, texts and lists and appended through A-635's `RepositoryLogs::append` at the "
  "revision's head with the Subversion record in the one durable entry, a `Moved` append judged again at the head by "
  "position (A-658), as FSFS merges again, a real conflict alone refused; and its recovery."
)
TITLE = (
  "### A-655 part 2a — One revision value a client's commit and an import's revision are made into, the commit step's "
  "check and apply over a repository's records, and the drive's hook pauses (2026-11-19, g268 for UI goal 93; §4.17.4 "
  "items 3.1, 3.3, 3.5 and 9, piece 7; A-602, A-628, A-661)"
)
APPLIED = (
  "Applied in the same change to: §4.17.4's status (A-655, part 2a), GAPS (the §4.17.4 entry), the research notes "
  "(§16, new), `crates/svn` (`commit.rs`: `pub mod revision` and its documentation; `commit/revision.rs` and "
  "`commit/revision/tests.rs`, new; `commit/drive.rs`: `Step::Hook`, `Due`, `Drive::resume`, `Pending` and "
  "`Drive::pending`, the revision properties' check at `open-root`, `Work::head`, the `svn:txn-` properties dropped; `commit/drive/tests.rs`: the replay over a "
  "repository's records through the step, and the hook and revision-property cases; `index.rs`: `Index::plan`, "
  "`Index::commit_planned`, `Planned`, `IndexRefusal::OutOfDate`, with a test in `index/tests.rs`; `lock.rs`: "
  "`LockTable::check_entries` and `LockTable::apply_all`; `migration.rs`, `migration/records.rs`, "
  "`migration/import.rs`, `migration/properties.rs`: the lock table, the step, `Checked`, the green, the import through "
  "the step; `migration/tests/step.rs`, new; `lib.rs`'s documentation)."
)
BULLETS = [
  "**What was there.** A-617's drive handed an accepted work to its caller, and A-628's import committed a prepared "
  "revision into a repository's records by a private path of its own; no value held what a version records, no one "
  "step committed it for both, and the drive ran its verdict at `close-edit` with no place for the hooks (A-661).",

  "**The value** (`commit::revision::Revision`, main's design 2a): `head`, the youngest revision it was judged against "
  "(FSFS makes the head it merged its transaction's base, `tree.c` 2291) or made on; `record`, the transaction's name "
  "and the changed-path rows; `properties`; `contents`, every text and property list its rows name by content "
  "identity, a list in FSFS's `K`/`V`/`END` form; `lock_entries`. `Revision::of_work(work, now)` dates a client's work "
  "at the owner's instant as `svn_time_to_cstring` writes it (`transaction.c` `write_final_revprop` 3617-3630: FSFS "
  "replaces the date of every transaction begun without `SVN_FS_TXN_CLIENT_DATE`, and `svnserve` begins its with "
  "`SVN_FS_TXN_CHECK_LOCKS` alone, `fs-wrap.c` 151-152), so a replay never reads a clock; an import's revision keeps "
  "the stream's properties exactly, an absent `svn:date` absent (`load-fs-vtable.c` `close_revision`).",

  "**The step** (`migration::Repository`, whose records gain the lock table): `check(revision) -> Result<Checked, "
  "StepRefusal>` changes nothing and refuses, in order, a head that is not the youngest (`OutOfDate`, `E160028` "
  "\"Transaction out of date\", `transaction.c` `commit_body` 3784-3786), a lock entry the table does not take "
  "(`LockTable::check_entries`, all of them in their order), a text or list the rows name that neither the records "
  "hold nor the revision brings (`Corrupt`), records past the room (`Room`), and rows the index refuses "
  "(`Index::plan`, every check `append` makes); `apply(checked)` commits it (`Index::commit_planned`, which refuses a "
  "plan once a revision was appended after it; `LockTable::apply_all`; the new contents and the properties kept), "
  "refused with nothing changed should the records have moved since; `commit` is the two at once. So an owner checks "
  "a revision before its log records it and applies it after, and a recovery that checks and applies the logged "
  "revision again reaches the same records. The records' out-of-date check is `commit_body`'s; the owner's step "
  "(part 2b, main's decision) first judges a work whose head moved again at the head by position, as "
  "`svn_fs_fs__commit_txn` merges again (`tree.c` 2295-2310; A-658's `judge_computed`), so a client is refused only "
  "where the verdict finds a conflict. A-628's import makes each dump revision into a `Revision` "
  "(`Repository::revision_of`) and commits it through the step, its refusals mapped to the import's own (`From<"
  "StepRefusal> for MigrationRefusal`); a repository's records also serve the drive as its `Green`, naming texts and "
  "lists by BLAKE3 as they keep them.",

  "**The hooks' places** (A-661's call points, main's brief): `open-root` checks its revision, then each revision "
  "property as `svn_repos_fs_change_txn_props` does inside `svn_repos_fs_begin_txn_for_commit2` (`fs-wrap.c` 160-164, "
  "344-357; a gap part 1 left: an `svn:log` with a carriage return or an `svn:date` that is no date was taken), then "
  "returns `Step::Hook(Due::StartCommit)`; `close-edit` returns `Step::Hook(Due::PreCommit)` before the verdict, as "
  "`svn_repos_fs_commit_txn` runs pre-commit before `svn_fs_commit_txn`'s merge (`fs-wrap.c` 69-71). "
  "`Drive::resume(index, green, locks, answer, out)`: passed, `open-root`'s root opens on the youngest it read and "
  "`close-edit`'s verdict judges against the head as it stands then; refused, the hook's errors are written as the "
  "command's failure and the drive drops until `abort-edit`. An item handed in while a hook is due, or a resume with "
  "none, ends the drive. While its command waits, the hook reads the transaction through `Drive::pending` (its name, "
  "base, revision properties, rows, texts and property lists), over which FSFS's read-only view of a transaction, "
  "owed, is built (main's answer). The accepted work names the head its verdict judged against and keeps no `svn:txn-` "
  "property (`fs-wrap.c` 72-89).",

  "**Departures.** The revision properties are checked in name order, where `svnserve` meets them in APR's seeded hash "
  "order, so of two bad ones slates names the first by name. An owner's instant before 1970 or past the year 9999 is "
  "refused (`Date`), where `svn_time_to_cstring` writes any; no owner's clock reads one.",

  "**Evidence** (kiln, 2026-11-19, debug, Windows x86_64 on jupiter while the Mac Studio d1323898 was offline, main's "
  "order; each copy a fresh clone of the pushed branch with its own target directory). "
  f"{RED} {GREEN} The merge with main ({MERGED}) brought no input of `slates-svn` (`crates/svn`, the lock file, the "
  "workspace manifest and the toolchain pin unchanged), so the green run stands for the landed tree. The macOS run is "
  "owed (the run-request queue), as are clippy, `xtask check` and the doc gate (main's).",

  "**Owed.** Part 2b, over A-662's per-repository map once it is on main: the repository's Subversion records held in "
  "its Subversion arm; the server's step, turning a checked revision into the green's next version (an `Increment` "
  "from the rows, texts and property lists), appended through A-635's `RepositoryLogs::append` at the revision's head "
  "with the Subversion record named in the one durable entry, a `Moved` append judged again at the head by position "
  "(A-658's `judge_computed`), as FSFS merges again, a real conflict alone refused, then `Repository::apply`; the "
  "record's encoding with golden vectors and hostile inputs; its recovery replaying check and "
  "apply. Then A-652 part 3 over it. Found, not fixed: the crate keeps two writers of `svn_time_to_cstring` "
  "(`reads`'s and `dav::response::svn_time`, which the step uses).",
]
GAPS_START = "which a dump's import drives too (A-652 part 3). A-602"
GAPS_END = "which a dump's import drives too (A-652 part 3)."
GAPS_SENTENCE = (
  " A-655 part 2a (2026-11-19, g268, UI goal 93): one `Revision` a client's accepted work (dated at the owner's "
  "instant, its `svn:txn-` properties dropped) or an imported dump revision is made into, committed by one step over a "
  "repository's records (`Repository::check`, then `apply`; `Index::plan` and `commit_planned`; the lock entries all "
  "or none), which A-628's import now takes too; the drive waits for start-commit at `open-root`, its revision "
  "properties checked first, and for pre-commit before the verdict (A-661's `Step::Hook`, `Due`, `Drive::resume`); "
  "`cargo test --locked -p slates-svn --lib` on Windows x86_64 (jupiter; the macOS run owed), red first (see A-655 "
  "part 2a). Owed from it: part 2b, "
  "the server's step over A-662's map (the green's version and the Subversion record in one durable entry through "
  "A-635's append, a `Moved` append judged again by position, recovery), then A-652 part 3."
)
RESEARCH_TITLE = "## 16. The commit step and the hooks' places (A-655 part 2a; §4.17.4 items 3.1, 3.3, 3.5 and 9)"
RESEARCH_LEAD = (
  "Read 2026-11-19 by g268 at §1's revision, each file fetched from "
  "`raw.githubusercontent.com/apache/subversion/d434dc14866e0c48602dba59a15443255d0b8c1e/<path>`, its hash §1's row's "
  "(`svn_props.h`, not in §1's table: `c0a7446d92934323`)."
)
RESEARCH_BULLETS = [
  "**Begin** [C: `fs-wrap.c` `svn_repos_fs_begin_txn_for_commit2` 132-184]: the transaction begun on the revision "
  "`open_root` read, with `SVN_FS_TXN_CHECK_LOCKS` alone (151-152), its name fetched for the hook; the revision "
  "properties set through `svn_repos_fs_change_txn_props` (160-164), which runs `svn_repos__validate_prop` on each "
  "(344-357) in the order `svn_prop_hash_to_array` lists APR's hash; then start-commit (166-172), whose refusal aborts "
  "the transaction. `commit.c` `open_root` (414-466) checks the revision first (428-431).",

  "**Commit** [C: `fs-wrap.c` `svn_repos_fs_commit_txn` 48-121]: pre-commit (69-71); every property named "
  "`SVN_PROP_TXN_PREFIX`, `svn:txn-` (`svn_props.h` 706), removed (72-89); `svn_fs_commit_txn`; post-commit, whose "
  "failure leaves the commit made and is reported. `commit.c` `close_edit` (799-882) refuses no transaction first "
  "(811-813).",

  "**The date** [C: `transaction.c` `write_final_revprop` 3598-3630]: `svn:date` set to "
  "`svn_time_to_cstring(apr_time_now())` unless the transaction was begun with `SVN_FS_TXN_CLIENT_DATE` and the "
  "client set one; `svn_fs_fs__begin_txn` (4195-4235) stamps a transaction with the time it began, which the commit "
  "overwrites.",

  "**Out of date** [C: `transaction.c` `commit_body` 3741-3786; `tree.c` `svn_fs_fs__commit_txn` 2194-2330]: the "
  "commit refuses a transaction whose base is not the youngest, \"Transaction out of date\" "
  "(`SVN_ERR_FS_TXN_OUT_OF_DATE`, 3784-3786), then verifies every lock again (`verify_locks`); `svn_fs_fs__commit_txn` "
  "merges against the youngest, makes it the transaction's base (2291) and, on that refusal, merges again if the "
  "youngest moved (2295-2310), returning the refusal only when it did not.",

  "**Recorded** (§14's commits): stock `svn` 1.15.0 sends `svn:txn-user-agent` and `svn:txn-client-compat-version` "
  "with every commit; no revision `svnserve` made keeps either.",
]


def design(text):
  if "**Status (2026-11-19, A-655, part 2a).**" not in text:
    lines = text.split("\n")
    starts = [i for i, l in enumerate(lines) if l.startswith("> **Status (2026-11-19, A-655, part 1).**")]
    if len(starts) != 1:
      raise SystemExit("part 1's status not found once")
    end = starts[0]
    while end + 1 < len(lines) and lines[end + 1].startswith("> "):
      end += 1
    lines[end + 1:end + 1] = [">"] + wrap(STATUS, "> ", "> ")
    text = "\n".join(lines)
  if "### A-655 part 2a " not in text:
    entry = [TITLE] + wrap(APPLIED, "", "")
    for bullet in BULLETS:
      entry += wrap(bullet, "- ", "  ")
    text = text.rstrip("\n") + "\n\n" + "\n".join(entry) + "\n"
  return text


def gaps(text):
  if "A-655 part 2a (2026-11-19" in text:
    return text
  return replace_in_paragraph(text, "> ", GAPS_START, GAPS_END, GAPS_END + GAPS_SENTENCE)


def research(text):
  if RESEARCH_TITLE in text:
    return text
  section = [RESEARCH_TITLE, ""] + wrap(RESEARCH_LEAD, "", "") + [""]
  for bullet in RESEARCH_BULLETS:
    section += wrap(bullet, "- ", "  ")
  return text.rstrip("\n") + "\n\n" + "\n".join(section) + "\n"


for path, edit in [("docs/wip/SLATES_DESIGN.md", design), ("docs/wip/GAPS.md", gaps),
                   ("docs/wip/research/vcs-servers-subversion.md", research)]:
  pathlib.Path(path).write_text(edit(main_text(path)))
print("rebuilt from origin/main with A-655 part 2a's lines")
