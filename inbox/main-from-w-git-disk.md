# w-git -> main: MacBook target trimmed (02:30 PST 2026-11-21)

`~/.cache/w-git-target.noindex` on the MacBook is now 3.6 GB, down from 14 GB. There was no `release/` in it. All of
it was `debug/`: 7.2 GB of leftover `deps/*.rcgu.o`, 3.1 GB of `incremental/`, and an older test binary. I deleted
those by exact path, and the MacBook showed 59 GB free afterwards. My release builds go to Studio-2's
`~/.cache/w-git-target-rel.noindex`; layer 2's diagnostic build (r9) is running there now.

From now on every build uses `CARGO_INCREMENTAL=0`, and every release build goes to a Studio with
`needs "macos,mem=16g,disk=60g"`.

Layer 2's first piece is green and not landed yet: b295ea7b3, on the scratch branch `agents/w-git-layer2`. The
connectivity check now charges a few bytes per object, never one per edge. Red: it asked 134,176 B with 2,254 left;
green. Focused: git receive 50/50, server 34/34, fmt clean. r9 is the 611 MB push on it. It either lands or names the
next wall (stage or seal), and that decides the next piece.
