# w-k90-win: goal 90's Windows parts -- kiln builds and tests vendor/slates on Windows, and the bare_metal tests run there (main, 2026-11-20 ~09:00 PST)

## Why
Ada (2026-11-20, repeated): spawn agents for kiln goal 90's remainders; "It needs to be completed." Goal 90 = kiln vendors
slates and drives it only through slates' library API. It stands at 32 of 39 parts met. Nobody was on the Windows parts.

## Your parts (kiln goal 28e73c34; main marks each met from your evidence: kiln commit, run id, numbers, base commit)
1. **bare_metal on Windows** (part 01a1f153aa82776dabb687f9c7c42b4d): "kiln's slates_e2e bare_metal tests run on
   Windows too (today cfg(unix): W5's step ran 0 of them)". Make the bare_metal drive build and run on Windows through
   the slates library (a slates volume per agent, review, drift, landing through a grant), red first, and land it.
2. **The Windows half of part 01a1ec34599974448569750ac79093b6** ("kiln vendors slates at a pinned commit ... builds it
   in kiln's workspace and CI on every platform ..."): kiln with vendor/slates builds and its tests run on Windows,
   locally on jupiter and in kiln's CI Windows lane. w-k90-pin owns the rest of that part; tell it what you land.

## What's known (main, measured 2026-11-20 08:55 PST, read-only)
- jupiter: Windows x86_64, 32 cores. kiln runs commands there in MSYS bash; cargo at /c/Users/corph/.cargo/bin.
  Hyper-V on, Docker Desktop present, **WinFsp NOT installed**, **C: had 7 GB free**.
- A slates mount on Windows is WinFsp (slates crates/bridge-winfsp, crates/server/src/winfsp.rs; slates' own WinFsp
  tests run on GitHub's Windows runner only, because jupiter has none). Installing WinFsp on jupiter is a tool install:
  main has asked Ada. Until she answers: build everything, run every test that needs no mount on jupiter, and run the
  mount tests wherever WinFsp already is (find out whether kiln's CI has such a Windows lane; slates' CI installs WinFsp
  in its workflow). Don't install anything yourself.
- No NASM on jupiter: for a scratch TEST build that pulls aws-lc set AWS_LC_SYS_PREBUILT_NASM=1 in your own
  environment (own CARGO_TARGET_DIR) and say so in the commit; never change a repo's config for it.
- w-k90-containers has a Windows cargo check of kiln queued on jupiter (r33). Ask it (inbox note on the state branch)
  for its findings, and take the Windows build over from it.
- Disk: put your CARGO_TARGET_DIR outside any synced folder, on the drive with the most room (check every drive
  first). If jupiter's free space blocks you, tell main with the numbers. Never delete anything that isn't yours there:
  a partial delete on Windows broke two runs on 2026-11-19.

## Rules
Follow ~/fanout-state-w-k90-win/briefs/common.md in full. Clone the state branch first:
`git clone --branch agents/fanout-state git@github.com:hyper-light/slates.git ~/fanout-state-w-k90-win`.
It covers the fast landing loop, keeping checkouts current with origin/main, performance gaps fixed up front, decisions
by A/B data, no `rm -r`, landing within ~30 minutes, unsigned commits and the Studio push routes. Kiln work lands on
kiln's origin/main, confirmed with merge-base; slates work, if any, on slates' origin/main. Never force. Never
checkout, switch, stash, reset or commit-tree. No sudo, admin, installs or system changes without Ada's word (ask
main). Never print a credential. Bound every run and read its output at least every 10 minutes. Keep
~/fanout-state-w-k90-win/progress/w-k90-win.md current and pushed. In your kiln summary report per part: landed
(commit), ran (run id, numbers, base commit), next.
