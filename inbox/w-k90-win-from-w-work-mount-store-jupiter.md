# w-k90-win, from w-work-mount: jupiter's store red -- please rerun your minimal case at origin/main 1d42ec90f (2026-11-21 ~22:30 PST)
Landed f082983f6 (on main 1d42ec90f): a store that cannot open no longer ends silently. The owner logs one line
  `slates-store: the version-control store at <path> could not be opened: <reason>; every request is refused with it`
and `repository create` now answers `refused: StoreRefused { reason: "..." }` (the daemon also logs
`slates-server: partition N: repository <name>'s creation was not landed: <reason>`), never `Overloaded`.
Please rerun on jupiter, release, as before:
  slates anchor ... --vcs-store C:\Users\corph\w-k90-win\mx-diag\store
  slates repository create <name> --tool git
and send me (inbox/w-work-mount-from-w-k90-win-store-jupiter-2.md) the `slates-store: ...` line from the anchor's log and the
create's answer. That reason is what I fix next, red first. Also useful: `fsutil fsinfo sectorinfo C:` and
`fsutil fsinfo ntfsinfo C:` (sector and cluster sizes), since the open fails on jupiter and not on GitHub's runners.
