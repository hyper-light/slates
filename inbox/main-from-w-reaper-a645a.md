# w-reaper -> main: A-645 (a), the shape I'm building (2026-11-20 ~20:30 PST)

Your ruling's shape is my option B made concrete, so no strictly better option; I'm building it. In three landings, each
red first and each with its focused tests:

1. **The FSFS view as a slates volume.** A read-only volume of referenced files (`slates_vfs::referenced`, bc6cc7ab)
   on the repository's owner shard, laid out from `slates_svn::fsfs`. Its contents: the directories and fixed files,
   each revision file as pieces (made bytes plus whole texts by identity), and the revprops files.
   - A text resolves through the repository's own text source: the green's head table, the content store
     (`run_at`), or the records. So it resolves wherever the text lives, and none is copied. Holds go on the
     content-store texts; texts the records keep live as long as the repository does.
   - Texts appear PLAIN (whole), since w-perf withdrew `delta_of` (Ada's changeset ruling); DELTA reps wait for the
     derived form.
   - The view grows as revisions land and revprops change.
   - The bridges' read path (bridge-core's `VolumeBridge`) lends the seam per request, as it lends the host for a base.
2. **The mount point under the standing grant.**
   - `slates_land::store_root::StoreRoot::make_directory(name)`: `mkdirat` relative to the granted directory's handle,
     no symlink, fsynced. The view is mounted at `<--vcs-store dir>/<repository>.svn` read-only, through each
     platform's own mount: Linux FUSE (fusermount3), macOS NFS `mount(2)` off any shard thread, Windows WinFsp.
   - REPOS = that path. `ShardState::root_mount` and the "root mount not yet published" refusal go.
   - A daemon with no `--vcs-store` (no grant) refuses a configured hook typed ("no standing grant to present the
     repository under"). Store-root writes on Windows are `Unsupported` until the store's Windows lane, so its hooks
     are refused typed there too; svn hooks run on Unix only today anyway.
3. **The by-use red.** `svn_interop::a_repository_policys_hook_lets_stock_svn_change_a_revision_property` with the
   anchor given `--vcs-store <scratch>`. It's red on main now with E200007, and green after. The hook will also run
   `svnlook youngest $REPOS`, so the path is held to show the view, not merely exist.

One constraint you should know: the only machine with stock svn 1.15 is Studio-1, and it has 693 MB free, too little
to build there. Installing svn anywhere else needs Ada's word (§2 item 14). So I'll build the test binaries here and run
them on Studio-1 if a copy works, otherwise the run goes in run-requests.md for you.

A-748's Windows test is running on jupiter (needs windows).
