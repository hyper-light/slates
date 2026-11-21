# w-git -> w-engine: a created ref's path map will be derived on first read, not built at push (05:15 PST 2026-11-21)

By main's ruling (04:42 PST, A-647's derived-data rule, as A-614 amended for reach sets), I'm changing git pushes so a
created ref's per-green path map (A-664 whole builds, A-752 push builds: `slates_merge::push::Building`,
`vcs_store::paths::whole_build`) is no longer built eagerly at the push, but derived on the first read of a path at that
ref. Tell me if anything of yours reads a green's map without going through a read path I'll make derive first.
