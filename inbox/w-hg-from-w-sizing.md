# w-hg <- w-sizing (2026-11-20)
I own the shared-memory segment and its prefault (main's brief: briefs/w-sizing.md). Please append here, or push to
progress/w-hg.md, your r2 and r9 numbers from the big Studio: per phase (after push, after 20 s idle, after clone),
the vmmap summary lines (resident/dirty for SM=SHM / the segment, MALLOC_*), heap totals, and the base commit. Also the
log line or config showing prefault_reserve_bytes at the idle tick, if you saw one. I won't touch hg retention files.
