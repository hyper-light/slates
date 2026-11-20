"""A-680 part 1's docs in the landing worktree: the design's status block, decision 4, piece 4's line and the
amendment at the log's end; GAPS' section 4.17.3 entry; research section 26. Run from the worktree's root."""
import sys

DESIGN, GAPS, RESEARCH = "docs/wip/SLATES_DESIGN.md", "docs/wip/GAPS.md", "docs/wip/research/mercurial-server.md"
WIDTH = 120


def wrap(text, first, rest, width=WIDTH):
  out, current = [], first
  for word in text.split():
    joined = current + ("" if current in (first, rest) else " ") + word
    if len(joined) > width and current not in (first, rest):
      out.append(current)
      current = rest + word
    else:
      current = joined
  out.append(current)
  return out


def load(path):
  return open(path, encoding="utf-8").read().split("\n")


def save(path, lines):
  open(path, "w", encoding="utf-8").write("\n".join(lines))


def one(lines, predicate, what):
  found = [index for index, line in enumerate(lines) if predicate(line)]
  if len(found) != 1:
    sys.exit(f"{what}: {len(found)} matches")
  return found[0]


def replace_span(lines, start, end, pairs, first, rest):
  """Joins lines[start:end] (each stripped of its prefix), replaces each `old` of `pairs` by its `new` once, and
  re-wraps the span."""
  words = []
  for index in range(start, end):
    lead = first if index == start else rest
    if not lines[index].startswith(lead):
      sys.exit(f"line {index + 1} lacks {lead!r}")
    words.append(lines[index][len(lead):])
  text = " ".join(words)
  for old, new in pairs:
    if text.count(old) != 1:
      sys.exit(f"{old[:60]!r}: {text.count(old)} matches")
    text = text.replace(old, new)
  lines[start:end] = wrap(text, first, rest)


def replace_local(lines, start, end, old, new, prefix):
  """Replaces `old` by `new` once within lines[start:end] (each beginning with `prefix`), re-wrapping only the lines
  the phrase spans, so the rest of the paragraph keeps its breaks."""
  bodies = [line[len(prefix):] for line in lines[start:end]]
  if any(not line.startswith(prefix) for line in lines[start:end]):
    sys.exit(f"a line of {start + 1}-{end} lacks {prefix!r}")
  text, spans = "", []
  for body in bodies:
    if text:
      text += " "
    spans.append((len(text), len(text) + len(body)))
    text += body
  if text.count(old) != 1:
    sys.exit(f"{old[:60]!r}: {text.count(old)} matches")
  at = text.index(old)
  first = next(index for index, (low, high) in enumerate(spans) if low <= at <= high)
  last = next(index for index, (low, high) in enumerate(spans) if low <= at + len(old) - 1 <= high)
  joined = " ".join(bodies[first:last + 1]).replace(old, new)
  lines[start + first:start + last + 1] = wrap(joined, prefix, prefix)


