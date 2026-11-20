# w-k90-containers: kiln goal 90 "kiln vendors slates ... drives it only through slates' library API, never the slates CLI" (main, 2026-11-19 ~21:55 PST)

You work on the KILN repository (github.com/hyper-light/kiln; clone it on your machine under ~/kiln-work/w-k90-containers-*), whose
own rules are its AGENTS.md and CLAUDE.md: read both in full first and follow them over anything here. Some steps touch
slates (hyper-light/slates: follow ~/fanout-state/briefs/common.md for slates changes) or Shards: if you touch
hyper-light/shards, first read its AGENTS.md and docs/development-rules.md in full and follow R01-R16 plus
docs/development-evidence.md, recording the policy revision you read (Ada's global rule). Your own progress file:
~/fanout-state/progress/w-k90-containers.md (keep it current; you compact automatically and never end for context). Earlier owners'
notes: ~/fanout-state/progress/g088v-kiln-vendors.md (vendoring), g264-kiln-bare-metal.md (bare metal), g293-kiln-oci-engine.md
(OCI/containers), g294-kiln-k8s-engine.md (Kubernetes). Check kiln's and slates' main for what already landed; build only
what is missing. One piece at a time: implement, focused tests, land, report in your turn text. Never force-push.
jupiter (Windows) takes no kiln work right now (its kiln is 156 commits behind and kiln can't build itself for Windows):
a Windows step that needs it is BLOCKED -- record it in your progress file and do the rest.

## Your queue
Goal 90's container engines:
1. kiln's OCI/Docker engine runs each agent's container on its slates volume through the slates library (a bound slates
   volume, never the slates CLI) -- g293's notes.
2. kiln's Kubernetes engine runs each agent's pod on its slates volume through the slates library (kubelet's mount of the
   volume) -- g294's notes.
3. A-520 piece 5: a runner's container engine wired onto workspace::container (g293).
