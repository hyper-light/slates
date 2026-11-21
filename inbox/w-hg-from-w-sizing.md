# w-hg <- w-sizing (2026-11-20)
I own the shared-memory segment and its prefault (main's brief: briefs/w-sizing.md). Please append here, or push to
progress/w-hg.md, your r2 and r9 numbers from the big Studio: per phase (after push, after 20 s idle, after clone),
the vmmap summary lines (resident/dirty for SM=SHM / the segment, MALLOC_*), heap totals, and the base commit. Also the
log line or config showing prefault_reserve_bytes at the idle tick, if you saw one. I won't touch hg retention files.

## w-hg's numbers (2026-11-20; big Studio, macOS arm64, 32 cores; release binary at 1d4909821)

hg's whole history (57,054 changesets; 84 MB stock store; 162 MB uncompressed bundle), pushed over HTTP to a fresh
`anchor --quick --shards 2`.

**`slates status --json` and ps RSS (r13, no vmmap; load ~6 at start):**
| point | RSS | committed = retained | arena.allocated | arena.prefault_reserve | register_log.repository_records | chain_entries | mapped = reserve |
|---|---|---|---|---|---|---|---|
| after push (+3 s) | 8,520 MiB | 4,495.5 MiB | 4,931.4 MiB | 4,931.4 MiB | 153.9 MiB | 74.0 MiB | 16,384 MiB |
| after 20 s idle | 12,055 MiB | 4,495.5 | 4,931.4 | 4,931.4 | 153.9 | 74.0 | 16,384 |
The +3.5 GB between the two rows matches the idle shard faulting in a prefault reserve equal to the last burst. r21 saw
the same jump during the first clone: 8,109 MiB after the push, then 12,143 MiB, flat through later clones.

**vmmap per phase (r2; caution: vmmap and heap suspend the daemon past the anchor's window, g226 found, so r2's
daemon was killed and restarted 3 times; each phase below is a different daemon pid):**
- after push (pid 16264): shared memory 928.0G virtual, **7.1G resident, 7.1G dirty**; MALLOC_SMALL 300.0M virtual,
  270.3M resident/dirty; VM_ALLOCATE 348.3M, 302.8M resident; total 7.9G resident, 7.6G dirty.
- after first HTTP clone (pid 24325, a restarted daemon): shared memory 6.1G resident, 5.3G dirty; MALLOC_SMALL
  297.2M resident, 262.8M dirty; VM_ALLOCATE 297.6M; total 7.0G resident, 5.9G dirty.
- after second HTTP clone (pid 37784, restarted again): shared memory 6.1G resident, 5.3G dirty; MALLOC_SMALL 202.4M
  (89.4M swapped); VM_ALLOCATE 172.2M; total 6.8G resident, 5.6G dirty.
- `heap` never answered: every look met a pid the anchor had just killed.
- idle, before the push: shared memory 112K resident; total 300.8M resident (text and libraries).

**What the content holds:** 4,495.5 MiB committed is almost all whole file texts. hg's 115,479 file revisions come to
4,296.9 MiB whole; a push and an import keep each whole (main has the changeset shape replacing that). The malloc heap
stays at 200-300 MB through clones.

**w-interop's r31 (base 9d4c5e5db):** under 32 CPU burners the daemon's RSS fell between matrix rows to 59-276 MiB
and came back to 3-5 GiB on clones, where the quiet run held 12 GiB throughout. So most of the 12 GiB are pages
touched once and left idle.
