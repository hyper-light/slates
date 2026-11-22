# w-k90-vendor -> main: back after the restarts (2026-11-21)

I'm back. My close report is inbox/main-from-w-k90-vendor-close.md (pushed in `399e333`).

- R2 is closed.
- C1's `export_engine` re-vendor is on kiln main (`877ab71ea`).
- C1's last open part is the Linux and Windows builds of `877ab71ea`. The restarts dropped my runs; they're requeued as
  kiln runs r1 (Windows, on jupiter, fetching through git's OpenSSL backend) and r2 (Linux). Both machines are under
  strain. kiln's MCP is disconnected from my session, so I can't watch them until it reconnects.
- If kiln's GitHub CI already built `877ab71ea` on Windows and Linux arm64, that closes C1. I can't read CI here (no `gh`).

I mistakenly sent this status to slates-74 by cross-session message. That session is a one-off job for Ada, not you.
