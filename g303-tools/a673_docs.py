"""A-673's docs: the section 4.17.3 status sentence, GAPS' open item closed and its Fixed line, the research bullet's
macOS run, and the amendment entry at the log's end."""
import sys

source = open("/Users/adalundhe/.local/state/slates-fanout/g303-tools/final_docs.py", encoding="utf-8").read()
namespace = {"sys": sys, "__name__": "editor"}
sys.argv = ["x", "0", "0", "x"]
exec(compile(source[:source.index("DESIGN, GAPS = ")], "editor", "exec"), namespace)
edit, wrap = namespace["edit"], namespace["wrap"]
DESIGN, GAPS, RESEARCH = "docs/wip/SLATES_DESIGN.md", "docs/wip/GAPS.md", "docs/wip/research/mercurial-server.md"
quote_end = lambda line: line in ("", ">") or line.startswith("> **")
gaps_end = lambda line: not line.startswith("> ") or line.startswith("> **") or line.startswith("> Owed, in order")
bullet_end = lambda line: line.startswith("- **") or line.startswith("### ") or line == ""

edit(DESIGN, "> **Status (2026-11-19, A-650 part 1: HTTP version 1 served).**", quote_end, "> ", "> ", [
  ("(zlib's, stock zlib's `compressBound`, is outgrown by the linked zlib-rs's level 1 on bytes that do not compress: "
   "GAPS, its own piece)", "(zlib's as the linked zlib-rs states it, A-673)"),
])
edit(GAPS, "> **Built (A-650 part 1):**", gaps_end, "> ", "> ", [
  ("Open, each its own piece with its failing test first (main's to start): the zlib bound an answer is charged before "
   "it is compressed is stock zlib's `compressBound`, which the linked zlib-rs's level 1 outgrows on bytes that do not "
   "compress (65,536 of the bytes its fixed codes write in nine bits came to 73,721 against 65,569; Windows x86_64, "
   "jupiter, kiln r92, 2026-11-19), so a compressor choosing zlib above level 0 could hold an answer up to about an "
   "eighth of its stream past its charge (the site's first value takes zlib at level 0, stored, within it); and a "
   "query's fields past the request's room are `400` where a path past it is `414`.",
   "Open, its own piece with its failing test first (main's to start): a query's fields past the request's room are "
   "`400` where a path past it is `414`."),
])
lines = open(GAPS, encoding="utf-8").read().split("\n")
start = next(i for i, line in enumerate(lines) if line.startswith("> **Built (A-650 part 1):**"))
end = next(i for i in range(start + 1, len(lines)) if gaps_end(lines[i]))
fixed = (
  "**Fixed (A-673, 2026-11-19, g303): an answer's room charged at the linked zlib-rs's own bound.** "
  "`compression::bound` charged zlib at stock zlib's `compressBound`, which counts stored blocks, while zlib-rs's "
  "level 1 (zlib-ng's `deflate_quick`) writes every literal in fixed codes and never falls back to one, so bytes that "
  "do not compress outgrew their charge (65,536 that its fixed codes write in nine bits came to 73,721 against 65,569). "
  "zlib's arm is now zlib-rs 0.6.8's `compress_bound`, n + ceil(n / 8) + 9, a byte more below 9 bytes and one more "
  "for none, which level 1 meets exactly at 4,095 and 4,096 bytes. Red first on main with the oracle alone "
  "(`cargo test --locked -p slates-hg --lib compression`: 2 of 18 failed, both at zlib level 1, as jupiter's r92 "
  "found), then `slates-hg --lib` 308 of 308 and fmt clean on macOS arm64 (Mac Studio, kiln r181); the other "
  "platforms owed.")
lines[end:end] = wrap(fixed, "> ", "> ")
open(GAPS, "w", encoding="utf-8").write("\n".join(lines))
print(f"{GAPS}: Fixed (A-673) inserted at line {end + 1}")

