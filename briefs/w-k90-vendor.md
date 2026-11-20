# w-k90-vendor: kiln goal 90 "kiln vendors slates ... drives it only through slates' library API, never the slates CLI" (main, 2026-11-19 ~21:55 PST)

You work on the KILN repository (github.com/hyper-light/kiln; clone it on your machine under ~/kiln-work/w-k90-vendor-*), whose
own rules are its AGENTS.md and CLAUDE.md: read both in full first and follow them over anything here. Some steps touch
slates (hyper-light/slates: follow ~/fanout-state/briefs/common.md for slates changes) or Shards: if you touch
hyper-light/shards, first read its AGENTS.md and docs/development-rules.md in full and follow R01-R16 plus
docs/development-evidence.md, recording the policy revision you read (Ada's global rule). Your own progress file:
~/fanout-state/progress/w-k90-vendor.md (keep it current; you compact automatically and never end for context). Earlier owners'
notes: ~/fanout-state/progress/g088v-kiln-vendors.md (vendoring), g264-kiln-bare-metal.md (bare metal), g293-kiln-oci-engine.md
(OCI/containers), g294-kiln-k8s-engine.md (Kubernetes). Check kiln's and slates' main for what already landed; build only
what is missing. One piece at a time: implement, focused tests, land, report in your turn text. Never force-push.
jupiter (Windows) takes no kiln work right now (its kiln is 156 commits behind and kiln can't build itself for Windows):
a Windows step that needs it is BLOCKED -- record it in your progress file and do the rest.

## Your queue
Goal 90's vendoring and the rest:
1. kiln vendors slates at a pinned commit (vendor/slates with its vendor manifest entry, as for Shards, Hex and Mantle) and
   drives it only through slates' library API, never the slates CLI -- g088v's notes.
2. kiln's review reads the repository's core.fileMode: with it false, execute bits are never committed, as git records none.
3. kiln's QUIC link takes a runner only in the runner's role (node and holder certificates refused), with a reissue path.
4. kiln's slates_e2e bare_metal tests run on Windows too (today cfg(unix): W5's step ran 0 of them) -- the cfg work and the
   tests can be written now; running them on Windows is BLOCKED while jupiter takes no kiln work (record it; CI's Windows
   runner may run them if kiln's CI has one).
