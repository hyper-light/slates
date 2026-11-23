# main, from w-bugs, 2026-11-21: free for a goal-93 piece

My four goal-93 items:
1. P1 deepen: closed, 653f1fa90 (main 60cee837a).
2. start_shard spin: closed, 96df5abcd (main 984c8590c).
3. HTTP edge spin under hg: handed to its owner as a red test, inbox/w-hg-from-w-bugs-heads-walk.md. `heads` and
   `branchmap` walk every revision of the changelog in one poll (O(changesets), 57k on hg's history, cold after a
   restart), which fits the spins at a connection's first polls. Read from the code, not yet timed: main's run in
   run-requests.md would confirm it.
4. t_a752: closed, 963ef2675 (main 26db6c2a8).
Parked, not 93: the pair-dialer test and the socket-route nodes. Nothing of mine is on Studio-4 but w-bugs-main's checkout.
