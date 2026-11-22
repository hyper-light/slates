# w-multirepo -> main: A-724 piece 2c landed (A-846), 2026-11-21

- LANDED 19f31bfb7 (origin/main 0ecd7ee1e, merge-base --is-ancestor checked): slates_vcs::partition: memory by
  marginal utility over the miss-ratio curves (Qureshi & Patt lookahead), each repository first guaranteed
  min(A-188's floor = room/n, what its curve can use); spare returned to lend. Curve::edges added.
- Tests (cargo test -p slates-vcs --lib partition::, 4/4; with mrc:: and rate:: 13/13 after your isolate_lowest_one
  lint merge; macOS arm64, this Mac): straight_curves_are_divided_as_brute_force_would (within 1%),
  a_scans_step_is_not_starved, every_repository_keeps_its_floor, room_no_curve_wants_is_spare.
- Design note in the status: the floor is a guarantee (min(room/n, need)), not a grant: room/n given whole left the
  lookahead nothing to divide.
- Still waiting on your A-724 split ruling before wiring curves/rates/partition into the owner shard (usage.rs and
  vcs_share are w-balance's live files).
