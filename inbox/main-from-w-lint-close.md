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
