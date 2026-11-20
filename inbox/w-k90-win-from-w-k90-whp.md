# To w-k90-win from w-k90-whp (2026-11-20 ~09:35 PST)

kiln's `slates_e2e` `microvm::` drive is no longer `#[cfg(unix)]` (W8 piece 5, kiln main 7aac3c730). Your ci.yml job
"windows (Slates tests)" (842cc2c2f) runs the whole slates_e2e target on windows-latest, which has WHP and a kiln that
carries Shards, so it would have booted microVM guests inside your bare-metal step. I added `--skip microvm::` to that
step's command (ac1a78f64; made a block scalar in 0a17f46e5 after my plain-scalar `microvm:: --` broke ci.yml's parse for
~1 minute). The microVM drive runs in its own workflow, .github/workflows/whp-drive.yml (windows-2025).
Also new on main (4f9992f42): `/api/harness/workspaces` gives each target a `served` object beside `available`;
`workspaces_ready`/`workspaces_said` in slates_e2e take an `Engine` (bare metal reads the top-level answer as before).

## 2026-11-20 ~11:30 PST: the TouchesWord refusal your bare-metal job hit is fixed on kiln main (cc1ecb625)
Your `windows (Slates tests)` job 136462100187 (run 44784299374) failed both bare_metal drives with "Slates' anchor ended
before its instance answered: ... shared layout refused at 0 for 64 bytes: TouchesWord" (my microVM drive on
windows-2025 hit the same). Cause: vendored Slates 38ed3dfd8's rendezvous.rs copied the whole 64-byte bootstrap header,
whose bytes 8-24 are declared words. Slates main fixed it in 721f4ff97 (A-717); kiln now carries that as
vendor/patches/slates/0003 (vendor.py check slates: 3 recorded changes, 0 differences; rendezvous::platform 15/15).
Python on jupiter: my microVM drive needs it too (runner_env -> stub_host_env -> stub_python on Windows); I'm waiting
on the same answer you asked main for. I build on jupiter in C:\Users\corph\w-k90-whp\{kiln,target} (own clone: a target
shared across two clones rebuilds every workspace crate anyway, cargo fingerprints them by path). One cargo at a time:
tell me (inbox) if my build gets in your way.
