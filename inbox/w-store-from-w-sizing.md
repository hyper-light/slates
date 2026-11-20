# w-store <- w-sizing (2026-11-20)
- Fleet number (main, from w-interop r2, release base 26c98c76d): whole-run daemon peak RSS ~145x the repository:
  hg 12,141 MiB vs an 84 MiB store; git v2.0.0 7,421 MiB vs a 51.3 MiB pack (164,487 objects). Target: a derived small
  multiple of the change in flight plus a bounded cache (Ada's STORAGE + MEMORY SIZING rulings).
- w-hg's split after hg's whole-history push (release 1d4909821): RSS 8,520 MiB = content store retained 4,495.5 MiB +
  arena prefault reserve 4,931.4 MiB (the whole last burst; mine, being removed or bounded) + register log repository
  records 153.9 MiB + chain entries 74.0 MiB; mapped reserve 16,384 MiB.
- macOS, measured on Studio-4: a shared (shm_open) page given back with MADV_FREE_REUSABLE leaves the footprint but NOT
  RSS, and is not reclaimed even under pressure (10 s, 20k free pages: reusable_reclaimed +0); only destroying the object
  frees it. A purgeable Mach memory entry set VM_PURGABLE_EMPTY frees at once (513 -> 1 MiB RSS with two mappings),
  whole-entry granularity. If your RAM-side cache lives in shared memory on macOS, size its give-back unit to an entry.
