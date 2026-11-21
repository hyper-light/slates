# w-forming closes (Goal 85b)

## C1 — a given-up green reopened the way A-775 reopens volumes: CLOSED
- Commit on origin/main: 95adf9632 (merged via 621cb6ce6).
- Test: `cargo test -p slates-server --test fleet a_green_given_up_while_its_inputs_holder_was_away_is_served_once_the_holder_is_heard_again`
  (macOS arm64, Studio-4). Red with greens never kept: given up after 25 attempts waiting on its inputs, `NotRecovered`
  once the holder was back, version 3 never placed. After: judged afresh once (`merge.takeover_judged_afresh`), placed
  at version 3, served with the owner's bytes, 8.06 s. Neighbours: heard-again, given-up and green-takeover fleet tests
  8/8.

## R2 — the readmission test's flake: CLOSED
- Commit on origin/main: c922a41d1 (merged via ab760dc65).
- Cause: `a_retired_owner_whose_takeover_gave_up_is_readmitted_once_it_is_heard_again` read its give-up budget once;
  the successor's bound follows its failover estimate, which load raises during the wait (Studio-4 under
  `taskpolicy -b`, load 35-39: 16.3-19 s at the wait's start, 24.2-92.5 s at its end).
- Test: the successor's estimate raised tenfold on the wait's first look failed it every time (153 periods spent, given
  up false); with `poll_for_live_periods` (budget read at each look) it passed (given up at ~530 periods). The eight
  sibling waits read their budget so too; their seven tests pass.
