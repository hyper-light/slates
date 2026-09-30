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

## Sibling (closed in the follow-up change)

- **Configuration versions.** Every regional and root mutator checks `version.checked_add(1)` before
  anything moves: admit, retire, settle, confirm, admit/retire/promote a region, move a home. A change at
  the last version is refused with the existing "changed nothing" result. The refusal reads only
  replicated state, so every replica refuses the same entry alike, which keeps apply deterministic.
- **Host epochs.** `RegionalConfiguration::take_over` returns `Option<HostEpoch>` and is `None`, with
  nothing changed, when the dead host's epoch or the version has no successor. `Owner::take_over`
  refuses `TakeoverError::EpochExhausted`.
- **The server's takeover round.**
  - A holder's fence at `u64::MAX`, or a held record written at it, refuses the round, counted as
    `fleet.takeover.epoch_exhausted`.
  - The first epoch is a checked maximum over the held records.
- **Volume and lease epochs.**
  - A snapshot checks the volume's next epoch before its barrier or any effect, and a write lease
    checks its next epoch. Both are refused `Unsupported { feature }` (`volume.epoch_exhausted`,
    `volume.lease_epoch_exhausted`).
  - Recovery's head advance at an exhausted epoch is counted and leaves the head as recorded.
- **Tests.**
  - `a_configuration_at_the_last_version_refuses_every_change_unchanged`.
  - `a_host_at_the_last_epoch_cannot_be_taken_over`.
  - `a_takeover_above_the_last_epoch_is_refused` (ledger).
- **Suites.** `slates-db`, `slates-cluster` and `slates-server` pass on macOS.
