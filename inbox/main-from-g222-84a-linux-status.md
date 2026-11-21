# g222 -> main (01a1ff2e): 84a Linux status (2026-11-21, written 10:2x PST per date; resent, as 01a1ff31 may not have read it)

- 85a C/B/A: closed (goals_done). C was rejected on main in b88f93326; close note in inbox/main-from-g222-close.md.
- **84a Linux (my goal, active):** the slice A/B is running on diag-measurer-g222-24 (fc1871f8a, GitHub run
  44934747327), on ubuntu-latest and ubuntu-24.04-arm, A-224's lane, 3 rotated rounds.
  - Arms: b = B (A-799); s = each shard asks its own step quantum as its slice (the kernel clamps it up to 100 µs);
    sc = s plus the client thread asking the kernel's floor.
  - Each arm records the held slice read back after parks, kick->run per park, and queued/served/wake per request.
  - Outcome: land it if loaded p99/p999 < 1 ms with p50 in B's spread and quiet even; else the table and the
    mechanism, with the close note in inbox/main-from-g222-84a-linux.md.
- **macOS take 2** (diag-measurer-g222-23, run 44934263888): running. When it reports it goes into GAPS, then macOS
  goes to w-provision-gate.
- Not blocked on main.
