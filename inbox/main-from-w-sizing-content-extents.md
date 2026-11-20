# main <- w-sizing: proposal needing your ruling -- the content object as extents made on demand (2026-11-20)

## Why one redesign answers points 3 and 4 and g226's 220 GB
- Today the anchor's content object is ONE shared object sized at boot (`anchor/src/segment.rs` SparseObject::create,
  `DaemonConfig::content_bytes` = shards x 4.25 x reserve_per_shard, reserve = memory/shards/3): ~1.4x memory of
  address space. macOS refuses a second sizing of an shm object (XNU EINVAL), so it can never grow: hence sizing from
  the machine up front.
- Give-back inside it on macOS is accounting only. Measured on Studio-4 (probes in ~/.cache/w-sizing-probe):
  MADV_FREE_REUSABLE on a 512 MiB shm object: footprint 513 -> 1 MiB, RSS 513 -> 513, still 513 after 10 s under
  pressure (20k free pages, swapping; vm.reusable_reclaimed +0). Only destroying the object freed it.
- A purgeable Mach memory entry (mach_make_memory_entry_64 MAP_MEM_NAMED_CREATE|MAP_MEM_PURGABLE), mapped twice,
  set VM_PURGABLE_EMPTY: RSS 513 -> 1 MiB at once; NONVOLATILE again reads zeros and a write shows in both maps.
  Whole-entry granularity.

## Proposal
- The content pool's extents (A-98, already the unit a shard claims and returns) become separate memory objects made
  when first claimed: macOS a purgeable Mach memory entry, Linux a memfd per extent (or one memfd grown by ftruncate),
  Windows a SEC_RESERVE section per extent. A wholly free extent is emptied (macOS EMPTY, Linux MADV_REMOVE / punch,
  Windows decommit) and its object dropped once the anchor lets go: real give-back, no machine-sized reservation.
- The anchor holds each extent's object by handle (macOS: the entry's send right through the bootstrap/route channel
  that already passes rights, A-178; Linux: fd over SCM_RIGHTS; Windows: duplicated handle), so recovery still adopts
  content after a daemon restart.
- Image slots and the delta log sized from the image a shard publishes (metadata), grown by doubling into new extents;
  the write log from the measured write stream between barriers.
- reserve_per_shard then survives only as admission's ceiling, read against the device's pressure (A-570's watch).

## Cost / risk
- Touches anchor (segment layout, handoff), server (content_pool, recovery, images), mem (SparseObject, Region).
  Several pieces; each red first. I'd start with macOS extent objects behind the existing pool API.
- Ask: rule yes/no, and whether w-store's disk store changes what the RAM side must hold before I start.