edit(RESEARCH, "- **The room a compressed answer is charged**", bullet_end, "- ", "  ", [
  ("Measured (Windows x86_64, jupiter, kiln r92, 2026-11-19: the oracle",
   "Measured (Windows x86_64, jupiter, kiln r92, and macOS arm64, Mac Studio, kiln r181, alike, 2026-11-19: the oracle"),
])

entry = [
  "### A-673 — An answer's room charged at the linked zlib-rs's own bound, where stock zlib's `compressBound` was "
  "outgrown (2026-11-19, g303 for UI goal 93 (kiln 51cef9f7); §4.17.3 \"The wire surface\"; A-650 part 1's \"Found, "
  "and left to its own piece\"; A-607)",
]
applied = (
  "Applied in the same change to: `crates/hg` (`compression::bound`'s zlib arm and its constants; the oracle "
  "`the_longest_codes_stay_within_every_engines_bound` and `arbitrary_bytes_stay_within_every_engines_bound` in "
  "`tests/compression.rs`), §4.17.3 (A-650 part 1's status sentence on the bound), GAPS (§4.17.3: the open item "
  "closed, a Fixed line), `research/mercurial-server.md` (§25's bullet on the bound: the macOS run beside jupiter's).")
bullets = [
  ("**What was wrong.** ",
   "`compression::bound`, the room an HTTP answer is charged before it is compressed, counted zlib at stock zlib's "
   "`compressBound` (`len + (len >> 12) + (len >> 14) + (len >> 25) + 13`), which counts stored blocks. The workspace "
   "links zlib-rs (A-607), whose level 1 is zlib-ng's `deflate_quick`: every literal in fixed codes, nine bits for the "
   "bytes 144 to 255 (RFC 1951 §3.2.6), and no fallback to a stored block. Bytes that do not compress outgrew their "
   "charge by up to about an eighth: 65,536 of those bytes came to 73,721 against 65,569. Nothing served met it, since "
   "the site's first value takes zlib at level 0 (stored), but the `Compressor` contract lets a caller choose any "
   "level."),
  ("**Decided.** ",
   "zlib's arm is zlib-rs 0.6.8's own `compress_bound` (`src/deflate.rs`; zlib-ng's `compressBound`): `len + ceil(len "
   "/ 8) + 3 + 6`, a byte more below 9 bytes and one more for none. It is the worst case itself, with nothing to "
   "spare: level 1 meets it exactly at 4,095 and 4,096 bytes. The constants name the backend and its version they "
   "come from, so a change of backend (A-607) is a change of the bound with it."),
  ("**Tests** ",
   "(macOS arm64, Mac Studio, kiln r181, 2026-11-19, on main b04c552b0). Red first, the oracle alone: "
   "`cargo test --locked -p slates-hg --lib compression` 16 passed and 2 failed, the table at zlib level 1 alone (64 "
   "bytes in 80 against stock zlib's 77, 4,095 in 4,616 against 4,108, 4,096 in 4,617 against 4,110, 65,536 in 73,721 "
   "against 65,569: jupiter's r92 numbers) and the generated bytes shrunk to 47 written in 61 against 60. Green, with "
   "the fix: `cargo test --locked -p slates-hg --lib` 308 of 308; `cargo fmt -p slates-hg -- --check` clean. The "
   "oracle: every engine at every level the bound is checked at (every zlib level, bzip2's ends, zstd's fastest, "
   "default and strongest) over runs that zlib's level 1 writes nine bits each, at lengths about its rounding and past "
   "4 KiB and 64 KiB, and over arbitrary bytes (generated), none past `bound`."),
  ("**Owed.** ", "The other platforms' runs (main's). The proptest's failing seed from the red run is not kept: its "
   "block has no seed file, and the table holds the same case."),
]
out = ["", *entry, *wrap(applied, "", "")]
for head, body in bullets:
  out.extend(wrap("- " + head + body, "", "  "))
text = open(DESIGN, encoding="utf-8").read()
if not text.endswith("\n"):
  sys.exit("the design doc does not end with a line end")
open(DESIGN, "w", encoding="utf-8").write(text + "\n".join(out) + "\n")
print(f"{DESIGN}: A-673's entry appended ({len(out)} lines)")
