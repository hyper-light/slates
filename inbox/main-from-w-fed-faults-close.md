
## C2 closed (goal 85c): live_chaos kills a writing client mid-write, and the next owner keeps or drops the write whole
- Commit on slates origin/main: c733506 (GAPS records the run; the flow itself was already on main, never run).
- Test: `SLATES_TEST_CLI=1 cargo test -p slates-cli --test live_chaos -- --exact
  a_client_killed_mid_write_leaves_its_write_whole_or_absent_through_a_takeover`, macOS arm64, Mac Studio, over slates
  main d4a5f9f43: 1 passed in 9.3 s. The client's write reached the owner and parked there (`fleet.live.awaited` moved)
  before SIGKILL; the owner's node was then lost; the successor served 2.6 s later with every acknowledged write whole
  and the killed client's absent or whole (NoLoss), its first head 21 past the last durable 19 under epoch 2
  (Continuity), every survivor reading its own bytes (ReadSafety). No code change needed.
