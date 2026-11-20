# w-k90-close: close goal 90's open Linux proofs (main, 2026-11-20 ~07:15 PST)

## Why
Ada (2026-11-20): "Kiln goal 90 should be making more progress. Is it stalled?" ... "Dispatch a new agent if needed to
complete the work!" Goal 90 is kiln vendoring slates and driving it only through slates' library API. It stands at 32 of
39 parts met. Of the open parts, two had no working owner: the Kubernetes engine's KIND proof, which had been left to main,
and A-520 piece 5. Those two are yours.

## Your parts (kiln goal 28e73c34's parts; main marks them met from your evidence)
1. **The Kubernetes engine's KIND proof** (part 01a1f1249c5e75b5bbf0c92c84385109): "kiln's Kubernetes engine runs each
   agent's pod on its slates volume through the slates library (kubelet's mount of the slates export, never a host
   copy), with review, drift and landing through a grant, proven on a KIND cluster through kiln". w-k90-containers wrote
   the W10 code; all of it has landed on kiln main (k8s::volume, kubeapi PV/claim verbs, chart rbac.agentPods, the node
   tlshd certificate, workspace::held). What's missing is the proof: a real pod on a KIND cluster gets its slates volume,
   an agent writes in it, and review, drift and landing work through a grant. Run it, fix what fails (red first), land
   it, and report the run's id and its numbers.
2. **A-520 piece 5** (part 01a1f1c12da27069a741fd0c3d67c57d): "a runner's container engine wired onto
   workspace::container". Read A-520's entry in slates' docs/wip/SLATES_DESIGN.md and kiln's docs/agentic.md for W9/W10,
   then build and land it on kiln main, proven by use.

## Machines
- adacachy is the only Linux x86_64 box with KVM, Docker and KIND (kindnetd processes show a KIND cluster up for 15
  days). Run Linux steps there with kiln run (`on: adacachy`, sync false where the step isn't about the synced tree).
  It's at load ~62, with other sessions' long jobs, so:
  - bound every run you start;
  - read its output at least every 10 minutes, with kiln output wait_s up to 600;
  - never end a turn waiting more than one of your own estimates without reading the output.
  If a run loops or hangs, stop it (your own run only) and find out why.
- Never kill or touch another session's processes on adacachy. List them for main if they're in your way.
- Build kiln on the Studios where you can, and use adacachy for the Linux-only steps.

## Coordination
- w-k90-containers owns the OCI/Docker engine's live test (part 01a1f1249c5e75b5b569e52f814f3dc6), and w-k90-microvm
  the KVM microVM proof. Don't take their parts. Ask them, through an inbox note on the state branch
  (inbox/w-k90-containers-from-w-k90-close.md, ...), for anything about the code they wrote.
- Ada's word is still pending on the Windows items (WinFsp on jupiter, WHP). Don't touch them.

## Rules
- Follow ~/fanout-state-w-k90-close/briefs/common.md in full. Clone the state branch first:
  `git clone --branch agents/fanout-state <slates origin> ~/fanout-state-w-k90-close`, or a worktree of it.
  It covers:
  - the fast landing loop;
  - checkouts kept current (merge origin/main; never rebase, reset, checkout or stash);
  - performance gaps fixed up front;
  - decisions by A/B data;
  - no `rm -r` (kiln's gate holds it for a person);
  - finished work landed within ~30 minutes;
  - unsigned commits are fine;
  - the Studio push routes.
- Kiln work lands on kiln's origin/main, confirmed with merge-base. Slates work, if any, lands on slates' origin/main.
- Never force. Never checkout, switch, stash, reset or commit-tree. Install no tools without Ada's word. Never print or
  copy any credential.
- Keep a progress file at ~/fanout-state-w-k90-close/progress/w-k90-close.md and push it to agents/fanout-state after each
  step. Report in your kiln summary: what landed (commit), what ran (run id, numbers, base commit), and what's next.
