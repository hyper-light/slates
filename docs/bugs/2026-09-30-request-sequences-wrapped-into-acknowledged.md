# Request sequences wrapped into "already acknowledged" (AUD-29-21)

## Description

- **The wrap.** The client advanced its `u32` request sequence with `wrapping_add`.
- **What the daemon saw.** After `u32::MAX` the next sequence was 0. The daemon's completion window
  (`ClientWindow`, `sequence <= acknowledged_up_to`) met it as `Acknowledged` and never recorded it as
  new.
- **Other risks.** Reusing a sequence before its acknowledgement also risked meeting an earlier
  completion.
- **The session marker.** `Session::next_sequence` wrapped the same way, so a resumed client could
  reissue the last id.

## Root cause

- **Modulo arithmetic over an unbounded history.** The id word is a `u32` client and a `u32` sequence,
  and nothing refused the end of the sequence range.

## Fix (within the wire format)

- **The range.** `LAST_SEQUENCE = u32::MAX − 1` is the last sequence a client issues.
  - `Client::fresh_id` refuses past it with `ClientError::SequencesExhausted { client }`, before
    anything is sent. `call`, `begin`, the automatic acknowledgement and a rebind all draw ids through it.
  - `u32::MAX` is kept as the `Session::next_sequence` of a client that issued the last sequence, so a
    resume from it issues nothing.
- **Arithmetic.** The acknowledgement cadence and the unpublished watermark use `saturating_sub`, which is
  exact now that sequences never wrap.
- **The defined transition is a new client.** A fresh client id and window take new work, while retries
  of issued ids still reach their completion records.

## Evidence

- **`a_client_at_its_last_sequence_refuses_fresh_requests_and_keeps_its_retries`** (`crates/client/tests/client.rs`).
  - A client is resumed, through a daemon restart over one anchor-held segment, at the second-last
    sequence. Its acknowledgement takes that sequence and its create takes the last.
  - The next create is refused `SequencesExhausted`, the session carries `u32::MAX`, and a retry of the
    last id returns the original `Created` from its record.
- **Seeding.** A resumed client acknowledges first, so the seed is one below the last.
