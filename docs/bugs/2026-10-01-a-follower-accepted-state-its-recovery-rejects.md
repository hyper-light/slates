# A follower accepted state that its own recovery rejects

**Date:** 2026-10-01. **Area:** `slates-cluster` (`raft.rs`). **Audit:** AUD-29-38 (P1). **Design:** §4.8
status (A-57), §4.9, §4.13.

## Description

A follower accepted an `AppendEntries` at leader term one carrying an entry of term two and answered
success. Saving that state and restoring it at once failed `InvalidLogTerm`. The live handlers and the
retained-state validator (`SavedRaft::validate`) held different rules: the validator required entry terms
to be nonzero, nondecreasing and no later than the current term, voter sets nonempty and distinct,
snapshot boundaries consistent, and window slots positive, distinct and no later than the current term.
The handlers asked none of it before changing term, vote, log or configuration.

A valid Raft leader never sends such a message. The finding is about authenticated input and corruption,
not Byzantine consensus. A malformed message must still be refused before it poisons acknowledged
retention or forces an avoidable shutdown.

## Root cause

Validation existed only at recovery. Every handler trusted what it placed.

## Fix

- One admission check at the top of each handler, before any state changes, enforcing the validator's
  rules on what the message would place. The refusals form the closed `Malformed` taxonomy, each counted
  (`RaftNode::malformed`).
  - **AppendEntries:** term ≥ 1. A previous position whose index and term agree on being empty, with its
    term no later than the leader's. Entries with nonzero terms that never decrease from the previous term
    and are no later than the leader's, and with legal configurations. For an append anchored inside the
    committed prefix (which the handler re-anchors at the commit index), the first remaining entry is also
    held to this node's own term there.
  - **InstallSnapshot:** term ≥ 1, a boundary at a positive index with a term from one to the leader's, and
    a legal configuration.
  - **RequestVote and PreVote:** term ≥ 1, and a last position consistent with itself and no later than
    the campaign term.
  - **VoteReply:** window reports at distinct positive indices, accepted no later than the reply's term,
    each entry's term from one to that term, configurations legal. A won election recovers from these.
  - **AppendReply and InstallSnapshotReply:** no match past the leader's last entry.
- `SavedRaft::validate` and the admission share one voter-set rule (`valid_config`).
- **Sender binding:** already in place. `RaftMessage::decode_from` refuses a claimed sender that is not the
  authenticated session's peer (`ForeignSender`), and `decode_message` refuses another group's envelope
  (`ForeignGroup`).
- **No membership check on leader messages:** a follower that has not yet received the entry adding a
  voter must still accept that voter's appends when it leads, or it never catches up. Thesis §4.1, and
  `docs/bugs/2026-09-29-a-member-that-missed-its-promotion-refused-every-election.md` for the vote side.

## Tests

- `a_follower_refuses_an_entry_from_a_term_after_its_leaders`: the audit's witness. It failed before the
  fix (the append was answered success). Now it is refused, the retained state is unchanged, and the
  saved state restores.
- `every_admitted_message_leaves_a_state_recovery_accepts_and_a_refused_one_changes_nothing`: generated
  histories of up to 24 steps (appends, snapshot transfers, vote and pre-vote requests, vote replies with
  reports, append replies, won elections). Every field is drawn from small ranges that include the
  malformed values.
  - After every step the node's saved state restores, and a refused step leaves it unchanged.
  - The census meets all seven refusal kinds and an admitted step that changed retained state.
  - **Mutation:** with the append check disabled the test fails, shrunk to
    `Append { term: 0, …, entries: [(0, None)] }` → `InvalidLogTerm`.
- Every cluster test (239 unit and the integration suites) and every server unit test (144) pass
  unchanged, so valid traffic is admitted.
- The Raft safety explorer at full scale (`cargo test -p slates-cluster --release --test explore --
  --ignored`; 400 seeds × 4,000 steps × 2 sizes, every message reordered, dropped or duplicated, crashes,
  partitions, compaction, membership changes, the fast track) keeps Election Safety, Log Matching, Leader
  Completeness, State Machine Safety and fast agreement with the admission in place: 41.8 s, green
  (2026-10-01, Apple M5 Max, under a load average near 10 from other sessions).

## Siblings reported

- AUD-29-37 (recovery expanding a peer's reported index into unadmitted work) shares the vote-report
  path. The admission here bounds each report's fields, not the gap between reports and the log; that is
  37's fix.
