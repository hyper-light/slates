# w-work-mount from w-k90-win (main's Windows run keeper): the store's owner dies on jupiter -- every create refused (2026-11-21 ~19:10 PST)
Minimal case (jupiter, Windows 11 x86_64 10.0.26200, NTFS C: 925 GB with ~78 GB free, kiln runner elevated in session 0
as Jupiter\corph; release build of slates origin/main dba4ae793 and e18a2ee60, both red):
  slates anchor ... --vcs-store C:\Users\corph\w-k90-win\mx-diag\store      (ci/interop/git-matrix.sh's start)
  slates repository create <name> --tool git   ->   slates: refused: Overloaded { shard: 0 }
- Same red from `cargo test --release -p slates-cli --test git_interop stock_git_clones_pushes_and_fetches_over_the_vcs_
  http_edge_with_basic` (git_interop.rs:606, "repository create: slates: refused: Overloaded { shard: 0 }") and from the
  git and hg matrices (stop at "repository create failed").
- GREEN at the same commits on GitHub's windows-latest and windows-11-arm (runs 44968040276, 44969321647, 44974715481),
  so it is jupiter's machine/volume, not a regression.
- The store's real answer (a one-line eprintln after `disk.land` in repository.rs's creation task, jupiter scratch only):
  `the store's owner is gone (Gone)`. The anchor log says the store was granted ("the version-control store at
  C:/Users/corph/w-k90-win/mx-diag/store granted ... device 16164238977575522456, inode 1688849862613600") and nothing
  about the owner ending; the store directory holds `changesets` and `rows`, both 0 bytes. So the owner thread ends at
  or right after opening the store on this machine, silently.
- Two findings beside it (not chased): (a) the owner's exit is never logged, so its reason is lost; (b) repository.rs
  answers every unlanded creation `Overloaded`, which hides `Gone` (and any StoreError) from the client and the logs.
- I can run any diagnostic build you give me on jupiter (a branch or a patch): say what to print.
