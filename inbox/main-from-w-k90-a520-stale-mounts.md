# w-k90-a520 -> main (2026-11-20 ~11:20 PST; kiln server unreachable, so here): the two stale NFS mounts on Studio-4

Slates' library unmount does not REFUSE them: it never answers.
- Red/green: slates_e2e checks::what_a_killed_drive_left_is_taken_down_and_no_mount_of_it_stays (kiln branch
  agents/w-k90-a520). Red: LEFTOVERS = exactly my two mounts; failed. Green attempt: the sweep called
  `slates_api::mount::unmount` (plain unmount(2)) and, after 10 s, `slates_api::mount::unmount_busy` (unmount(2) with
  MNT_FORCE). Both threads sat 18 min in the `unmount` syscall (sample: slates_api::mount::ask_unmount ->
  libsystem_kernel unmount) on the FIRST mount, whose server (instance kilne2e20807...) is gone since run 2. No error.
- I stopped only my test process (SIGTERM, it exited). Anchor 24116 + daemon 41438 of kilne2e23950... are untouched, up
  beneath the second mount.
- Mount lines (exact):
  slates:/kiln-01a1f9e974cb724bb826f0199619ef25 on /private/var/folders/sw/nh0dcwzs73l_z8b94ws0wgzm0000gn/T/.tmpV4H91q/slatesct-state/workspaces/kiln-01a1f9e974cb724bb826f0199619ef25 (nfs, nodev, nosuid, mounted by adalundhe)
  slates:/kiln-01a1f9f09ab67208b023d8d6c6d40daa on /private/var/folders/sw/nh0dcwzs73l_z8b94ws0wgzm0000gn/T/.tmpxlK24F/slatesct-state/workspaces/kiln-01a1f9f09ab67208b023d8d6c6d40daa (nfs, nodev, nosuid, mounted by adalundhe)
  (`umount -f` as me earlier: EPERM on the first. Both hard,intr NFS mounts per nfsstat -m; the second's server is alive
  but `ls` of it hangs.)
- Next on my side: every unmount in the sweep gets its own bound and is reported "didn't answer", never a hung test;
  an instance is never ended under a mount that stays.
- Ada decides any root step.
- Not mine, untouched: slates:/served on ~/slates-work/g226-live-shipped-volume/target/tmp/slates-mount-38179-3.
