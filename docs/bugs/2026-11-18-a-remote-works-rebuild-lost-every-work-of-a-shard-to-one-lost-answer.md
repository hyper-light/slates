# A remote work's rebuild lost every work of a shard to one lost answer (2026-11-18)

Contracts: §4.16 "Recovery" (A-247: a work replays its durable log through its green's owner once the partitions are
all recovered, and is unserved until it has), A-207 (every change a work acknowledged before a restart is there after
it), and the cross-shard asks judged in their target's own time (A-435), whose answer a target that stops for a window
loses after the ask ran (A-488, A-505).

## Description

Found by reading, as the third of A-488's siblings (GAPS, class A). At a daemon's start, before any client is
admitted, `daemon::rebuild_remote_works` replays each recovered work whose green lives on another partition: it asks the
work's shard for its list of such works, then for each work its log, the green's owner for the replay, and the work's
shard to install the outcome. Its docs accepted the loss: "a shard that does not answer leaves its works unbuilt, and a
retry of their verbs answers `NotFound`, as a work whose log would not replay does".

## Root cause

The list was asked with `std::mem::take(&mut s.work_home.unbuilt)`. An ask judged in its target's own time may lose its
answer after it ran (a target stopped for a window, or one that ran a window of CPU without answering, still runs the
ask once it resumes, after the caller has given up). The take had then emptied the shard's list, so every work on it
stayed unbuilt for the daemon's life. A lost answer to the log read or the owner's replay skipped that work the same way.
An install asked again, had the pass asked again, would have made the work afresh and charged its budget twice.

## Impact

Judged under §4.16, the acceptance does not hold. Nothing is lost from storage (the work's log and record stay), but a
work left unbuilt by a lost answer is unserved until the daemon's next start: its verbs answer `NotFound`, so every
change it acknowledged before the restart is out of its client's reach, which A-207 forbids, where A-247 makes a work
unserved only until it has been replayed. Its floor stays (`reconcile_work_floors` keeps the floor of a work it could
not rebuild), so its green's checkpoint cannot fold past the work's base. A log that does not replay is the work's own
state and stays refused and counted (`merge.work_rebuild_refused`); a lost answer is not the work's state.

## Exact edits

- `crates/server/src/verbs.rs`: `unbuilt_works` reads the list without taking it; `install_replayed_work` ends the
  work's record and installs the outcome once, and an ask that finds no record changes nothing.
- `crates/server/src/daemon.rs`: `rebuild_remote_works` reads the list, and `rebuild_remote_work` makes each ask through
  `ask_once_more` (a second ask behind the first on the target's channel, as A-505); the works whose asks were unanswered
  twice are counted in the recovery's log line and keep their records.
- `crates/server/src/work_home.rs`: `Unbuilt` derives `Clone` and `Copy`, its record documented.
- Design: §4.16's status after A-247's, the A-513 amendment; GAPS: class A's lost-answer siblings.

## Test

`a_remote_works_rebuild_asked_again_after_a_lost_answer_finds_the_work_and_installs_it_once` (`crates/server/src/
verbs.rs`): on a shard, a recorded work listed twice with the first answer lost, and its replay's outcome installed
twice with the first answer lost and the work marked between. It expects the second listing to name the work, the first
install to make it and charge it, the second install to charge nothing and keep the marked work, and nothing owed after.

## Still owed (closed by A-533, 2026-11-19)

A work whose asks go unanswered twice stays unbuilt until the next start, its verbs answering `NotFound` rather than
`Unpublished`; a retry after admission is not built (GAPS).

A-533: such a work's verbs answer `Unpublished` while its record stands (`work_home::unbuilt_refusal`), and that
answer is never recorded as the request's completion (`verbs::not_started`), so a resend under the same id runs the
verb once the work is rebuilt. As clients are let in, every shard asks its recorded works' green owners again
(`daemon::ask_again_for_unbuilt_works`, `verbs::ask_owners_to_replay`) by a message the owner runs when it next runs,
whose outcome comes back by a message of its own and is installed once: no timer, and nothing waits. Tests:
`an_unbuilt_works_verb_answers_unpublished_and_its_resend_runs_once_the_work_is_rebuilt` (red before: `NotFound`) and
`an_unbuilt_works_shard_asks_its_greens_owner_again_by_message_and_installs_it`.
