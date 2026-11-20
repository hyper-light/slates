# w-k90-kvm: finish goal 90's Linux microVM proof, the resync end, and A-520 piece 5 (main, 2026-11-20 ~08:10 PST)

## Why
Ada (2026-11-20): "Please dispatch a new agent [to] tackle Kiln goal 90. It needs to be completed." Goal 90 is kiln
vendoring slates and driving it only through slates' library API, and it stands at 32 of 39 parts met. The KVM half of the
microVM proof has made no progress in four hours, so it's yours, with the two parts nearest it.

## Your parts (kiln goal 28e73c34; main marks each met from your evidence: kiln commit, run id, numbers, base)
1. **Shards microVM engine on Linux KVM** (part 01a1f1249c5e75b5bc82c9eb011550f4: "kiln's Shards microVM engine proven
   through kiln on Linux KVM and Windows WHP as well as macOS HVF"). HVF is proven: the macOS `microvm::` drive set
   passes 2 of 2 at kiln main 81174ac49 (g266 and g90). Run the same drive set (`SLATES_E2E=1 VM_E2E=1 slates_e2e
   microvm::`) through kiln on adacachy's KVM, at current kiln main, and fix what fails, red first. WHP stays blocked
   until Ada's word about Windows' hypervisor on jupiter.
2. **SlatesVolumes keeps slates' resync end** (part 01a1f18921e974cf81e815c89ec5603d): "a daemon restart never ends a
   VM's share". It's proven on HVF (g90: daemon 4630 killed, successor 19863 served the agent's next write). Prove the
   same on KVM in the same drive. Shards D288 and Hex's patch are carried by kiln as vendor/patches until their owners
   merge them. That's fine and not yours.
3. **A-520 piece 5** (part 01a1f1c12da27069a741fd0c3d67c57d): "a runner's container engine wired onto
   workspace::container". Read A-520 in slates' docs/wip/SLATES_DESIGN.md and kiln's docs/agentic.md (W9/W10). Build it on
   kiln main, proven by use, and land it.

## Hand-off
w-k90-microvm ran the HVF drive and the KVM run r4. Read its progress file first
(`git show origin/agents/fanout-state:progress/w-k90-microvm.md`) and its logs under ~/.cache/w-k90-microvm-logs on the
Studio where it ran. Earlier, a `kiln agent --name slatesvm` process built from its target was respawning every few
seconds on adacachy. Find out whether that's the drive looping.

## Machines
adacachy is the only Linux x86_64 box with KVM, and it's heavily loaded (load ~62; other sessions' long jobs).
- Bound every run.
- Read the output at least every 10 minutes.
- Never wait more than one of your own estimates without reading it.
- Stop a run of your own that loops, and find out why.
- Never touch another session's processes. List them for main.
- Build on a Studio where you can.
- No sudo, no kernel modules, no system changes, no tool installs: those need Ada's explicit word. Ask main.

## Rules
Follow ~/fanout-state-w-k90-kvm/briefs/common.md in full (clone the state branch first). It covers the fast landing loop,
keeping checkouts current with origin/main, performance gaps up front, decisions by A/B data, no `rm -r`, landing within
~30 minutes, unsigned commits, and the Studio push routes. Kiln work lands on kiln's origin/main, confirmed with
merge-base. Never force. Never checkout, switch, stash, reset or commit-tree. Never print a credential. Keep
~/fanout-state-w-k90-kvm/progress/w-k90-kvm.md current and pushed. In your kiln summary, report per part: landed (commit),
ran (run id, numbers, base commit), next.
