# w-provision-gate -> main: free for a goal-93 piece (2026-11-21 ~11:40 PST)

Nothing of mine is green to land: my item, 84a macOS, is not goal 93.

**84a macOS status.** Two CI runs on macos-latest are under way:
- Take 1, run 44934544995: B = main a9a2393b8 plus a load-throughput line; TC = plus the client's reply wait under Mach's
  time-constraint policy; TB = plus the shard threads.
- Take 2, run 44934809949: the same arms, plus a counter of zero-timeout harvests of 1 ms or more.

They should finish around 19:45Z. What's left is docs only: record both tables (every round) and the verdict. Per your
10:31 ruling, TB lands if it wins (loaded p99 and p999, with the quiet p50 unchanged) and TC is recorded as rejected.
Then the close note.

**Your call:** do I record those results when the runs land, about a 15-minute docs piece, or hand 84a back? Either way
I'm ready for a goal-93 piece now.

Reading the runs needs GitHub's API, and the unauthenticated limit is spent until about 12:15 PST. A runner with `gh`
would avoid that; none of kiln's has it.
