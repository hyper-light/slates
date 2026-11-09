# The dirty-page test took an early write-back for a lost refusal

**Seen:** B10's Linux gate on adacachy2 at load about 66 (main 3fd1de00), intermittent on main.
`a_dirty_shared_map_page_never_undoes_a_later_write_through_another_path` (`crates/cli/tests/cli.rs`) failed with "the
write-back of a page the replacement made stale is refused, and the sync says so: Ok(())".

**What it was: the test's precondition, not the product.** The test stores a byte through a shared map and leaves the page
dirty, replaces the whole file through the SDK, and expects the map's sync to be refused (A-248, "Refuse, like NFS"). The
refusal is owed only if the page is still dirty when the replacement comes. A daemon trace of every FUSE write
(temporary; adacachy2, kernel 7.3.3, load 66-68, 2026-11-09) shows the kernel writing the page back first:

| run | store | write-back reaches the daemon (page 0, `M` at byte 10, `A` elsewhere, `error=0`) | replacement |
|---|---|---|---|
| 1 | 0 ms | +5.8 ms | +37.9 ms |
| 2 | 0 ms | +4.3 ms | +52.5 ms |
| 3 | 0 ms | +3.1 ms | +41.7 ms |

The write-back carried no stale byte, so A-248 rightly applied it. The replacement then made every byte `S`, and the sync
had nothing dirty to refuse. `fuse.writeback_conflicts`, `fuse.writeback_identical`, `fuse.writebacks_unknown` and
`fuse.states_lost_for_room` stayed 0 throughout. The volume held the replacement whole every time, so no stale byte
reached it.

**The trigger.** In that traced version the test spawned `slates status` between the store and the replacement. A red
check with one `Command::new("true")` spawn there found the page written back first in 6 attempts of 6; a 50 ms sleep
there left it dirty. So a fork from the test's process while the page is dirty writes it back on this kernel. In a
gate, a sibling test's spawn landing between the store and the replacement is the likely cause; this is inferred, not
traced in a gate. The test as on main passed 10 of 10 alone at load 66-68.

**The fix (the test, and one counter).**
- `crates/bridge-fuse/src/channel.rs`, `crates/server/src/fuse.rs`: `fuse.writebacks_applied` counts cache write-backs
  applied with no stale byte.
- The test reads it just before each store and just after the other path's write. An applied write-back between them
  can only have come first, since afterwards every byte of a page fetched before the write is stale. Such an attempt
  says so and starts again with a fresh fixture, up to 6 (Derived: a miss rate under 30% at 95% confidence from 0 of 10;
  0.3^6 = 0.0007). Exhausted, the test fails naming the precondition, never the rule. Both phases that race (the
  whole-file replacement and the second mount's write) are guarded, and each asserts `fuse.writeback_conflicts` moved.

**Runs (adacachy2, load 65-77, `cargo test -p slates-cli --test cli -- --exact ... --test-threads=1`, own target dir):**
- before, the test as on main: 10 of 10 passed; instrumented with spawns in the window: 10 of 10 failed as in B10;
- after: 10 of 10 passed twice (load 66-68, then 77), no attempt missed;
- red check (a spawn in the window): 6 of 6 attempts reported the precondition not held, then the precondition failure.

## Second fix: the scenario runs in a process of its own (2026-11-09)

**What the first fix left.** In the whole CLI suite (B11's Linux gate, adacachy2, load 38-77) every attempt missed
and the test failed naming the precondition: sibling tests spawn `slates` processes all the time, and each spawn that
lands between a store and the other path's write writes the page back. More attempts cannot fix that; the spawns
are the process's, so the scenario needs a process without them.

**The edit.** `a_dirty_shared_map_page_never_undoes_a_later_write_through_another_path` re-invokes its own test binary
for itself alone (`--exact`, one test thread) with `SLATES_CLI_TEST_DIRTY_PAGE_ROLE` set, the cross-process fixture
pattern of `crates/anchor/tests/anchor.rs`. In that child the attempts run as before, each proving its precondition by
`fuse.writebacks_applied`. Each line the parent reads carries the tag `dirty-page-child: `, looked for anywhere in a
line, since libtest's `test <name> ... ` comes glued to the first. The parent requires the child's exit 0 and its
`held` line, and echoes its tagged lines.

**Runs (adacachy2, kernel 7.3.3-cachyos, `SLATES_TEST_CLI=1 cargo test -p slates-cli --test cli`, the whole suite,
own target dir):**
- red check, the scenario back in the suite's process (the role variable set on the parent), 7 runs with
  `--nocapture` at load 23-34: 20 of 27 attempts missed, each "the whole-file phase: 1 write-back(s) of the store's
  page applied before the replacement", and the scenario held only at attempts 4, 4, 4, 4, 2, 4 and 5 (one more run
  without `--nocapture`, at load 41, passed with its misses unrecorded);
- after, the scenario in its child: 13 of 13 runs passed at load 28-34, and in the 10 that echo the child's lines
  every run held at attempt 1, so 0 attempts of 10 missed.

All these runs skipped `a_start_whose_shards_sum_past_a_window_comes_up`. In the first whole-suite run its anchor
restarted the daemon (`--shards 308`) for 65 minutes without coming up, and another agent's run of the same binary
on the same box hung in the same test. Alone, it passed in 3 s. It has no deadline of its own; reported, not fixed
here.

**A sibling with the same cause.** In one of the 20 whole-suite runs,
`unsynced_map_stores_a_second_mount_and_the_sdk_agree_at_every_sync_point` failed its non-vacuity assertion ("another
path wrote into a page holding unsynced stores", `crates/cli/tests/cli.rs`): it runs in the suite's process, so a
sibling's spawn can write its dirty pages back before the other path's write. Reported, not fixed here.