STATUS = (
  "**Status (2026-11-19, A-680 part 1: the served view over a repository's index).** `serve::indexed::Indexed` is the "
  "`Repository` and `Store` the serving answers read, made of what one repository holds at one position of its "
  "register: its index's changelog rows (A-588 part 2), the content store its texts and markers live in (A-581; the "
  "index's `M` rows, A-649), and its register's state (its phase roots, each bookmark's node, and whether it "
  "publishes; A-642, A-657). It serves what 7.2's `served` filter serves, the view `hg serve` answers every peer from: "
  "a changeset is left out when it is hidden or its phase is secret, archived or internal; it is hidden when it is "
  "obsolete (a draft or secret changeset a marker names as its predecessor, a prune among them) or archived or "
  "internal, no bookmark pins it (a repository here has no working copy, local tags or merge state, so its bookmarks "
  "are its only pins), and no visible changeset that is not public descends from it (decision 4). The view numbers "
  "what it serves densely, oldest first, which the wire never shows; it walks only from the first changeset that is "
  "not public (every one below the lowest phase root is public, served and numbered as the changelog numbers it), "
  "forward once for phases and back once for what a visible descendant reveals, each list it keeps charged to the "
  "request's room before it is made; a phase root whose node the changelog lacks is dropped and counted, as 7.2's "
  "`_readroots` drops it, and a marker or a changelog row it cannot read refuses the view. Its texts are lent in place "
  "where the store holds one as one window, and copied otherwise, the copy charged to the request's room first; a file "
  "revision with a metadata block is put back together as 7.2 stores it. `getbundle::Store` now lends every text that "
  "way, and its `file_node` is gone: a changegroup lends each changeset's text once and each manifest's once, and finds "
  "a path's file node in its manifest's text by halving (`manifest::find`, as 7.2's `_lazymanifest.bsearch` halves), "
  "so no manifest is read again for a path. Proven on macOS arm64 (Mac Studio, kiln r256 over main b94ae6e90, "
  "2026-11-19; the other platforms owed): `slates-hg --lib serve:: manifest::` 61 of 61, `slates-server --lib "
  "vcs_http::hg` 7 of 7, fmt clean; the view's tests failed first, 6 of 9, against a stub that left nothing out and "
  "lent nothing in place. They hold `fixtures/state`'s two repositories to what stock 7.2's `served` filter serves of "
  "them (`make-served-fixtures.sh`: each changeset served with its phase, each left out with why, and `heads`, "
  "`branchmap` and `listkeys` of `bookmarks` and `phases` byte for byte), a clone over the view to exactly its "
  "changesets with every text lent once in place, texts past a window to copies within the room, generated histories "
  "to 7.2's filter stated literally, and pins, reveals, a dropped root, an unreadable marker and every room up to the "
  "least to their answers. Owed: the view handed to the daemon's sites over A-662's stores (A-680 part 2)."
)

DECISION_OLD = "An obsolete changeset leaves the served view at once;"
DECISION_NEW = (
  "An obsolete changeset leaves the served view at once, unless a bookmark pins it or a visible changeset that is "
  "not public descends from it, as 7.2's `served` filter keeps it [C: `repoview.computehidden`] (A-680);"
)

PIECE_OLD = (
  "served by the daemon's `vcs_http::hg` site); owed: the process around the stdio session, the learned response "
  "compression (the first value until then), and A-662's stores handed to the site."
)
PIECE_NEW = (
  "served by the daemon's `vcs_http::hg` site), and the served view over a repository's index, its content store and "
  "its register's state, held to 7.2's `served` filter (A-680 part 1, `serve::indexed`); owed: the process around the "
  "stdio session, the learned response compression (the first value until then), and A-662's stores handed to the "
  "site with the view over them (A-680 part 2)."
)

