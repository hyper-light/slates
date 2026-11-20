"""A-680 part 2a's docs in the part 2 worktree. Usage: a680p2a_docs.py RUN PASSED (the focused run's id and count)."""
import sys

sys.argv, (RUN, PASSED) = sys.argv[:1], sys.argv[1:3]
source = open("/Users/adalundhe/.local/state/slates-fanout/g303-tools/a680_docs.py", encoding="utf-8").read()
namespace = {"__name__": "editor"}
exec(compile(source[:source.index("STATUS = (")], "editor", "exec"), namespace)
wrap, load, save, one = namespace["wrap"], namespace["load"], namespace["save"], namespace["one"]
replace_local = namespace["replace_local"]
DESIGN, GAPS = "docs/wip/SLATES_DESIGN.md", "docs/wip/GAPS.md"

STATUS = (
  "**Status (2026-11-19, A-680 part 2a: Mercurial's site on the daemon's edge, over each repository's served view).** "
  "The daemon's HTTP edge hands each request to its tool's site (`vcs_http::ToolSites`): Mercurial's for a request "
  "whose query names a command, as hgweb tells a wire request from its pages (`wireprotoserver.handlewsgirequest`), "
  "git's for every other, so a repository of the other tool is answered as one that does not exist. Mercurial's site "
  "reads each repository through `vcs_store::hg_view::ServedViews`, the lender both edges reach a Mercurial store "
  "through: on the shard the request moved to, afresh for each step, the repository's served view (A-680 part 1) over "
  "its index and the shard's content store in A-662's map, with its register's phase roots and bookmarks (each "
  "`bookmark:` name's green's head) as they stand then, made within the request's room. Every repository publishes "
  "until its record carries 7.2's `phases.publish` (decision 3). A store the map does not hold, one an import is "
  "filling, and a view that cannot be made (counted) are `503`. The register is the record: an imported repository's "
  "phase roots and bookmarks are served once its import records them there (A-687). Proven on macOS arm64 (Mac Studio, "
  f"kiln {RUN}, 2026-11-19; the other platforms owed): `slates-server --lib` filtered to `vcs_http::`, "
  f"`vcs_store::hg_view` and the namespace's names, {PASSED} of {PASSED}, fmt clean: a two-shard daemon's edge, over "
  "TLS with a bearer token, answers `capabilities` and `heads` of a Mercurial repository on the other shard whose "
  "store took `fixtures/state`'s `states` by an import, `heads` equal to its history's heads read from the index, "
  "while a git repository asked as Mercurial asks is `404` and git's own request still git's; the lender lends nothing "
  "while an import fills a store, for a git repository or an unknown one, or past the room. Owed: the site's pushes "
  "(`unbundle` through `hg_push`, A-680 part 2b), and the stock `hg` interop run over the edge once a repository can "
  "be made from the CLI (A-685) and the edge offers hg's client Basic first (A-686)."
)
PIECE_OLD = "and A-662's stores handed to the site with the view over them (A-680 part 2)."
PIECE_NEW = (
  "and the site's pushes (`unbundle` through `hg_push`, A-680 part 2b). Built (A-680 part 2a, `vcs_http::ToolSites`, "
  "`vcs_store::hg_view`): the site on the daemon's edge, each repository's served view lent from its store on its "
  "owner shard."
)
HEADING = (
  "### A-680 part 2a — Mercurial's site on the daemon's edge: a request whose query names a command handed to it, each "
  "repository's served view lent from its store on its owner shard with its register's state (2026-11-19, g303 for UI "
  "goal 93 (kiln 51cef9f7); §4.17.3 \"The wire surface\", \"Access\" and piece 4; §4.17.2 \"Transports\" item 1; "
  "A-650 part 1, A-662, A-680 part 1, A-625 part 3)"
)
APPLIED = (
  "Applied in the same change to: §4.17.3 (a status block, piece 4's line), `crates/server` (`vcs_http`: "
  "`ToolSites`, `ToolExchange`, `Decided::map`, `hg` made crate-visible; `vcs_http::hg`: `HgStores` lending a view "
  "made within the request's room, `wire_request`, `NoStores` removed; `vcs_http::daemon_edge`: `DaemonSite` both "
  "sites; `vcs_store`: `mercurial_ref`, `hg_view` new with its tests; `repository::hg_push`: `bookmark_of`; the edge's "
  "and the site's tests), GAPS (the §4.17.3 entry)."
)
BULLETS = [
  "**Why.** The daemon's edge served git's site alone, so stock hg met `404` from a running daemon (g083's interop "
  "tests, `crates/cli/tests/cli.rs`), and Mercurial's site, tested alone, answered every repository `503` "
  "(`NoStores`) since nothing lent it a store.",
  "**Which site.** A request whose query names a command (`cmd`) is Mercurial's, as hgweb tells a wire request from "
  "its pages (7.2's `wireprotoserver.handlewsgirequest`: \"if b'cmd' not in req.qsparams: return False\"); every other "
  "is git's, whose requests never name one, so git's answers are unchanged. A repository of the other tool is answered "
  "`404` by the site asked. hgweb's pages (piece 12) name no command, so they reach git's site and are `404` there.",
  "**The lender** (`ServedViews`). Each step's view is made afresh on the shard the site moved the connection to "
  "(A-625 part 3), so nothing of the shard is held across a wait: the repository's index and the shard's content "
  "store from A-662's map, its register's phase roots and bookmarks copied and charged to the request's room, then "
  "A-680 part 1's view over them, charged to the same room; the step gets what is left of it. `HgStores::lend` takes "
  "that room and its view type is generic over the borrow (`type Store<'s>`), since the view borrows the shard's "
  "state for the step alone. A store an import is filling is not lent, so no client is served half a history; a view "
  "that cannot be made is counted (`vcs_store.hg_view.unmade`); each is `503`.",
  "**Decided.** Every repository publishes, Mercurial's default (decision 3), until its record carries "
  "`phases.publish`. A bookmark is a name `bookmark_of` reads back exactly (`bookmark_name`'s inverse) whose green "
  "names a head; a phase root of a phase or node Mercurial does not have is not read; neither is ever written by a "
  "push.",
  f"**Tests** (macOS arm64, Mac Studio, kiln {RUN}, 2026-11-19). `cargo test --locked -p slates-server --lib -- "
  f"vcs_http:: vcs_store::hg_view repository::hg_push::tests::names --test-threads=4` {PASSED} of {PASSED}; `cargo "
  "fmt --check -p slates-server` clean. `a_mercurial_repositorys_store_is_served_over_the_daemons_edge`: a two-shard "
  "daemon, a Mercurial repository on the other shard's partition holding `fixtures/state`'s `states` by an import, "
  "and a git repository; over TLS with the owner's bearer token, `capabilities` `200` naming `getbundle` and "
  "`httpheader`, `heads` `200` equal to the heads read from the index (no row names them as a parent, newest first), "
  "the git repository asked as Mercurial asks `404`, and git's own request answered by git's site. "
  "`t_a680_a_view_is_lent_from_the_store_or_not_at_all`: nothing while the import fills the store, the imported "
  "history's five changesets once it ends, nothing for a git repository or an unknown id (uncounted), and nothing past "
  "the room (counted). `bookmark_of` reads every name `bookmark_name` writes back, and no second spelling, line's "
  "name or name without its end. No integration target reaches the site yet: the stock-hg CLI tests wait on A-685 and "
  "A-686 too.",
  "**Owed.** The site's pushes (`unbundle` through `hg_push`: the push capabilities, the exchange driving prepare, "
  "admission, hooks and the landing, the reply framed as 7.2's `streamreslegacy`; A-680 part 2b); the stock `hg` "
  "round trip over the edge (g083's tests, once A-685 and A-686 land); the other platforms' runs (main's).",
]
GAPS_BUILT = (
  "**Built (A-680 part 2a):** Mercurial's site on the daemon's edge (`vcs_http::ToolSites`: a request whose query names "
  "a command is Mercurial's, as hgweb tells a wire request, every other git's), each repository's served view lent "
  "afresh for each step from its store on its owner shard with its register's phase roots and bookmarks "
  "(`vcs_store::hg_view::ServedViews`, which both edges reach a Mercurial store through; `NoStores` removed), made "
  "within the request's room, a store an import is filling not lent; every repository publishing until its record "
  f"carries `phases.publish`. `slates-server --lib` filtered {PASSED} of {PASSED} and fmt clean on macOS arm64 (Mac "
  f"Studio, kiln {RUN}, 2026-11-19): a daemon's edge answers `capabilities` and `heads` of a repository on another "
  "shard holding `fixtures/state`'s `states` by an import; the other platforms owed."
)
GAPS_OWED_OLD = "A-662's stores handed to the HTTP site with the served view over them (A-680 part 2, g303, running)"
GAPS_OWED_NEW = "the HTTP site's pushes, `unbundle` through `hg_push` (A-680 part 2b, g303)"
GAPS_PUSH_OLD = "and the site's use of the served view over them (A-680 part 2; the view is built, A-680 part 1);"
GAPS_PUSH_NEW = "and the HTTP site's pushes through them (A-680 part 2b; the served view and the site's lender are built);"


