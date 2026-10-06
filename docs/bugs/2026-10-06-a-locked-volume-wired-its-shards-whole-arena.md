# A locked volume wired its shard's whole arena

**Found:** 2026-10-06, by the first use-level test of `volume create --locked`
(`a_locked_volume_locks_only_its_own_content_across_a_restart_or_is_refused_with_nothing_locked`, `crates/cli/tests/cli.rs`),
written to give the capability table's "owed" row a proof.

## Description

Measured on macOS 26.4, Apple M5 Max, 2 shards, one daemon (`vm_stat` wired pages before and after):

| | Wired | Time |
|---|---|---|
| before | 5,262 MiB | — |
| a 4 MiB plain create | 5,262 MiB | 78 ms reply |
| a 4 MiB `--locked` create | 21,696 MiB | the reply missed the client's 1 s deadline; the lock finished about 2.3 s after |

So a 4 MiB strict volume pinned 16,434 MiB of physical memory, stalled its shard for about 2 s, and its client was
told "no reply" for a volume that had been created (a retry got `AlreadyExists`).

Three more defects sat in the same mechanism:
- **Neighbours were pinned.** Locking was per shard, so every plain volume on a strict volume's shard was locked too.
  Under a real lock limit, a plain volume's writes would have been refused because of its neighbour's policy.
- **No admission against the limit.** The lock was the whole arena's at create, so a create either over-refused
  (the arena beyond the limit) or, once locking became per block, admitted a strict volume whose later writes failed
  as `BadRequest { reason: "Memory(LockRefused …)" }`, an uncategorized refusal (seen on Linux, `ulimit -l 512`).
- **Recovery never re-locked.** `rebuild_volume` did not lock anything, so after any daemon restart a strict volume's
  content was swappable, silently (the use-level test, mutated back to that behaviour: 0 bytes locked after the
  restart against 1,048,576 before it).

## Root cause

`ChunkArena::lock` locked whole regions, and the daemon called it for the strict volume's shard. Linux's lock is
`mlock2(MLOCK_ONFAULT)` (2026-10-03), which charges the range and wires pages only as they are touched, so the cost
was hidden there. macOS has no on-fault lock: `mlock` wires every page of the 16 GiB mapping before it returns.

## Fix

Lock what a strict volume holds, and only that (§4.2 D-12, the refinement GAP-A9-1 recorded):
- **The arena** locks blocks, not regions. The buddy keeps a per-head locked bit, `alloc_locked` and `lock_extent`
  lock a block's pages (a block is a page multiple on a page boundary, so no other block's page is touched), and
  every release path unlocks a block that was locked, including the deferred releases a recovery image holds back.
- **The content store** opens a strict volume's extents locked. A block replacing another (grow, shrink, the seal's
  move, a truncate's rebuild) inherits its lock.
- **The volume** carries the policy (`Volume::set_locked`), set by the daemon's create, clone, takeover and recovery.
- **Recovery** claims a strict volume's blocks locked (`Claims::prepare_with`). A volume whose blocks the OS will not
  lock is refused at its rebuild, never served swappable, and the shard's other volumes recover as usual.
- **Admission.** A strict volume's whole entitlement (its bound, or its dynamic maximum) is reserved against the
  process's measured lock capacity in a per-daemon ledger (`lock_ledger`). If it does not fit, the create is refused
  `BudgetExceeded` up front. The credit is released when the volume's slot drops.
- **Typing.** A block the OS still will not lock is refused `BudgetExceeded`, never `BadRequest`.
- **Reporting.** `slates status` reports each shard's locked bytes (`locked=`, and `locked_bytes` in the JSON beside
  `mapped_bytes`, which the JSON lacked).

Measured after, on the same machine and probe: wired 5,290 MiB, then 5,288 MiB after the `--locked` create (an empty
volume holds nothing); the create replies in 48 ms. With a 1 MiB file written, exactly 1,048,576 bytes are locked, and
a plain volume's 1 MiB beside it stays unlocked. On Linux with no limit the kernel's `VmLck` agrees, and with
`ulimit -l 512` the strict create is refused `BudgetExceeded { available: 524288 }` with nothing locked.

## Tests

- `crates/mem/tests/capacity.rs` `the_arena_locks_the_blocks_asked_never_the_region_or_their_neighbours`: locked bytes
  at every step, a plain neighbour never locked, a freed block unlocked; on Linux the kernel's own count moves by
  exactly as much.
- `crates/server/src/lock_ledger.rs` `credits_fit_the_capacity_and_return_it_when_dropped`.
- `crates/cli/tests/cli.rs` `a_locked_volume_locks_only_its_own_content_across_a_restart_or_is_refused_with_nothing_locked`:
  the real binary, anchor and daemon, a `SIGKILL` and restart. macOS, and Linux at two lock limits. It fails with
  recovery's re-lock removed.

## Sibling sweep

Every caller of the removed `ChunkArena::lock`: the strict create and the takeover, both converted. The arena's other
locking user is the anchor's sealing-root page (`SharedObject::lock_range`), a single page by design, unaffected.
