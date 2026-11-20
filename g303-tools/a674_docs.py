"""A-674's docs: GAPS' open item closed with a Fixed line, and the amendment entry at the log's end.
Usage: a674_docs.py RUN HG_SITE_COUNT HG_HTTP_COUNT RED_DETAIL"""
import sys

RUN, SITE, HGHTTP, RED = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
source = open("/Users/adalundhe/.local/state/slates-fanout/g303-tools/final_docs.py", encoding="utf-8").read()
namespace = {"sys": sys, "__name__": "editor"}
saved = sys.argv
sys.argv = ["x", "0", "0", "x"]
exec(compile(source[:source.index("DESIGN, GAPS = ")], "editor", "exec"), namespace)
sys.argv = saved
edit, wrap = namespace["edit"], namespace["wrap"]
DESIGN, GAPS = "docs/wip/SLATES_DESIGN.md", "docs/wip/GAPS.md"
gaps_end = lambda line: not line.startswith("> ") or line.startswith("> **") or line.startswith("> Owed, in order")

edit(GAPS, "> **Built (A-650 part 1):**", gaps_end, "> ", "> ", [
  (" Open, its own piece with its failing test first (main's to start): a query's fields past the request's room are "
   "`400` where a path past it is `414`.", ""),
])
lines = open(GAPS, encoding="utf-8").read().split("\n")
fixed_673 = next(i for i, line in enumerate(lines) if line.startswith("> **Fixed (A-673, 2026-11-19, g303)"))
end = next(i for i in range(fixed_673 + 1, len(lines)) if gaps_end(lines[i]))
fixed = (
  "**Fixed (A-674, 2026-11-19, g303): a query past the request's room answered `414`.** The hg site answered `400` "
  "for a query whose fields were past the request's room, where a path past it was `414` and git's routes answer a "
  "target past theirs `414` (RFC 9110 §15.5.15); `route` now tells the query codec's `PastAdmission` from its other "
  "refusals (`query_refused`, every refusal named, so one the codec adds is judged before it compiles). Red first: "
  f"{RED}; then `slates-server --lib vcs_http::hg` {SITE} of {SITE}, `slates-hg --lib http` {HGHTTP} of {HGHTTP} and "
  f"fmt clean on macOS arm64 (Mac Studio, kiln {RUN}); the other platforms owed.")
lines[end:end] = wrap(fixed, "> ", "> ")
open(GAPS, "w", encoding="utf-8").write("\n".join(lines))
print(f"{GAPS}: Fixed (A-674) inserted at line {end + 1}")

entry = ("### A-674 — A query past the request's room answered `414`, as a path past it and git's routes answer a target "
         "past theirs (2026-11-19, g303 for UI goal 93 (kiln 51cef9f7); §4.17.3 \"Access\"; A-650 part 1's "
         "\"Decided\"; RFC 9110 §15.5.15)")
applied = ("Applied in the same change to: `crates/server` (`vcs_http::hg`: `route` and `query_refused`, the module doc's "
           "order; the room test's query case), GAPS (§4.17.3: the open item closed, a Fixed line).")
bullets = [
  "- **What was wrong.** A-650 part 1's site answered `414` for a path past the request's room but `400` for a query "
  "whose fields were past it: `route` mapped every refusal of the query's codec (`slates_hg::serve::http::command`) to "
  "`400`, `PastAdmission` among them. A client could not tell a target too long to read from a malformed one, where "
  "git's routes answer both lengths `414`.",
  "- **Decided.** A query whose fields are past the request's room is `414` (RFC 9110 §15.5.15: a target \"longer than "
  "the server is willing to interpret\"), as a path past it is; the codec's other refusals stay `400`, each named in "
  "`query_refused`, so a refusal the codec gains is judged there before the site compiles.",
  f"- **Tests** (macOS arm64, Mac Studio, kiln {RUN}, 2026-11-19, over main 1c6ac5006). Red first, the room test's "
  f"query case alone (the site's `hg.rs` as main has it): {RED}. Green: `cargo test --locked -p slates-server --lib "
  f"vcs_http::hg` {SITE} of {SITE}; `cargo test --locked -p slates-hg --lib http` {HGHTTP} of {HGHTTP}; `cargo fmt -p "
  "slates-server -p slates-hg -- --check` clean. The test: the owner's `heads` to a site admitting 256 bytes a request, "
  "by a path past them, by a query whose fields are past them, and with an `X-HgArg` header past them, then of a "
  "repository whose policy holds a rule only git reads: `414`, `414`, `431`, `500`.",
  "- **Owed.** The other platforms' runs (main's).",
]
out = ["", entry, *wrap(applied, "", "")]
for bullet in bullets:
  out.extend(wrap(bullet[2:], "- ", "  "))
text = open(DESIGN, encoding="utf-8").read()
if not text.endswith("\n"):
  sys.exit("the design doc does not end with a line end")
open(DESIGN, "w", encoding="utf-8").write(text + "\n".join(out) + "\n")
print(f"{DESIGN}: A-674's entry appended ({len(out)} lines)")
