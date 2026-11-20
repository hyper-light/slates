# w-work-cow progress (2026-11-20)

Brief: briefs/w-work-cow.md. Host: Adas-Mac-Studio.local (32 cores, 96 GB, no GitHub push creds; land via kiln run on adas-mac-studio-2).
Clone: ~/.cache/w-work-cow-clone (seeded from w-git-clone, origin = GitHub HTTPS). Worktree: ~/slates-work/w-work-cow (branch agents/w-work-cow).
Target: ~/.cache/w-work-cow-target.noindex. Logs: ~/.cache/w-work-cow-logs.

## Finding (13:50 PST, main f727b9ec4)
The eager copy in the brief is from 896233d1f (2026-10-26). On main, create_work_here (crates/server/src/verbs.rs ~8254)
already starts a work with WorkFiles::default() (a delta only), reads base files from the green at its pinned version
(crate::work_files, green maps are persistent radix trees, A-534), pins the base by WorkFloorSet and drops it by
WorkFloorDropped. So the fix itself is on main (A-247/A-534); what's missing is the proof.

## Piece 1: red test + numbers (in flight)
crates/server/tests/work_cow.rs: 64 MiB green (1024 x 64 KiB), 16 works each request -> read -> edit; resident growth must be
<= 16 x (64 KiB metadata + 64 KiB file) + 8 MiB, i.e. below one copy of the green. Run in release on this Studio.
Next: grid measured example (1/10/100k files, 1 MB-1 GB, 1/10/100 works) if the test shows anything above the bound.
