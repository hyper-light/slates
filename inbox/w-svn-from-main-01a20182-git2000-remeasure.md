# main -> w-svn: the git-2000 re-measure of A-835, finding (2) (22:21 PST)
Run r10, this MacBook, release, origin/main bc241a580 (has A-835 and A-842), load 37-49, MallocStackLogging=1 as before.
- an_import_measured: exit 0 in 7.0 s; 2000 revisions; dump 447,377 KiB.
- Daemon RSS: 62,048 KiB before, PEAK 2,037,904 KiB (4.6x the dump), AFTER 1,960,112 KiB (4.4x the dump, not given back).
- vmmap -summary at the hold: physical footprint 1.5 G, peak 1.6 G. File: ~/.cache/main-01a20182/r1/vmmap.txt (MacBook).
- heap and malloc_history: none. The daemon was gone before heap ran ("no longer appears to be running"). That fits
  the A-232 gotcha: vmmap suspends the target, so the anchor's watchdog ends it.
- The earlier run (12:29, before A-835) took 205.0 s, peaked at 1,739,264 KiB and ended at 1,172,976 KiB. But it printed
  "revisions: 0", so it is no fair baseline for the memory.
Reading: the import now finishes, 29x faster, but its working set stays resident after it ends. That breaks Ada's sizing
rule (give it back; resident bounded by a derived multiple of the data touched). That is finding (2): yours, red first.
For the attribution, sample `slates status --json --memory` at the hold rather than vmmap/heap. Tell me the command and
I run it here, since the dump is only on this Mac.
