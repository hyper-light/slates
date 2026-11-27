# A made changeset over a let-go parent was never made, and its refusal named the wrong cause

2026-11-25, w-hg; main's ruling of 22:17 PST and its follow-up on the refusal's name.

## Description

A version a work submits to a Mercurial line's green is the changeset the server makes of it (A-731,
`vcs_store::hg_made`). The made commit's task takes the change's changegroup into a fork through the push's own intake
(`hg_push::take_made`), lands it on the node's changeset store, and commits the version with it.

In production that intake was abandoned whenever it wanted a text from the changeset store. That happens when a delta
base or a parent's revision had been let go once its push landed. So the version was never made its changeset. The
client was refused `NotHead`, "no changeset could be made of the version: NotHead", although its version was the head.

The test builds made the changeset, because a `#[cfg(test)]` arm answered the same want from the store with a blocking
read. That arm is a mode switch (CLAUDE.md §2, item 7), and it hid the bug.

## Root cause

`hg_push::taken_whole` took the changegroup in one step, every slice at once. On an intake's `Stepped::Store` it called
`stored_now`, which had two arms:

- **Production (`#[cfg(not(test))]`)** answered `false`, documented as "a made changeset's, which sends every revision
  whole (`slates_hg::push::made::changegroup`), so never asks; unanswered". The premise is false once a client's push
  has let a parent's texts go (goal 93, item 2b(b)). The intake then needs a base from the store.
- **Test (`#[cfg(test)]`)** answered the want from the store, blocking on the shard's thread
  (`test_store::answer_wants_blocking`).

On `false`, `taken_whole` abandoned the taking and returned `None`, and `hg_made::prepared` mapped `None` to
`Unmade::NotHead`.

Found when hg_made's tests moved off the shard-blocking allow-list (`test_store::SHARD_BLOCKING_ALLOWED`). With
`stored_now`'s allowance gone, the guard refused the test arm's blocking read inside the production task. The shard
thread died on the guard's panic, and the watchdog read the dead shard as "shard 2 is wedged … in task spawned at
crates/server/src/vcs_store/hg_made.rs:141, at its poll 5" (twice, load 67).

## Red, with production's answer

The test arm was made to answer `false`, as production's did (a throwaway edit), and the two tests that reach it were
run alone (`--test-threads=1`):

- `a_changeset_made_over_a_parent_let_go_reads_its_texts_from_the_store`: "the green's durable head at the second
  version, left: 1".
- `t_a731_a_submitted_version_is_the_changeset_the_server_makes`: "1 of 2 changesets made after 60s". Its client push
  now goes through the daemon's driver, which lets the parent's texts go.

A third red holds the refusal's name: `a_made_changeset_whose_store_refused_its_intake_is_refused_in_the_stores_words`.
It makes the next changeset over the first while the parent's texts are at hand, lets every changelog and manifest
object go, and stops the store's owner. Then it takes the made changeset's changegroup from a task, as the made commit's
task does (`hg_made::taken`). The intake asks the store for the parent manifest and is refused `Gone`. It answers
`Store(Owner(Gone))`. With this case mapped to `NotHead`, as before the fix, the test fails `Some(Err(NotHead))`.

A whole submit is no red for this, because its path never reaches the intake's ask with the owner gone. With the
parent's texts let go, the made commit's own parent read is refused first, `ParentUnread`. With them kept, the intake
asks nothing, the landing is refused `Gone` (`Unlanded`), and `made_commit::abandon` retries it without end (main's
ruling: its own piece).

## Impact

- **The commit was never made.** A submitted version over a parent whose texts were let go never became its changeset
  (A-731). Its submit was refused and nothing of it applied. In production that is any submit after a client's push
  whose texts the push let go.
- **The wrong answer was given.** The client was told its version was not the head, so it could not tell a store read
  that failed from a race it lost, and could not act on the true cause: an owner busy past its bound, or gone.

## Exact edits

