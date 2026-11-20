# w-k90-pin: close goal 90's vendoring part (main, 2026-11-20 ~09:00 PST)

## Why
Ada (2026-11-20, repeated): spawn agents for kiln goal 90's remainders; "It needs to be completed." The vendoring part
has been open since w-k90-vendor moved to slates' git server work.

## Your part (kiln goal 28e73c34; main marks it met from your evidence)
Part 01a1ec34599974448569750ac79093b6: "kiln vendors slates at a pinned commit (vendor/slates with its vendor manifest
entry, as for Shards, Hex and Mantle), builds it in kiln's workspace and CI on every platform, and kiln's workspace
code calls the slates library API instead of the slates CLI: crates/kiln/src/workspace/slates.rs is replaced, not
layered (Ada 2026-11-18)."

## What to do
1. Audit kiln's origin/main against each clause and write the result in your progress file, with evidence:
   - vendor/slates at a pinned commit with its manifest entry, checked by kiln's vendor tooling the way Shards, Hex and
     Mantle are;
   - no call of the slates CLI left anywhere in kiln (search every spelling: Command::new, the binary's name in
     strings, scripts, CI steps, docs that tell a user to run it) and workspace/slates.rs replaced, not layered;
   - kiln's CI builds and tests vendor/slates on every platform kiln ships (macOS arm64, Linux x86_64 and arm64,
     Windows x86_64 and arm64): name the workflow, the job and a green run id for each.
2. Fix what's missing, red first, and land each fix within ~30 minutes.
3. Windows is w-k90-win's: it makes kiln build and test on Windows. Coordinate through inbox notes on the state branch;
   don't do its half.
4. Report when every clause holds, with a kiln commit and the CI run ids. Main marks the part met from that.

## Rules
Follow ~/fanout-state-w-k90-pin/briefs/common.md in full (clone the state branch first:
`git clone --branch agents/fanout-state git@github.com:hyper-light/slates.git ~/fanout-state-w-k90-pin`). Kiln work
lands on kiln's origin/main, confirmed with merge-base. Never force. Never checkout, switch, stash, reset or
commit-tree. No installs or system changes without Ada's word (ask main). Never print a credential. Keep
~/fanout-state-w-k90-pin/progress/w-k90-pin.md current and pushed. Report: landed (commit), ran (run id, numbers, base
commit), next.
