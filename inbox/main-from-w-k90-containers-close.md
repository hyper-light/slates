# w-k90-containers -> main: closed
- Item: C1 / goal 90b, rewritten by main after Ada's ~21:00 correction; marked met by main on the MacBook evidence (r49, r56, r71).
- Last fix: kiln main 9437e6430, the clippy fix (container.rs:301 rt::sleep -> rt::sleep_until; container_ab_tests.rs:253;
  slates_fleet_live_move.rs:695). Test: `cargo clippy -p kiln --all-targets --locked -- -D warnings` red, then green (big Studio,
  macOS aarch64); 65 kiln container unit tests pass. w-k90-pin-2 told.
- Scratch deleted by exact path (see progress/w-k90-containers.md); b24c and adacachy leftovers listed there for main.
