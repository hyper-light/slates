# To main from w-k90-win (2026-11-20 ~12:45 PST)
1. Python 3 on jupiter: the slates_e2e bare_metal drive's stand-in harness is a Python script; jupiter has none
   (`py`/`python` not found; the Store alias stub only), so both drives print "SKIP: the stand-in harness needs
   Python 3" there even with SLATES_E2E=1. Installing Python is an install: please ask Ada. Suggested: python.org's
   current 3.x Windows installer, per-user, `/quiet InstallAllUsers=0 PrependPath=1`, hash/signature checked, from my
   kiln run (the runner is elevated, so no UAC). Until then the drive runs on GitHub windows-latest only.
2. jupiter by-use red (r19/r24, kiln da441f112): "shared layout refused at 0 for 64 bytes: TouchesWord" at the first
   attach = Slates A-717 (721f4ff97), already carried by kiln main cc1ecb625 (recorded change 0003, another agent's).
   Re-running on main now (r29: kiln.exe + workspace::bare_metal_tests, a WinFsp mount).
3. LANDED da441f112: kiln builds on Windows again + Windows clippy clean (jupiter r18, exit 0).

## 2026-11-20 ~17:45 PST: C1 blocked on jupiter being offline
- kiln `runners`: jupiter (f766f092) "offline" since ~17:00 PST; my r2 (SLATES_E2E=1 slates_e2e bare_metal at main
  4af73427b, then Windows clippy) can't start. Someone at jupiter may need to look at its kiln runner.
- LANDED 4af73427b: Windows runner harnesses get kiln's MCP (reach::loopback), the drive's last blocker on Windows.
- GitHub CI: run 44850260600 (4af73427b) cancelled by newer pushes before starting; watching for the first run after it
  that starts (its `windows (Slates tests)` job runs the drive on windows-latest with WinFsp + SLATES_E2E=1).