HEADING = (
  "### A-680 part 1 — A repository's served view over its index: what 7.2's `served` filter serves, numbered densely, "
  "walked from the first changeset that is not public, its texts lent in place or copied within the request's room "
  "(2026-11-19, g303 for UI goal 93 (kiln 51cef9f7); §4.17.3 decision 4, \"Pull and clone\" items 1 and 2, "
  "\"Access\" and piece 4; A-588 part 2, A-642, A-649, A-657, A-662)"
)
APPLIED = (
  "Applied in the same change to: §4.17.3 (a status block; decision 4, an obsolete changeset kept as 7.2's served "
  "filter keeps it; piece 4's line), `crates/hg` (`serve::indexed`, new, with its tests; `serve::getbundle`'s `Store`, "
  "every text lent with the request's room and `file_node` gone, the changegroup lending each text once; "
  "`manifest::find`; `fixtures/state/make-served-fixtures.sh` and its twelve captures, their sums in `SHA256SUMS`; the "
  "fixtures' list), `crates/server` (`vcs_http::hg`'s `NoStore` and its tests' store on the new `Store`), GAPS (the "
  "§4.17.3 entry), `research/mercurial-server.md` (§26)."
)
BULLETS = [
  "**Why.** Every serving answer (discovery, `listkeys` and `getbundle`, A-619; HTTP, A-650 part 1; the session over "
  "a byte channel, A-650 part 2) reads a `Repository` and a `Store`, and nothing made them from what a repository "
  "holds: its index (A-588 part 2), the shard's content store (A-581) and its register's state (A-642, A-657). The "
  "daemon's sites answer `503` until they are handed one (A-680 part 2, over A-662's stores).",
  "**The filter is 7.2's `served`, every rule of it** (read from 7.2's source, research §26). `computeunserved` joins "
  "the changesets `visible` hides with those `served.hidden` hides (secret, archived, internal). Hidden is "
  "`hideablerevs` (obsolete: a draft or secret changeset whose node is a key of `obsstore.successors`, which keys "
  "every marker by its predecessor, a prune's too; or archived, or internal) less `pinnedrevs`, then every hidden "
  "ancestor of a visible changeset that is not public revealed (`_revealancestors`). A repository here has no working "
  "copy, local tags or merge state, so its bookmarks are its pins, every one the register holds. Decision 4's \"leaves "
  "the served view at once\" now says so: an obsolete changeset a bookmark pins, or one a visible changeset that is "
  "not public descends from, stays served as in 7.2, and the view stays closed under ancestors, which its dense "
  "numbering needs.",
  "**Numbered densely, walked from the first changeset that is not public.** The view numbers what it serves from 0, "
  "oldest first; a served changeset's parents are served (a secret parent would make it secret, a public one has only "
  "public parents, and any other reveals its hidden parents), so each parent stays before its child. Below the lowest "
  "phase root every changeset is public, served and numbered as the changelog numbers it (7.2's `computeimpactable` "
  "starts there for the same reason). The filter walks forward once from there for each changeset's phase (the "
  "highest of its parents' and of a root at itself) and back once for what a visible descendant reveals, and keeps "
  "only the served changesets from there, with their phases, and those left out, with why, each list charged to the "
  "request's room before it is made. A changeset's node, parents, branch and text are read from its row when asked. "
  "7.2 keeps the changelog's numbers in a filtered view; the wire carries nodes, never numbers.",
  "**Texts lent.** `getbundle::Store`'s texts were slices of what the store held, which an index over a content store "
  "cannot lend for an object past one window. Each is now lent in place where the store holds it as one window, and "
  "copied otherwise, its length charged to the request's room first; a file revision with a metadata block is put "
  "back together as 7.2 stores it (the block between its markers, then its bytes). `file_node` is gone: the "
  "changegroup lends each changeset's text once into its entry and each manifest's once, checked whole, and finds each "
  "path's file node by halving its text (`manifest::find`: the line holding the middle byte of what is left says which "
  "half holds the path, as 7.2's `_lazymanifest.bsearch` halves, with nothing built). `Indexed::lends` counts which "
  "way each text went, the fast path's non-vacuity counter.",
  "**Refused, and dropped.** A phase root whose node the changelog lacks is dropped and counted (7.2's `_readroots`); "
  "a changelog row the walk cannot read, a parent at or after its child, and a marker the store cannot read refuse "
  "the view (7.2 aborts on an obsstore it cannot read). The view's `Census` counts the hidden, the secret, the pinned, "
  "the revealed and the dropped roots.",
  "**Tests** (macOS arm64, Mac Studio, 2026-11-19). Red first at the branch's 8c949e4ff, the filter and in-place "
  "lending stubbed out: `cargo test --locked -p slates-hg --lib serve::indexed` 3 passed, 6 failed. Green over main "
  "b94ae6e90 (kiln r256): `cargo test --locked -p slates-hg --lib -- serve:: manifest::` 61 of 61; `cargo test "
  "--locked -p slates-server --lib vcs_http::hg -- --test-threads=4` 7 of 7; `cargo fmt --check -p slates-hg -p "
  "slates-server` clean. The golden: `fixtures/state`'s two repositories (bookmarks in `.hg/bookmarks`, and in the "
  "store) imported and viewed at their imported state, held to stock 7.2's `served` filter over the same stores "
  "(`make-served-fixtures.sh`, run on the Mac Studio): `c3daaa7` public and `baa2576` and `b252f2a` draft served, "
  "`a1286ad` (amended into `b252f2a`) hidden and `4ed9423` secret left out, and `heads`, `branchmap` and `listkeys` of "
  "`bookmarks` (`one` and `two`; the secret changeset's `secret-one` absent) and `phases` byte for byte. The oracle: "
  "7.2's filter stated literally over generated histories of one to nineteen changesets (merges, roots of every "
  "phase, prunes and rewrites, bookmarks, and now and then a root, a prune and a bookmark of a node never held), each "
  "changeset's fate, the dense numbering and the census equal. Beside them: a clone over the view (exactly the three "
  "changesets, their manifests and files, every text hashing to its node, each lent once in place), texts past a "
  "4,096-byte window copied exactly within the room, pins and reveals case by case, a dropped root, an all-public and "
  "an empty history, an unreadable marker, and every room from nothing to the least that holds the view. "
  "`manifest::find` is held to a walk of every line over generated manifests, and never panics on arbitrary bytes.",
  "**Owed.** The view handed to the daemon's HTTP site over A-662's stores, and the site's pushes through `hg_push` "
  "(A-680 part 2); the other platforms' runs (main's).",
]

