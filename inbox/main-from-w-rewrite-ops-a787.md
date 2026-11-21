# w-rewrite-ops to main, 2026-11-20 ~16:30 PST (A-787, your 15:20 pushback)

- Status: item 1 part 1 (slates_merge::refine sliced: every piece of work metered, no unit past WORK_PER_TICK of it, no
  vector doubled or table rehashed in one step) is green locally. The release A/B against A-776 (561c655a3),
  interleaved, is running on a quiet Studio, since this Mac is at load 35-58. It lands once the numbers are in. Part 2,
  the submit driving the refinement over the shard's turns (today `compose_increment` still calls it in one go), is my
  next piece. Then items 2 and 3.
- Measured so far, this Mac under load: per save, sliced is 1.25-1.5x A-776's on files up to 4 MiB and about equal at
  12 MiB. The longest unit in quiet rounds is tens of µs on source files, 33-130 µs at 1 MiB and 75-120 µs at 4 MiB,
  against the whole save in one step before (16 ms for a 300 KiB file, 230-270 ms at 12 MiB).
- For w-lint: `cargo fmt --check -p slates-server` fails at crates/server/src/git_push/tests.rs:1932 (on main; not mine).
- Overlap: A-783 (w-engine, slates-pace's Step: yield at the quantum only when someone waits) is the primitive part 2
  should drive the refinement with. I use slates_merge::pace::Ticks (A-752) for now. Say if part 2 should wait for
  A-783 to land.
- Owned up: at ~15:45 I ran `git checkout --` with no paths in ~/slates-work/w-rewrite-ops, a reflex while restoring
  a lint line. With no paths it only printed status and changed nothing; the file was then restored by an edit. Noted;
  it won't recur.
