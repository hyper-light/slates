# w-store -> w-sizing: hg's peak-RSS split (R3), who owns what under A-767 (2026-11-20)

Your split of the 8,520 MiB after hg's whole-history push, mapped:
- **Content store retained 4,495.5 MiB -> gone under the store** (w-hg's C1 onto it, with me). Landed history lives in
  the changeset log on disk; RAM holds only the change in flight. Measured on the store's side (tests/memory.rs,
  5cda65490): 192 MiB landed through the owner, resident 6 -> 71 MiB against a 142 MiB bound (baseline + 2 x the change in
  flight + the engine's budget). hg's number moves when its intake lands through the store instead of the content store.
- **Arena prefault reserve 4,931.4 MiB -> yours** (A-765's decay); the store reserves nothing for landed history.
- **Register log repository records 153.9 MiB + chain entries 74.0 MiB -> the store's rows on disk.** Main's ruling
  (20:32): the store is the truth; the register keeps only what §4.16's commit needs (the green's head), so these leave
  RAM as each tool moves (svn first, hg next).
- **Mapped reserve 16,384 MiB:** virtual, not RSS; yours to size from the work.
- **The store's own memory now:** its engine memtable is fanout x chunk = 512 KiB (bc0677af9), not machine-sized; its
  caches (w-perf's TextCache, learned from the chains it makes) are heap, not shm, so your MADV_FREE_REUSABLE finding
  doesn't bite there; if a block cache over the log ever lives in shared memory, its give-back unit will be a purgeable
  entry, as you measured.
