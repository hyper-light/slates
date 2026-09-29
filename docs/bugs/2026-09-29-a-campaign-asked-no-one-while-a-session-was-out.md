# A campaign asked no one while a session was out

Date: 2026-09-29. Scope: how a campaign's round borrows the other voters' record sessions
(`take_campaign_sessions`, `crates/server/src/fleet.rs`), for the council's and the root group's elections and
an invited campaign (leadership transfer). Found by the KIND lane's succession measurement
(`cargo xtask kind succession`; `docs/wip/kind-lane.md`, Piece 6), after the fixes of
`docs/bugs/2026-09-29-a-round-with-no-reply-yet-gave-up-at-its-lookahead.md` and
`docs/bugs/2026-09-29-a-yielding-voter-refused-the-voter-it-yielded-to.md`.

## Symptom

On KIND a central council leader's egress was cut. The central survivor's first pre-election often drew no
reply at all: neither a grant nor a refusal. The survivor then waited a whole election timeout to try again.
- Six trials on `2c6c034`: 3 had pre-elections that drew nothing, with successors at 3.50, 4.03 and 5.76 s
  against 2.01–2.40 s in the other three.
- In the earlier session's logged trials, the survivor's record link to the other survivor sat with no
  session and none lent to a dispatch, for at least 2 s.

## Evidence

The daemon was given counters for each voter a campaign's round could not ask, by why:
- no link (`fleet.election.no_link`);
- its session lent to a dispatch (`fleet.election.session_lent`);
- its session held by its link task (`fleet.election.session_held`).

It also counted each pre-vote grant dropped for arriving late (`fleet.election.late_pre_vote_grant`). Ten
KIND trials on `3f8733e`:

| Trials | Pre-elections by the successor | Voters not asked: session held by its link | Late grants dropped | Successor |
|---|---|---|---|---|
| 0 | 3 | 2 | 0 | 5.01 s |
| 1, 2, 3 | 2 each | 1 each | 0 | 3.45, 3.91, 3.52 s |
| 4–9 | 1 each | 0 | 0 | 1.58–2.77 s |

Every unanswered pre-election matches one campaign whose only live voter's session was held by its link
task. The other voter was the cut leader, whose link had gone at its retirement (`no link`, as designed).
No late grant was ever dropped.

## Root cause

A record session is lent exclusively. The link task takes its own session out of the link for a discovery
page, for one round trip, and a coordinator dispatch takes it for a round. `take_sessions` borrowed only the
sessions in their links at the moment of the call. A campaign whose only live voter's session was out at
that moment sent its round to no one. It had already begun its pre-election, so it then waited out a whole
election timeout before trying again.

A campaign's start and a discovery page to the one other survivor overlapped in 4 of 10 losses on KIND. The
counters show a page was in flight: no re-dials, no transport faults, no invalidations. They do not show why
it had started. A page to the 80 ms pod holds the session for about one round trip on that path.

The forward path met the same exclusive lending on 2026-09-25 and waits for the session
(`docs/bugs/2026-09-25-a-forward-refused-while-the-owners-session-was-out.md`). The election path did not.

## Impact

- Every consensus campaign: the council's and the root group's pre-elections, and an invited campaign.
- Whenever a voter's session was out at the round's start, that voter was not asked.
- A campaign that thereby asked no one cost a whole election timeout, 1–2 s on these paths. On KIND the
  median successor was 2.77 s where it is now 1.91 s.

## Fix

`take_campaign_sessions` waits for a voter whose session is out of its link:
- It waits for a session held by its link task or lent to a dispatch, paced at the round budget's poll
  interval, until the round's base deadline.
- A discovery page returns its session within the page's round trip. The base deadline covers that: it is
  at least the slowest voter's round-trip tail (`round_budget`).
- A voter with no link, retired or never dialled, is not waited for.

Each voter awaited and taken is counted (`fleet.election.session_awaited`), and each voter still unasked is
counted by why. The late-grant counter stays, to watch the other possible cause.

## Tests

- **Failing first:** `a_campaign_waits_for_a_voters_session_that_is_out_for_a_moment`
  (`crates/server/tests/fleet.rs`).
  - The council leader invites a named voter to campaign (a transfer). A test injection
    (`Daemon::inject_campaign_session_hold`) makes the other two voters' sessions count as out of their
    links for 30 ms at each of that voter's campaigns, as a discovery page holds one.
  - Before the fix every campaign asked no one, and the target had not led after three election timeouts
    (3.0 s against a 1 s timeout).
  - After it, the target leads alone in 0.089–0.130 s (5 runs). Its campaign awaited both sessions, and no
    voter was left unasked.
- The daemon's in-process fleet suite passes 57 of 57 (234 s).

## Measured on real pods (KIND, 2026-09-29)

Ten trials each, the same profile, a fresh fleet per trial, on the same machine with no other session's
load running:

| Daemon | Successor | Seconds to a successor | Voters not asked for a held session |
|---|---|---|---|
| `3f8733e` (counters only) | the central survivor, 10 of 10 | 1.58, 1.95, 1.95, 1.96, 2.35, 2.77, 3.45, 3.52, 3.91, 5.01 | 5, in 4 trials |
| with the wait | the central survivor, 10 of 10 | 1.54, 1.60, 1.69, 1.87, 1.87, 1.91, 1.93, 1.94, 1.95, 3.43 | 0 |

The one slow trial after the fix (3.43 s) drew a *refused* reply: the other survivor refused the first
pre-vote as leased. It had heard the cut leader up to a heartbeat later than the candidate, and the
candidate's randomized timeout fell inside that survivor's minimum-election-timeout lease (thesis §4.2.3).
The retry won. That is Raft's randomized retry, measured at 1 in 10 here, and it is left as designed.

## Siblings

- **The forward path** already waits (2026-09-25).
- **Replication rounds** (`drive_council_replication`, `drive_root_replication`) and the record plane's
  dispatches still take what is in the links. A round that misses a voter retries it next period (a heartbeat)
  rather than losing a timeout, and a commit needs only a quorum. They are unchanged.
- **The root cause, exclusive lending of one session per peer,** stands. The transport runs several
  exchanges on one session at once (`Endpoint::begin`/`drive`), so the links could share a session between a
  discovery page and a campaign instead of lending it out. That is a larger change to the record plane,
  recorded in GAPS.