GAPS_HEADER_OLD = "the session over a byte channel (A-650 part 2) and HTTP version 1 (A-650 part 1);"
GAPS_HEADER_NEW = (
  "the session over a byte channel (A-650 part 2), HTTP version 1 (A-650 part 1) and the served view over a "
  "repository's index (A-680 part 1);"
)
GAPS_OWNERS_OLD = "g303 for A-650)."
GAPS_OWNERS_NEW = "g303 for A-650 and A-680)."
GAPS_BUILT = (
  "**Built (A-680 part 1):** the served view over a repository's index (`serve::indexed`): its changelog rows, the "
  "content store and its register's state (phase roots, bookmarks, publishing) served as 7.2's `served` filter serves "
  "them (hidden: obsolete, archived or internal, pinned by no bookmark, and revealed by no visible descendant that is "
  "not public; then secret, archived and internal left out), numbered densely, walked from the first changeset that is "
  "not public, each list charged to the request's room; texts lent in place where the store holds one window, else "
  "copied within the room (`getbundle::Store` lends that way, its `file_node` gone: a path's file node found in its "
  "manifest's text by halving, `manifest::find`); a root the changelog lacks dropped and counted. `slates-hg --lib "
  "serve:: manifest::` 61 of 61 (red first, 6 of the view's 9 failing against a stub), `slates-server --lib "
  "vcs_http::hg` 7 of 7 and fmt clean on macOS arm64 (Mac Studio, kiln r256, 2026-11-19), the two `fixtures/state` "
  "repositories held to stock 7.2's `served` answers; the other platforms owed."
)
GAPS_OWED_SITE_OLD = "A-662's stores handed to the HTTP site (g303, running)"
GAPS_OWED_SITE_NEW = "A-662's stores handed to the HTTP site with the served view over them (A-680 part 2, g303, running)"
GAPS_OWED_PUSH_OLD = "and the served view over them (with A-650's site);"
GAPS_OWED_PUSH_NEW = "and the site's use of the served view over them (A-680 part 2; the view is built, A-680 part 1);"

RESEARCH_SECTION = [
  "## 26. Found while building the served view (A-680 part 1, 2026-11-19; 7.2's code [C] in the wheel `uv` installs "
  "on the Mac Studio, `mercurial==7.2`, CPython 3.12; stock 7.2's `served` filter over `fixtures/state`'s two "
  "repositories, `fixtures/state/make-served-fixtures.sh`)",
  "",
  "- **`served` is two filters joined** (`repoview.computeunserved`): the revisions `visible` hides and those "
  "`served.hidden` hides (`computesecret`: the `remotehiddenphases`, secret 2, archived 32 and internal 96). `hg serve` "
  "answers every peer from it (`wireprotov1server.getdispatchrepo`, `server.view` at its default).",
  "- **`visible`** (`computehidden`): `hideablerevs` (the obsolete revisions and those of the `localhiddenphases`, "
  "internal and archived) less `pinnedrevs` (the working directory's parents, every bookmark's changeset, the local "
  "tags' and, while files are unresolved, an active merge's two sides); then `_revealancestors` from every revision of "
  "a `mutablephases` phase (draft, secret, archived, internal) that is not hidden: each hidden parent leaves the hidden "
  "set and is walked in turn, on a stack, so a chain of hidden ancestors is revealed through hidden ones. A public "
  "revision reveals nothing (it is not mutable) and needs nothing revealed (its ancestors are public, never "
  "hideable).",
  "- **Obsolete** (`obsolete._computeobsoleteset`): a revision of the `relevant_mutable_phases`, draft and secret, whose "
  "node is a key of `obsstore.successors`, which keys every marker by its predecessor, a prune's too. An archived or "
  "internal revision is hideable by its phase alone.",
  "- **Phase roots** (`phases._readroots`): a root whose node the changelog lacks is dropped (`removing unknown node %s "
  "from %i-phase boundary`) and the roots marked dirty, never refused.",
  "- **The captures.** Each repository made again from its committed store, `.hg/requires` and `.hg/bookmarks` (its "
  "nodes the fixtures' own) and opened with evolution on and publishing off: `served` keeps `c3daaa7` (public), "
  "`baa2576` and `b252f2a` (draft), and leaves out `a1286ad` (hidden: amended into `b252f2a`, no bookmark on it, no "
  "visible descendant) and `4ed9423` (secret); `heads` and `branchmap` answer `b252f2a` alone; `listkeys bookmarks` "
  "answers `one` and `two`, never `secret-one`, whose changeset is secret; `listkeys phases` answers the one draft "
  "root, `baa2576`. Both repositories (bookmarks in `.hg/bookmarks`, and in the store) answer alike.",
]


