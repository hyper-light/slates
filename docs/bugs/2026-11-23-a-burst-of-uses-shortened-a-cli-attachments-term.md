# A burst of uses shortened a CLI attachment's term below its holder's earlier silence (2026-11-23)

## Description
A-879's CLI flows on the Mac, with the write's hold in: `a_cli_attachment_outlives_the_process_that_made_it` attaches a
volume with `slates attach --write`, waits 2.5 s, writes under the attachment from another process, waits 2.5 s, and
detaches. The detach was refused `AttachmentExpired { attachment: 1 }`. The same test passed before A-879, and in one
earlier A-879 run on the Mac and on ubuntu: it depended on timing.

## Root cause
A CLI attachment's term is its holder's pace (A-384): `min(F, the tail of the gaps between its uses + W)`, the tail
RFC 6298's `smoothed + 4·deviation`. That estimator follows its latest samples. Before A-879 a write was one use, 2.5 s
after the attach, so the term stayed near `F` (about 3 s on the Mac). A-879's hold renews at the write's start and
the write's own verbs follow within milliseconds: a burst. The burst's millisecond gaps drove the tail, and the term,
toward `W`, so the attachment lapsed sooner than its holder had just been silent, and the 2.5 s pause before the detach
outlasted it. Any quick run of verbs did the same before A-879; A-879 made every write one.

## Impact
On every platform, a CLI attachment used by a burst of verbs, now every `slates write`, could lapse a liveness window
after the burst, though its holder had shown it keeps silences of seconds; the next command met `AttachmentExpired` and
the user had to attach again.

## Exact edits
- `crates/server/src/state.rs`: `AttachmentPace::longest_gap_ns`, the longest gap between two uses since the pace
  began, updated in `used`.
- `crates/server/src/verbs.rs`: `attachment_term` is `min(F, max(tail, longest gap) + W)`.
- `crates/server/tests/attachment_term.rs`: `a_burst_of_uses_does_not_shorten_a_term_below_its_holders_earlier_silence`.
- `crates/cli/tests/cli.rs`: `a_cli_attachment_outlives_the_process_that_made_it` pauses half the failover delay the
  daemon reports (`failover_delay_ns`, which the late-input test reads too), where a fixed 2.5 s pause bet on `F`.
- SLATES_DESIGN, A-384's record: its floor.

## Evidence
- Red (diag run 45309657568, 2026-11-23, ubuntu-latest and macos-latest; the test daemon's `W` 1 s, its failover
  delay held at 4 s): `a_burst_of_uses_does_not_shorten_a_term_below_its_holders_earlier_silence` with the floor
  reverted failed on both runners. After a 2.5 s silence the burst's terms fell to 1,356,596,976 ns (macOS
  1,357,744,563 ns), below the silence, the first fifteen at the 4 s ceiling and the rest falling.
- Green (the same run): the burst's terms ran from 3,500,583,548 ns (macOS 3,507,668,000 ns), the silence and `W`, to
  the ceiling, and the write after a shorter 2 s silence was accepted.
- The holder's own pace is unchanged (the same run):
  `a_held_attachment_lives_while_its_holder_renews_and_lapses_at_its_own_pace` named 1.001 s, rose to 1.907 s
  (macOS 1.908 s) while the estimator's deviation warmed, and fell to 1.439 s by the 24th renewal on both, toward four
  thirds of `W` (1.333 s). Its late write, two last terms (2.88 s) after the renewals stopped, was refused
  `AttachmentExpired`, before the ceiling.
- `attachment_term` 6 of 6 on both runners (40.9 s, 41.6 s), the library's holds 2 of 2, and the CLI's four attachment
  flows 4 of 4 on both (20.5 s, 27.5 s): `a_cli_attachment_outlives_the_process_that_made_it`,
  `a_write_whose_input_outlasts_its_attachments_term_holds_it`,
  `attach_hold_keeps_a_cli_attachment_alive_until_its_process_ends` and
  `the_anchor_supervises_a_daemon_the_verbs_answer_and_the_daemon_leaves_with_the_anchor`.
