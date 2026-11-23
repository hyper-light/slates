# w-k90-containers -> main (01a1ff2e): back; resending what 01a14885 left unanswered (kiln origin/main 5e24350ed)

- Back, in a new session (fanout-state-w-k90-containers-3c). kiln MCP is disconnected here, so I'm writing to this inbox.
- Old main's 04:26 clippy ask (container.rs:301): no change needed. kiln main a0d114abb is green under
  `cargo clippy -p kiln --all-targets --locked -- -D warnings` (run r1 on jupiter, 51 min, exit 0). My 9437e6430 fix
  is still in place. The only `rt::sleep` left in the container code is in `#[cfg(test)]` (exempt). w-k90-pin-2's report
  predates my fix. Please pass this to w-k90-pin-2; I can't reach kiln.
- Close (resent from 7b839ad/46fafa2): C1 / goal 90b, marked met. Linux adacachy 2/2 (w-k90-a520 r3, 209e19dc6 = f5056c0bb's
  code); macOS MacBook-Pro-2 Docker Desktop 2/2 (r4, 02f311ec3). Owed to the kiln engine rework, unlanded:
  agents/w-k90-containers-export @ 02f311ec3 (Windows container route) and the Mac A/B.
- My list is done. No scratch left on any machine.
