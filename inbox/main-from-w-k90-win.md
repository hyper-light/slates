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
