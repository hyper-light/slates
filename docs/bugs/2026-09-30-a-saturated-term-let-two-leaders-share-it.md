# A saturated term let two leaders share it (AUD-29-26)

## Description

- **The history.** Three voters are brought to a high term by a peer's term field or a publication.
- **The failure.** A campaigns and wins at `u64::MAX` with B's vote. B's timer then fires, and
  `start_election` saturated the term at `u64::MAX`, overwrote the vote with B itself, and sent vote
  requests at the same term. C granted, so A and B were both leaders of `u64::MAX`, which breaks
  Election Safety.
- **The same mistake at the log end.** `push_entry` noted the next index with `saturating_add(1)`, and
  the fast track chose `used.saturating_add(1)`, so a log ending at `u64::MAX` gave two entries one
  index.

## Root cause

- **Saturation was not the right answer.** `saturating_add` was used where the next value must be new.
  A term or an index with no successor must refuse, not repeat.

## Fix

- **Terms.**
  - `on_election_timeout` and `start_election` return `Result<_, TermExhausted>`, checked before any
    change; the refusal is counted as `terms_exhausted`.
  - A pre-vote reply matches `current_term.checked_add(1)`, and `TimeoutNow` at an exhausted term sends
    nothing.
  - The council and root group send nothing on an exhausted timeout.
- **Log indices.**
  - `push_entry` returns whether it appended, refusing at the last index (`indices_exhausted`).
  - A leader's append, membership change and window decision propagate the refusal.
  - A follower refuses an append that would run past the range whole, before touching its log.
  - The fast track refuses its proposal.
  - A publication past the range does not restore.

## Evidence

- **`a_term_with_no_successor_cannot_campaign_and_keeps_one_leader`.**
  - Red on the old `start_election`: B campaigned again at `u64::MAX`.
  - Green now: every later campaign, pre-vote and `TimeoutNow` is refused with nothing changed, and one
    leader holds the term.
- **Other tests.**
  - `a_pre_vote_at_the_maximal_term_is_refused_by_term`.
  - `an_append_past_the_last_index_is_refused_whole`.
  - `a_leader_at_the_last_index_refuses_new_entries`.

## Sibling

- **Still to sweep.** The regional and root configuration versions and the host fencing epochs
  (`register.rs`, `ledger.rs`, `takeover.rs`) still saturate. They are applied through the log, so their
  refusal must be a deterministic function of the replicated state; this is the follow-up change.
