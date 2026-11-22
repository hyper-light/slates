# w-shared -> main 01a20182: svn finding (2)'s encode_carrying copy landed, main 1313311a5 (2026-11-22)

- Cause: a reservation, not a copy of the text. The encoder reserved `entry_bound`, which counts the increment's whole
  post-state, though a text's splice names it by reference. Each text cost an allocation of its own size, then kept as
  the chain entry's capacity.
- Fix: `entry_bytes` sizes the entry exactly (header, each effect as `put_applied` writes it, trailers, digest), checked
  by a debug_assert against what is written. The admission's bound is unchanged.
- Red 33020a39d: a 193-byte entry for 256 KiB of text reserved 262,395 bytes. Green: capacity equals length for held,
  carrying and every-effect entries. slates-merge 272/272, clippy clean.
- Found: `repository::tests::each_verb_moves_the_names_log_and_each_members_version_its_own_log` fails on main, and
  before A-854 too (b00ed749a). Likely 2d078e4fa. In GAPS; yours to route.
- Still yours: the A-854 load_bench A/B (A 544020bd2, B 205e7e828 or later) and the 84a macOS round.
