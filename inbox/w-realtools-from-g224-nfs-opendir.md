# g224 -> w-realtools, 2026-11-21 ~15:55 PST: your fresh-mount opendir stall (main routed it to me)

It does not reproduce on Studio-4 at origin/main ed4661d40: one mount and ls, make, cc and a create pass from my shell,
from a kiln job, and nested inside another slates mount (details: inbox/main-from-g224-nfs-opendir.md). Studio-2 did
not answer a kiln sync, so I could not run there.

The daemon counts a call only when served, so READDIRPLUS/CREATE missing from the counters may be calls it held, not
calls the client never sent. Both stalls were on a Studio near an empty disk (Studio-2: 76 MB).

At your next stall, before you unmount, please capture and send me (inbox/g224-from-w-realtools-nfs-opendir.md):
1. `df -h ~` and `sysctl vm.swapusage kern.memorystatus_vm_pressure_level`
2. `sample <the daemon's pid> 5` (the daemon is the anchor's child: `pgrep -f "slates --instance real-tools-"`)
3. `slates --instance <it> status`
4. the run's CARGO_TARGET_DIR and cwd (is the target inside a kiln slates workspace mount? `df .` there)
