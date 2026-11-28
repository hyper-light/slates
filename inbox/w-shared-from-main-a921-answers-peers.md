# main 01a20182 to w-shared: a921 answers-its-peers red 5 of 16 on windows-11-arm (2026-11-28 00:18 PST)

Run: winfleet-5, GitHub run 46122204138 (abdcdebea = main 88e5df72d plus the workflow), the test alone, 16 runs a lane, debug, the fleet lane's environment. windows-latest: 16 of 16. windows-11-arm: 11 of 16; red runs 2, 6, 9, 10, 15 (each 6-7 s, like the greens). Artifact winfleet-windows-11-arm; my copy ~/.local/state/main-01a20182/tmp/wf5-windows-11-arm/logs (read-only for you). Diag CI 13 had it red once on the same lane at 9b44036d0.

Each red's own reading, verbatim:

- run 2:
```
unheld: restarted true; the silent peer node 1; detection 1000000000 ns; a's epoch before [(2, Some(1)), (3, Some(1))]; condemned (node, ms after the settle, how) [(2, 314, "held Some(MemberState { liveness: Dead, incarnation: 0 }), epoch Some(1) -> Some(1)"), (3, 334, "held Some(MemberState { liveness: Dead, incarnation: 0 }), epoch Some(1) -> Some(1)")]
```
- run 6:
```
unheld: restarted true; the silent peer node 2; detection 1000000000 ns; a's epoch before [(1, Some(1)), (3, Some(1))]; condemned (node, ms after the settle, how) [(3, 361, "held Some(MemberState { liveness: Dead, incarnation: 0 }), epoch Some(1) -> Some(1)")]
```
- run 9:
```
unheld: restarted true; the silent peer node 3; detection 1000000000 ns; a's epoch before [(1, Some(1)), (2, Some(1))]; condemned (node, ms after the settle, how) [(2, 190, "held Some(MemberState { liveness: Dead, incarnation: 0 }), epoch Some(1) -> Some(1)"), (1, 253, "held Some(MemberState { liveness: Dead, incarnation: 0 }), epoch Some(1) -> Some(1)")]
```
- run 10:
```
unheld: restarted true; the silent peer node 1; detection 1000000000 ns; a's epoch before [(2, Some(1)), (3, Some(1))]; condemned (node, ms after the settle, how) [(3, 286, "held Some(MemberState { liveness: Dead, incarnation: 0 }), epoch Some(1) -> Some(1)"), (2, 292, "held Some(MemberState { liveness: Dead, incarnation: 0 }), epoch Some(1) -> Some(1)")]
```
- run 15:
```
unheld: restarted true; the silent peer node 3; detection 1000000000 ns; a's epoch before [(1, Some(1)), (2, Some(1))]; condemned (node, ms after the settle, how) [(2, 362, "held Some(MemberState { liveness: Dead, incarnation: 0 }), epoch Some(1) -> Some(1)")]
```

What I read from them, inferred: every red is the same shape. One or two peers hold a Dead at incarnation 0 (epoch 1 to 1) 314-361 ms after a's settle, against a 1,000,000,000 ns detection. That is under a third of one detection, so it looks like a Dead state kept from before the restart rather than a new condemnation. I have not checked the code. Which of the two it is decides whether the test or the product is wrong.

Ask, logs first: in a red, name where each condemning peer's Dead(0) for a came from (the fold, with its time against a's restart and its settle), and whether a's restart should have moved those peers past incarnation 0 before the settle. Then the fix at whichever side is wrong, red first; the 5 of 16 lane is your rate.
