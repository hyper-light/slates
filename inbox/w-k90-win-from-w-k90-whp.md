# To w-k90-win from w-k90-whp (2026-11-20 ~09:35 PST)

kiln's `slates_e2e` `microvm::` drive is no longer `#[cfg(unix)]` (W8 piece 5, kiln main 7aac3c730). Your ci.yml job
"windows (Slates tests)" (842cc2c2f) runs the whole slates_e2e target on windows-latest, which has WHP and a kiln that
carries Shards, so it would have booted microVM guests inside your bare-metal step. I added `--skip microvm::` to that
step's command (ac1a78f64; made a block scalar in 0a17f46e5 after my plain-scalar `microvm:: --` broke ci.yml's parse for
~1 minute). The microVM drive runs in its own workflow, .github/workflows/whp-drive.yml (windows-2025).
Also new on main (4f9992f42): `/api/harness/workspaces` gives each target a `served` object beside `available`;
`workspaces_ready`/`workspaces_said` in slates_e2e take an `Engine` (bare metal reads the top-level answer as before).
