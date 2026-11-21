# main <- w-lint: closed items (2026-11-20)

- fetch_update deprecation (kiln's macos-arm64 / ubuntu-arm clippy on vendor/slates): CLOSED in slates by fe8bc4793
  (on origin/main 450f1a83d). All six Atomic*::fetch_update calls are try_update (same Result<T, T> call; stable on the
  pinned 1.98.1). Test: clippy -D warnings on 1.98.1 (workspace) and on 1.99.1 (machine, mem): no deprecation; tests
  mem arena 16, machine log 4, server lock_ledger 1, vcs_share 10. Kiln's vendored copy follows on its next vendor.
- fmt on main: git_push/tests.rs:1932 was already formatted on main when I merged; the remaining fmt diff
  (server tests/fleet.rs) is in fe8bc4793. Test: cargo fmt --check clean at 450f1a83d.
- g226 / 599603559 run requests:
  - `cargo xtask unsafe`: CLOSED. slates-mem 83/83 ok; the three heap.rs sites each have a SAFETY line.
  - Cross-target clippy matrix (six ci.yml triples): CLOSED, clean at ebd9014c3 and again at 15d4602ea; mallinfo2
    compiles on both gnu triples, the None fallback on musl and Windows.
  - Native Linux: clippy on mem/ipc/anchor clean (r40 at a7df9cc). The Linux heap test itself has not run: every Linux
    run since was refused "no room" or lost to a runner restart (r55, r56, r57). Open.
  - Native Windows: r54 on jupiter (ebd9014c3) still running. Open.
- The wall: host clippy --workspace --all-targets, six cross targets, structural, literals, unsafe, fmt clean at
  450f1a83d (rounds 8-9 and between, commits in progress/w-lint.md).

## 2026-11-20 19:15: refine.rs's lint-round-10 regression (GAPS D-O17 (f)) -- CLOSED for the whole path, sliced 1.05x left
- Commit 9f77462e4 on origin/main: the byte-class table back as a literal `[Class; 256]` read through
  `.get(usize::from(byte))`, with a test equal to classify() for every byte.
- A/B (release, interleaved, six passes, arm order rotated, the same examples/rewrite_slices.rs in all three trees,
  Adas-Mac-Studio-2 load 3.7-4.9), spread of the six best-of-fives:
  - whole: 12 MiB pre-round-10 (d4a5f9f43) 186.9-190.4 ms, today (b45a114e3) 241.4-244.5, fixed 184.4-190.5;
    1 MiB 16.0-16.3 / 20.2-20.7 / 15.7-16.0.
  - sliced: 12 MiB 186.6-194.1 / 256.8-261.1 / 199.4-203.5; 1 MiB 15.5-16.5 / 21.5-22.2 / 16.6-17.2.
- The sliced path keeps ~1.05x (GAPS D-O17 (f), not chased).
