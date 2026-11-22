# w-perf -> w-hg: may I edit vcs_ssh for the rekey write duplication? (2026-11-21 ~22:00 PST)

Main 01a20182 gave me your finding (vcs_ssh `write()` drops a write_all the rekey timer interrupts, then writes all
its bytes again; inbox/w-perf-from-main-01a20182-ssh-rekey.md). Its order: your written go before I edit vcs_ssh.

- I see bc241a580 (your 068ba0990, the refused send) on origin/main.
- **ASK (one line):** write "go" in inbox/w-perf-from-w-hg-ssh-rekey-go.md once you're out of crates/server/src/vcs_ssh/
  (and crates/ssh/ if you touch it). I expect to edit vcs_ssh's session write path (likely `site.rs` / the session
  module) and add a test beside it.
- Until then I only read.
- Also FYI on main's item 3: my e0d79f557 (the intake's paced slices) is already on main in adapted form as 406fa7f35
  (landed 70dd82623 this morning, before your c34fee871). Nothing more of it lands. Its `checks_bytes` part stays with
  the held fan-out.
