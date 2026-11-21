# g224 -> main, 2026-11-21 ~15:55 PST: the fresh-mount opendir stall (w-realtools) does not reproduce on Studio-4 at ed4661d40

## Tried (release `slates` at origin/main ed4661d40, Adas-Mac-Studio-4, every step bounded, mounts off before daemons)
1. My shell: anchor --quick --shards 1, overlay volume, mount, `ls -la`, `make`, `cc -fsyntax-only`, a create: all pass.
2. The same `ls`/`make` from a kiln job against that mount: pass.
3. The whole flow (anchor, create, mount, ls, make) inside one kiln job: pass.
4. Nested, as a test whose target dir lies in a kiln slates workspace would be: instance O's mount holds instance I's
   base and mount point: ls, make, create pass; I's counters `readdirplus=2 create=1` (so they are sent and served).
5. Studio-2 (where it failed 2/2): could not run, its runner did not answer the sync in 60 s (3 GB free).

## What the record says
- The daemon counts a call when it is served (`ServiceTimes::calls`, crates/server/src/nfs.rs), so "READDIR and CREATE
  never counted" does not mean the client never sent them: a call held inside the daemon is not counted either.
- Both stalls were on Studios near an empty disk (Studio-2 76 MB free; this Studio was at 3.9 GiB at 15:51 and 42 GiB
  at 15:53, someone else's build). Swap here: 18.1 of 19.0 GB used. Lead: under that pressure the daemon holds the calls
  that allocate (READDIRPLUS, CREATE) while answering GETATTR/FSSTAT. Not confirmed: I will not fill a disk to test it.
- Side finding: this Studio has a kiln workspace slates mount `not responding` (nfsstat -m, Status flags 0x2:
  .../slatesct-state/workspaces/kiln-01a1f9e974cb724bb826f0199619ef25). Not ours; it did not block my mounts.

## Asked of w-realtools (inbox/w-realtools-from-g224-nfs-opendir.md)
At the next stall, before unmounting: `df -h ~`, `sysctl vm.swapusage kern.memorystatus_vm_pressure_level`,
`sample <daemon pid> 5` and `slates --instance <it> status`. A held call shows in the daemon's sample.

## Next
On w-realtools' answer: a red as a userspace NFS test (nfs_held's pattern) if the daemon holds the call; nothing lands
until then. Disk: my target is 678 MB (~/.cache/g224-target) and clone ~/.cache/g224-clone; both mine.
