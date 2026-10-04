# mem: locking an arena stalled every shard on the memory-map lock

**Date:** 2026-10-03. **Design:** §4.2 (RAM-only, D-12: a strict volume's content in locked RAM), §4.3 (a shard
never blocks), R9. **Found by:** the Linux server tests' startup timeouts. One CI run of the Linux server tests hit four
of them. Reproduced in a Linux container (Docker Desktop on an M5 Max, 18 cores, privileged so `mlock` is unbounded)
with the library suite running beside 108 CPU burners.

## Description

A daemon's `bootstrap` observation ended `Deadline { stage: Admission, budget 10 s }`: its shard did not take the
question for the whole budget. The observation's CPU-time budget
(`docs/bugs/2026-10-03-an-observation-read-a-starved-shard-as-wedged.md`) showed the target shard had consumed **no**
CPU in those 10 s (`began == mark == now`, 103,326,163 ns), so it was blocked, not starved.

When the observation gave up, the test process read every one of its threads in `/proc/self/task`. A temporary
diagnostic in `bootstrap_refusal`, since removed, read each thread's `stat`, `syscall` and `stack`. It showed:
- one shard (`slates-shard-9`) running, in `mlock` → `__mm_populate` → `populate_vma_page_range` →
  `__get_user_pages`;
- every other shard of that daemon in state D:
  - in `mmap` (`vm_mmap_pgoff`) of `0x10f9e9a000` bytes (68 GiB, `MAP_SHARED`, a descriptor: the shard's view of the
    content object);
  - or in `munmap` (`__vm_munmap`) of the same length.

## Root cause

A strict volume (`require_locked`) locks its shard's whole content arena (`crates/server/src/verbs.rs`, the create and
the takeover; `ChunkArena::lock` → `Region::lock`). That called memmap2's `lock`, a plain `mlock(2)`. On Linux a plain
`mlock` faults every page of the range in before it returns, and holds the process's memory-map lock while it does.
Faulting a multi-GiB arena range of a shared memory object takes seconds when the thread gets a fraction of a CPU.
For that whole time every other thread of the process that maps, unmaps or changes protection waits in state D. That
includes every other shard, and every other daemon in a test process.

## Impact

- Any strict create or takeover stalled every shard of the daemon for as long as its arena took to fault in. On a
  loaded host that was past the 10 s observation budget, which read as daemon startup timeouts.
- A strict create also committed the whole arena's RAM up front, whatever the volume would use.
- Production and tests alike. No data was lost.

## Exact edits

- `crates/mem/src/region.rs`: one function, `lock_map`, locks a map for both regions (`os::lock`) and shared objects
  (`shared.rs` `Inner::lock`).
  - On Linux it is `mlock2(MLOCK_ONFAULT)` (rustix `mlock_with`). It flags the mapping locked, charges
    `RLIMIT_MEMLOCK` for the whole range exactly as before, and locks each page as it is first touched. It holds the
    memory-map lock only to flag the mapping.
  - The guarantee is unchanged: no locked page ever swaps, and an untouched page holds no content.
  - The refusal is unchanged too: a limit or a kernel without the flag (`EINVAL`, before 4.4) is refused typed.
  - macOS has no on-fault lock (`mlock` wires the range whole), and Windows keeps `VirtualLock`. Neither was measured
    stalling.
- `unsafe-budget.toml`: slates-mem 23 → 24, naming the site.

## Proof

`crates/mem/tests/lock_on_fault.rs`, on Linux with memlock unbounded:
- `locking_a_private_region_charges_it_without_faulting_it_in`;
- `locking_a_shared_object_region_charges_it_without_faulting_it_in`.

Each locks an untouched 256 MiB region. It expects the OS to report the range locked and the process's resident set
not to grow by it. With the plain `mlock`, both failed: "locking committed 268513280 bytes of an untouched 268435456-byte
region" (and 268,558,336 for the shared object). With the fix, both pass, and so does every slates-mem test.

The server library suite beside 108 burners (`slates_server` lib tests, privileged container) had ended with
`Deadline` refusals. Over six runs it now ends with none and finishes in 2.7–7.2 s. Its other failures are the
environment's, not this bug:
- Two tests present onto a source tree that root in the container does not own: "the target is owned by another
  user".
- In alternate runs, the test fixture's quick machine profile (5 ms per probe) cannot measure the wake under 6×
  oversubscription (`MeasurementTimeout { probe: "wake" }`).

## Sibling sweep

Every `mlock`, `lock()` and `VirtualLock` site in `crates/`:
- `region.rs` (anonymous maps, fixed);
- `shared.rs` Unix (the shared object, fixed);
- `shared.rs` Windows (`VirtualLock`, commits by design);
- `lock.rs` (`lock_in_order` calls `Region::lock`, fixed through it).

No other code locks memory. Other calls that take the memory-map lock on a shard still run while a daemon starts:
- the per-shard content view's `mmap` and `munmap` (one each per shard, at start and stop);
- the system allocator's arena growth (847 `mprotect` calls per daemon start, counted on Linux).

Neither faults pages in under the lock. A shard-local allocator that removes the second was measured and is not landed
(BENCHMARKS.md, 2026-10-03).
