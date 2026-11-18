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
