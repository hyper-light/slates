
## C2 closed (goal 85c): live_chaos kills a writing client mid-write, and the next owner keeps or drops the write whole
- Commit on slates origin/main: c733506 (GAPS records the run; the flow itself was already on main, never run).
- Test: `SLATES_TEST_CLI=1 cargo test -p slates-cli --test live_chaos -- --exact
  a_client_killed_mid_write_leaves_its_write_whole_or_absent_through_a_takeover`, macOS arm64, Mac Studio, over slates
  main d4a5f9f43: 1 passed in 9.3 s. The client's write reached the owner and parked there (`fleet.live.awaited` moved)
  before SIGKILL; the owner's node was then lost; the successor served 2.6 s later with every acknowledged write whole
  and the killed client's absent or whole (NoLoss), its first head 21 past the last durable 19 under epoch 2
  (Continuity), every survivor reading its own bytes (ReadSafety). No code change needed.

## C1 closed (goal 91): the multi-runner test with injected faults
- Commits on kiln origin/main: 30c5929 (A-580 harness), e33c470 (A-708 relay, bind), 0edd8dc + e3e5d7b (A-734: one
  fleet, kiln founds its groups once), ed8991d (A-750: workspaces live-shipped and shared with every member), 5bd0de7
  (flows on macOS: runners and mounts let go before a signal), 648c571 + 4f19751 (headroom off; starts asked again),
  73712cc (the move-cut flow over the relay). Slates origin/main: 51a061d + 8d0e137 (A-764: credentials are fleet state,
  with tombstones), a5140de (fleet.rs:4819).
- Test: `SLATES_FLEET_FAULTS=1 cargo test -p kiln --test slates_fleet_faults -- --exact <flow> --nocapture
  --test-threads 1`, three kiln runners of a test's own server forming kiln's Slates fleet at f = 1, the agent's
  workspace live-shipped and shared by kiln alone. Every flow PASSED on macOS arm64 (Mac Studio, local):
  killed 43.5 s, partitioned 46.2 s, restarted 32.6 s (base 2f589baa6); runner cut off 35.0 s (base ed8991d); slow
  lossy link 43.3 s, its move refused by kind unplaced (base e3e5d7b); move whose target is cut off midway 84.2 s, refused
  by kind not_a_member (base eb7194f00). Each: every acknowledged file served byte for byte through the survivors during
  the fault and by every node after, the move moved or refused by a W7 kind, one fleet re-formed.
- Linux x86_64 (adacachy): killed PASSED 332 s, restarted PASSED 132 s (base 715063153), partitioned PASSED 128.5 s (base
  c0f576eb3). The link flows and the relayed move-cut on Linux are main's lane (run-requests.md); adacachy swapped
  104 GB of 122 GB today, so a kiln build there took over an hour.
