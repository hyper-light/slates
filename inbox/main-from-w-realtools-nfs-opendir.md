# w-realtools -> main, 2026-11-21 ~13:30 PST: a fresh slates NFS mount stalls its first opendir on two Mac Studios (found by use)

## What happens
- Hosts: Adas-Mac-Studio-2, reproduced twice today (r6 at 1d14aa6; r13 at 7acdf39, both based on main 1a35ab09), and
  Studio-4 yesterday (base 2e4415441).
- On a new `slates mount` of an overlay volume, the first make blocks in the kernel's `opendir` (sample: __opendir2 ->
  open$NOCANCEL), and so does every later directory listing or create on that mount.
- The daemon answers every call it gets. Its counters: getattr 37, lookup 8, access 8, fsstat 345, fsinfo 1,
  pathconf 1, with no READDIR(PLUS) and no CREATE ever counted. So the macOS client never sends them: it repeats
  FSSTAT about every 5 s and holds the opendir.
- On Studio-2 there were no dead NFS mounts before the run (nfsstat -m: 0 "not responding"). So yesterday's
  dead-mount theory from Studio-4 doesn't explain it.
- The same flows passed on adas-macbook-pro-2 on 2026-11-20 (617c88102, 9793d9218).
- Not yet known: host-specific or a regression. I started the same test at 9793d9218 on Studio-2 (r14), but Studio-2
  stopped answering kiln (76 MB free). I'll check and clean up there as soon as it answers.

## What it blocks
My real_tools changes on branch agents/w-realtools-93:
- 1d14aa65: the held compile under clang, gcc and cc (my brief says "clang and gcc where present").
- 7acdf392: every tool step bounded at 300 s, so a stalled mount fails the test naming the stall instead of hanging.
  r13 shows it working: failed at 305 s with "make ... ran past 300s: the mount may be stalled", nothing left mounted.
- 69a06d89: A-781's status line names the acceptance flows.
They need one green run on a Mac where mounts work. The only one that has passed is adas-macbook-pro-2, your MacBook,
which I guard my runs off. Can I run that one test there once (about a minute of build plus a 30-second test)? Or say
which Mac to use.

## Owner
No brief names the macOS NFS mount path's owner; please route it. The repro is
`SLATES_TEST_CLI=1 cargo test -p slates-cli --test real_tools -- --exact a_pragma_once_build_make_cargo_and_git_status_hold_across_a_snapshot_and_a_daemon_restart`
on a Studio. Red: "make ... ran past 300s: the mount may be stalled".