- `crates/server/src/repository/hg_push.rs`:
  - `take_made` is now `async` and runs in the made commit's task. It takes the changegroup a slice a step, yielding
    between slices. An intake's store want is answered by awaiting the store, as a push's is (`take_sliced`'s
    `store_wants_answered`), with its refusal typed (`made_wants_answered`).
  - The take's end, `made_taken`, unpins and finishes in one step.
  - `MadeTake` (`Refused`, `Store(StoreUnanswered)`, `Gone`) is the take's refusal.
  - `stored_now` is deleted, both arms.
  - `taken_whole` is now `#[cfg(test)]` as a whole, the tests' `take_whole` only. Its blocking read is reached only
    under the allow-listed `audit_on_stored_shard` histories (hg_push's race histories, until they move), as its doc
    says.
- `crates/server/src/vcs_store/hg_view.rs`:
  - `StoreUnanswered` (`ShardGone`, `NoStore`, `Owner(OwnerError)`, `Missing`, `Unsized`) types why a store want went
    unanswered.
  - `answered_store_wants_on` answers with it.
  - `answer_store_wants_on` is its bool form, unchanged for its callers.
  - `answer_one` and `texts_made` return the typed refusal in place of `None`.
- `crates/server/src/vcs_store/hg_made.rs`:
  - `Unmade::Store(StoreUnanswered)` is the store's own refusal, never `NotHead`.
  - `landed` takes the changegroup by awaiting `take_made` (`taken`), then makes the changeset in one step
    (`prepared`, which now takes the fork).
  - The intake's own refusal is logged with its words.
- `crates/server/src/vcs_store/hg_made/tests.rs`: the red above.
- In the same change, hg_made's 10 tests move off the shard-blocking allow-list. Their repositories are made from the
  test's thread (`create_on`), and the client's push on a made changeset goes through `hg_push::unbundle` from a task
  (`pushed_changegroup`). So `land_in`'s and `stored_now`'s entries are gone (list 7 → 5).
- `docs/wip/SLATES_DESIGN.md`: A-731's status names the awaited take.

## Siblings

Swept and reported to main, not fixed here: every `#[cfg(test)]` arm inside a production function of
`crates/server/src` and `crates/hg/src` whose answer differs from production's (main's order of 2026-11-25, item 5).
Main's rulings set the follow-up pieces, each landing after this one:
- `crates/hg/src/landed.rs:159` (A1, fixed 2026-11-26). `Replaying::step` answered an intake's store want from the
  fork's own texts in tests (`replay.answer_from_fork`) and refused it `RecordRefused::StoreWanted` in production.
  - The seam keeps no store, and a whole replay reads every base it holds from the fork's rows, so a want comes only
    for a base whose bytes were let go. For such a base the fork has nothing either: the test arm answered
    `UnknownDeltaBase` where production refused `StoreWanted`.
  - No test reached the arm: with production's arm in place, `slates-hg --lib` passed 472 of 472, rc 0. And no
    production code drives `Replaying`; the server replays through `record::Replay` and awaits its wants.
  - Fixed: the test arm is deleted, so the tests run production's refusal.
  - Red: `a_replay_wanting_a_base_let_go_between_its_changesets_is_refused_as_production_refuses_it`
    (`crates/hg/src/landed_tests.rs`). Two pushes of one file land as two changesets, the second a delta on the first,
    and the first revision's bytes are let go between them. It expects `Record { sequence: 1, StoreWanted }`. With the
    test arm put back it fails `Record { sequence: 1, Intake(Unverified(UnknownDeltaBase { .. })) }`.
- `crates/hg/src/import/record.rs:1834` (A2, fixed 2026-11-26). `Retaken::step` answered an intake's store want from
  the fork's own texts in tests and left it pending in production. A waiting replay answers a pending ask through
  `Replay::store_wants`; a whole `record::replay` refuses it `StoreWanted`.
  - In tests a whole replay never saw the refusal; for a let-go base it answered `UnknownDeltaBase` instead.
  - No test reached the arm: with production's arm in place, `slates-hg --lib` passed 473 of 473, rc 0.
  - The server builds slates-hg without `cfg(test)`, so only the hg crate's own tests were ever bent.
  - Fixed: the test arm is deleted.
  - Red: `a_whole_records_replay_wanting_a_base_let_go_is_refused_as_production_refuses_it`
    (`crates/hg/src/push/intake_tests.rs`). One file's first revision is pushed and its bytes let go, then a second
    push, a delta on the first, is replayed whole from its record. It expects `StoreWanted`. With the test arm put back
    it fails `Some(Intake(Unverified(UnknownDeltaBase { .. })))`.
- `KEEP_LANDED` (`crates/server/src/vcs_store.rs:2654`, `:3958`): set by every default stored-test runner, so those
  tests never ran production's let-go. The runners stop setting it; tests that must keep packs resident set it by name.
- The gone owner (fixed 2026-11-26). Every landing the store refused answered the submit `Unmade::Unlanded`, which
  `made_commit::abandon` turned into `Overloaded`, unrecorded, so its retry met a gone owner again without end.
  - Fixed: `hg_made::landed_patiently` asks again after a yield only while the owner is busy, within the fleet's live
    deadline. Any other refusal, or a busy owner past that deadline, answers `Unmade::Landing(OwnerError)`, refused in
    the store's words and recorded.
  - `Unlanded` is left for a made commit whose task could not start.
  - Red: `a_made_changesets_landing_on_a_gone_owner_is_refused_in_the_stores_words_and_recorded`
    (`crates/server/src/vcs_store/hg_made/tests.rs`). Landed packs are kept so nothing reads the store before the
    landing, the owner is stopped, then a second submit is made. It expects a recorded `StoreRefused` naming
    `Landing(Gone)`. With the old mapping it fails "request 2 unanswered after 60s".