def design():
  lines = load(DESIGN)
  anchor = one(lines, lambda line: line.startswith("> **Status (2026-11-19, A-650 part 1: HTTP version 1 served).**"),
               "A-650 part 1's status")
  end = next(index for index in range(anchor, len(lines)) if not lines[index].startswith(">"))
  if lines[end] != "" or not lines[end + 1].startswith("**Role.**"):
    sys.exit("the status blocks do not end before **Role.**")
  lines[end:end] = [">"] + wrap(STATUS, "> ", "> ")
  start = one(lines, lambda line: line.startswith("   An obsolete changeset leaves the served view at once;"),
              "decision 4's sentence")
  stop = next(index for index in range(start + 1, len(lines)) if not lines[index].startswith("   "))
  replace_span(lines, start, stop, [(DECISION_OLD, DECISION_NEW)], "   ", "   ")
  start = one(lines, lambda line: line.startswith("   (A-650 part 1, `serve::http`, served by the daemon's"),
              "piece 4's A-650 part 1 line")
  stop = next(index for index in range(start + 1, len(lines)) if not lines[index].startswith("   "))
  replace_span(lines, start, stop, [(PIECE_OLD, PIECE_NEW)], "   ", "   ")
  while lines and lines[-1] == "":
    lines.pop()
  lines += [""] + [HEADING] + wrap(APPLIED, "", "")
  for bullet in BULLETS:
    lines += wrap(bullet, "- ", "  ")
  lines += [""]
  save(DESIGN, lines)
  print(f"{DESIGN}: status at {end + 2}, decision 4, piece 4, amendment appended")


def gaps():
  lines = load(GAPS)
  start = one(lines, lambda line: line.startswith("> **§4.17.3 the Mercurial server: designed (A-499);"), "4.17.3 head")
  stop = next(index for index in range(start, len(lines)) if lines[index].endswith("for A-650).**"))
  replace_span(lines, start, stop + 1, [(GAPS_HEADER_OLD, GAPS_HEADER_NEW), (GAPS_OWNERS_OLD, GAPS_OWNERS_NEW)],
               "> ", "> ")
  built = one(lines, lambda line: line.startswith("> **Built (A-678):**"), "Built (A-678)")
  end = next(index for index in range(built + 1, len(lines)) if lines[index].startswith("> **") or
             not lines[index].startswith("> "))
  lines[end:end] = wrap(GAPS_BUILT, "> ", "> ")
  owed = one(lines, lambda line: line.startswith("> Owed, in order, after §4.17.1's seven pieces:"), "Owed")
  for old, new in [(GAPS_OWED_SITE_OLD, GAPS_OWED_SITE_NEW), (GAPS_OWED_PUSH_OLD, GAPS_OWED_PUSH_NEW)]:
    stop = next(index for index in range(owed + 1, len(lines)) if not lines[index].startswith("> "))
    replace_local(lines, owed, stop, old, new, "> ")
  save(GAPS, lines)
  print(f"{GAPS}: header, Built (A-680 part 1) at {end + 1}, Owed")


def research():
  lines = load(RESEARCH)
  if any(line.startswith("## 26.") for line in lines):
    sys.exit("research already has a section 26")
  while lines and lines[-1] == "":
    lines.pop()
  lines.append("")
  for block in RESEARCH_SECTION:
    if block == "":
      lines.append("")
    elif block.startswith("## "):
      lines.append(block)
    else:
      lines += wrap(block, "- ", "  ") if not block.startswith("- ") else wrap(block[2:], "- ", "  ")
  lines.append("")
  save(RESEARCH, lines)
  print(f"{RESEARCH}: section 26 appended")


design()
gaps()
research()
