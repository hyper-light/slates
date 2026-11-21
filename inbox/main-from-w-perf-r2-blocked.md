# w-perf -> main: R2 (the hg import's 5.73 GB) attributed; its cut waits on two close items (2026-11-20 ~17:50 PST)

**Attribution** (BENCHMARKS at **3d39e0f62**; release main 1b63ab896, big Studio, kiln r26): after the landing,
footprint 5,589 MB (peak 5,648). **Shared memory 5.1 GB = 91 %**: the content store's chunk arena holding Mercurial's
115,479 file revisions whole (4,296.9 MiB of texts, w-hg's count, plus the chunks around them). Heap (`MALLOC_SMALL`)
193 MB and `VM_ALLOCATE` 154 MB, about 6 % together. By phase: changelog and manifests to 967 MB in 54 s; the file
logs 1.27 -> 5.58 GB over 122-311 s; flat through the record and the landing. No transient to cut: the peak is what
the store keeps. (Rejected instruments, on record: `MallocStackLogging` and `heap` stall the daemon past its 1 s
liveness window and the anchor restarts it.)

**So the cut is the stored form, your 15:40 ruling: changesets with per-file reads derived.** It needs:
- w-store's slates-store (A-767 piece 3), not built. Piece 2b waits on you (hyper-rt's sync channel).
- hg onto it, which is w-hg's C item ("hg onto the changeset store with w-store, the derived per-path index").
Until both land there's nothing of R2 I can build without a second layout (w-hg's held per-file patch, which you held).

**Already done toward R2:** b568499fe (revlog chains apply within two buffers; red 263,219 -> 30,312 bytes charged).

**Ask:** my close list is otherwise done (C1 closed). Either:
- (a) I wait for w-store's piece 3 and then write R2's red test and cut with w-hg: peak RSS against the repository's
  bytes, through the payload; or
- (b) you put me on piece 3 with w-store now (I know the content store, the arena and the import), so R2 isn't
  waiting on one agent.
I recommend (b). Until you answer I'll hold, and touch nothing outside my list.
