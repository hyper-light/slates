"""Replace each {{RUN-...}} token by its fill, each exactly once; print the line each fill starts on."""
import sys

FILLS = {
  "docs/wip/SLATES_DESIGN.md": {
    "{{RUN-DESIGN}}": "`slates-hg --lib` 293 of 293, `slates-http-edge --lib` 32 of 32 and `slates-server --lib vcs_http` 36 of 36 on macOS arm64 (Mac Studio, kiln r6, r33 and r48, 2026-11-19; the other platforms owed)",
    "{{RUN-AMENDMENT}}": "macOS arm64, Mac Studio, 2026-11-19: `cargo test --locked -p slates-hg --lib` 293 of 293, kiln r33; `-p slates-http-edge --lib` 32 of 32, r6; `-p slates-server --lib vcs_http` 36 of 36 on the merged tree, r48: git's site 10, Mercurial's 6, the edge's serving 11 and the waits 9; `cargo fmt --check` clean on the three crates. The first run, r6, failed the golden alone: its helper inflated 7.2's zstd frames, which name a 2 MiB window and no content size (2,490,432 bytes charged for the decoder), in the 1 MiB request room, and now has `INFLATE_ROOM`. No integration target reaches `vcs_http`, private to its crate, or `slates-hg`. The other platforms owed",
  },
  "docs/wip/GAPS.md": {
    "{{RUN-EDGE}}": "`slates-http-edge --lib` 32 of 32 and `slates-server --lib vcs_http` 36 of 36, the late fields over a real loopback connection among them, on macOS arm64 (Mac Studio, kiln r6 and r48, 2026-11-19); the other platforms owed.",
    "{{RUN-GAPS}}": "`slates-hg --lib` 293 of 293, the 13 captured exchanges among them, and `slates-server --lib vcs_http` 36 of 36 on macOS arm64 (Mac Studio, kiln r33 and r48, 2026-11-19); the other platforms owed.",
  },
}

for path, fills in FILLS.items():
  with open(path, encoding="utf-8") as handle:
    text = handle.read()
  for token, fill in fills.items():
    count = text.count(token)
    if count != 1:
      sys.exit(f"{path}: {token} appears {count} times")
    text = text.replace(token, fill)
    line = text[: text.index(fill)].count("\n") + 1
    print(f"{path}: {token} filled at line {line}")
  with open(path, "w", encoding="utf-8") as handle:
    handle.write(text)
