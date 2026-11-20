# Weekly-limit pause, 2026-11-19 ~02:45 UTC (resets 08:00 America/Los_Angeles = 16:00 UTC)
Resume each by SendMessage to its agent id ("the weekly limit has reset; continue your piece from where you stopped: re-read your progress file and worktree; if it landed, report and END"). If SendMessage says "stopped and won't be resumed", relaunch a fresh agent with the same piece (Ada asked for restarts).

| agent | id | UI goal | piece | where it stopped |
|---|---|---|---|---|
| g230 | abb5ad4f0f4b79683 | 85 | A-562 B9 sliced notify rounds | about to land: branch agents/g230-a562-sliced-rounds 60d630382 (merged with main 8cb6bef1a), fetch+merge+diff+push owed |
| g225 | a3405e2abbb29a993 | 85 | A-564 fenced forward NotFound | writing the bug record + amendment |
| g267 | afc1bb3989cecb612 | 84 | A-577 live clone durable before its reply | reading: clone capture, holders_placed, live-state fields |
| g076b | a9b52dae279c3c025 | 86 | A-574 laptop failover term | merging docs (keep main's A-570 paragraph) |
| g294 | a2409f26c476eda35 | 88 | QUIC link runner role + deploy/link.rs | reading net.rs ServerBook |
| g232 | a9eb7bae7a5369d13 | 85 | A-578 publication live counter | editing volume.rs (field, constructors, seed, head walk) |
| g292 | ae2d631e0d0ae42ad | 91 | Copy (A-542 second half) | reading engine.rs state fields |
| g085 | a733238e9e4525fc5 | 88 | A-568 Windows links (Developer Mode) | relaunching its combined red+green run (r144/r240 never ran) |
| g291 | ae65da73712de9041 | 91 | A-569 Mercurial piece 2 | writing golden tests in tests/wire.rs |
| g083 | a97c265c4c940a705 | 87 | (A-567 B8 LANDED b7081c388) | next: goal 87 follow-up after main's r278 read |
| g261 | af8383414982392fe | 91 | A-575 repository verbs | reading verbs edit/declare bodies |
| g255 | a10eb14106b7fd19d | 86 | (A-570 jobs 1-2 LANDED 1f712cc28) | next: A-570 rest (hints+delegation drain, idle seal, huge-page re-arm, delete the tick) |
Not hit (were running/waiting at 02:45): g079 a85e896a5c342db18 (A-550 B12 part 2), g258 a10b9bbe6a9b11c83 (A-576), g234 aecd99fcadeea9958 (A-572), g250 ac3bd86db01a0a4ee (idle), g264 acae8dd97f35df13e (exited refusal), g262 ad2a5c489cf4041ae (microVM piece 2), g268 a4415bbe48bd53d22 (A-555), g251b a9c33df38afb7693a (A-571 part 1), g293 afc821446f68fe42e (A-520 piece 4). Check each with a SendMessage after the reset; most will have stopped too.
Main's runs to read after the reset: r122 (KVM microvm), r272 (TLC small, 6900 s bound), r278 (goal 87 A/A, all 8 runs), r296 (macOS microvm at kiln b7720ebca: the regression bisect).
- 02:45Z: limit reset early for this session; all 11 resumed by SendMessage (none refused). g083 -> A-579 (goal 87 server p99), g255 -> A-570 rest.
