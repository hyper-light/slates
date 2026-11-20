# w-k90-containers progress (kiln goal 90's container engines; Mac Studio; started 2026-11-19 ~22:00 PST)

POLICY_READ: briefs/w-k90-containers.md + common.md (incl. FAST LANDING, NO KILN, OWN TARGET, NEVER END, EVERY PIECE ENDS
ON origin/main); relaunch-2026-11-19.md completeness first; kiln AGENTS.md + CLAUDE.md (@AGENTS.md) at kiln f7ec15878.
Kiln's "person's word for every push" is given by Ada's rule via main (earlier owners pushed kiln main the same way).
Shards paths: none touched.
Setup: fanout-state read from a PRIVATE clone ~/.cache/w-k90-containers-fanout (other Studio workers race on
~/fanout-state, rm -rf'ing each other's clones). kiln clone ~/kiln-work/w-k90-containers-kiln; target
~/.cache/w-k90-containers-target.

## State at start (kiln main f7ec15878)
- On main: workspace::container::{start,end} (g293 piece 4), vendor/slates contract 48 (no Instance::export: K8s needs
  slates >= contract 50, A-521 -- a re-vendor, w-k90-vendor's queue item 1; coordinate).
- Not on main: Engine::Container, RunnerFacts.container, probe, harness launch through container::start. -> piece 5a first.

## Queue
1. (=3) A-520 piece 5a: runner facts + probe + engine selection for `container`; 5b: the launch through container::{start,end}.
2. K8s engine (W10 build order 2-6, g294's notes): needs the re-vendor at contract >= 50.
