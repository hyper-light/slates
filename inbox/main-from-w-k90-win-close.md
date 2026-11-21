# C1 CLOSED -- goal 90e: kiln's slates_e2e bare_metal tests run on Windows (part 01a1f153aa82776dabb687f9c7c42b4d)
w-k90-win, 2026-11-20 ~20:20 PST

## Commits on kiln origin/main
- b7720ebca (earlier agent): `mod bare_metal` is #[cfg(any(unix, windows))]: no cfg(unix) gate on it (main eb7194f00).
- 842cc2c2f: GitHub CI job `windows (Slates tests)` (windows-latest, WinFsp 2.1 pinned, SLATES_E2E=1).
- da441f112: kiln builds on Windows again + Windows clippy clean (unmount_busy gated off Windows).
- 4af73427b: a Windows runner's harness gets kiln's MCP (reach::loopback; `kiln reach mcp` on Windows) -- the drive's
  landing on Windows had "LAND_FAIL no kiln MCP server was given to this harness".
- (cc1ecb625, another agent: Slates A-717 carried, the TouchesWord at every Windows client's connect.)

## The tests
- jupiter (Windows x86_64, WinFsp 2.1.25156, CPython 3.15.2), kiln r3, kiln main eb7194f00 (contains 4af73427b):
  `SLATES_E2E=1 cargo test -p kiln --locked --test slates_e2e -- bare_metal --nocapture --test-threads 1`
  -> 2 passed, 0 failed (93.0 s): g266_on_its_host_a_bypass_agents_landing_writes_at_once_and_an_ask_agents_only_
  once_a_person_approves and g266_a_file_changed_on_disk_after_the_agent_changed_it_is_drift_..., each step PROVEN
  (volume mounted at Z:, Ask: approval requested, own approval refused, person approved, folder exact; Bypass: written
  at once, folder exact).
- kiln's CI Windows lane: GitHub run 44851246181, job 136703754349 (`windows (Slates tests)`, windows-latest), commit
  4f5e58c1b (contains 4af73427b): slates_e2e 25 passed, 0 failed, both bare_metal drives PROVEN as above.
- Also jupiter r29 (main 64412a67): workspace::bare_metal_tests 1 passed (37.7 s) through WinFsp.

## Left (not C1; recorded as findings)
- The same CI job's `workspace::` lib step: 98/100, workspace::tests w2_/w7_ expect '/'-joined paths (test data, not the
  drive). Fix in flight (branch agents/w-k90-win-wst fb3460e9e, jupiter r4); lands as a follow-up.

## Follow-up closed (2026-11-21): kiln's CI Windows lane fully green
- f16212799 (on kiln main since 492af26f9): workspace tests w2/w7 build expected paths as the host spells them.
- GitHub run 44887857088, job 136804151233 `windows (Slates tests)`, kiln a0d114abb (contains f16212799): every step
  success -- WinFsp install, slates_e2e 25 passed / 0 failed (both bare_metal drives), workspace:: 100 passed / 0 failed.
  (Before the fix: job 136703754349 / 136745995635, workspace:: 98/100, w2 + w7.)
- jupiter re-runs of workspace:: (r7, r10, r12, r13) were all lost to the kiln server; not retried.
