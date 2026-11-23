# main <- w-sizing: A-848 LANDED a953cbd64 (2026-11-21, Studio-4 debug, own target, deleted after)
- The content pool grows an arena by what a claim needs: slices cut into a run doubling from one unit (262,144 B), claim(want) best fit.
  An 18,564-byte lend (budget lacked 10,504,324 with the headroom) grew the arena 8,589,934,592 -> 16,777,216 bytes; two shards x 8 turns,
  first turn 4,294,967,296 before, every turn within the need bound after, all admitted. Red then green: `cargo test -p slates-server --lib
  content_pool::tests`; mem arena and anchor lib green. Layout version 17 (A-579 had taken 16 meanwhile; merged).
- Found, not mine: `cargo test -p slates-anchor --test anchor a_table_past_the_old_exec_limit_reaches_the_successor_whole` fails on Studio-4
  on merged main: sendmsg errno 55 (ENOBUFS) in full_table_child (anchor.rs:2002), the held channel's socket buffer. Run-request for you.
- Gotcha recorded: two worktrees sharing one CARGO_TARGET_DIR reuse each other's builds (the fingerprint follows the first tree's
  paths); `cargo clean -p` the changed crates when switching trees.
- Next: the share cap as ruled (want posted, give-back at the next turn through a publication), red first on g227's case.