def design():
  lines = load(DESIGN)
  anchor = one(lines, lambda line: line.startswith("> **Status (2026-11-19, A-680 part 1: the served view"), "status")
  end = next(index for index in range(anchor, len(lines)) if not lines[index].startswith(">"))
  lines[end:end] = [">"] + wrap(STATUS, "> ", "> ")
  start = one(lines, lambda line: line.startswith("4. Read serving: discovery,"), "piece 4")
  stop = next(index for index in range(start + 1, len(lines)) if not lines[index].startswith("   "))
  replace_local(lines, start + 1, stop, PIECE_OLD, PIECE_NEW, "   ")
  while lines and lines[-1] == "":
    lines.pop()
  lines += [""] + [HEADING] + wrap(APPLIED, "", "")
  for bullet in BULLETS:
    lines += wrap(bullet, "- ", "  ")
  lines += [""]
  save(DESIGN, lines)


def gaps():
  lines = load(GAPS)
  built = one(lines, lambda line: line.startswith("> **Built (A-680 part 1):**"), "Built (A-680 part 1)")
  end = next(index for index in range(built + 1, len(lines)) if lines[index].startswith("> **") or
             not lines[index].startswith("> "))
  lines[end:end] = wrap(GAPS_BUILT, "> ", "> ")
  owed = one(lines, lambda line: line.startswith("> Owed, in order, after §4.17.1's seven pieces:"), "Owed")
  for old, new in [(GAPS_OWED_OLD, GAPS_OWED_NEW), (GAPS_PUSH_OLD, GAPS_PUSH_NEW)]:
    stop = next(index for index in range(owed + 1, len(lines)) if not lines[index].startswith("> "))
    replace_local(lines, owed, stop, old, new, "> ")
  save(GAPS, lines)


design()
gaps()
print("docs written")
