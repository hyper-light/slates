# A long CLI write let its attachment lapse (2026-11-23)

## Description
`a_freed_content_extent_gives_its_commit_back_on_windows` (`crates/cli/tests/content_extents_windows.rs`) makes a
volume's write attachment with `slates attach`, a process that then exits. It then writes 256 MiB through that
attachment with `slates write`, a second process. The file first ran in a lane in diag run 45260798132, and there the
write was refused `AttachmentExpired { attachment: 281474976710657 }` on windows-latest and windows-11-arm. The restart
test's 16 MiB write through the same flow passed. CI skipped the test by name as a tracked red, opened as A-878 and
renumbered A-879 when that id went to another change's design amendment.

The bug is not Windows-only. On macOS (2026-11-23), a 16 MiB input that reached `slates write` after an 8 s pause was
refused the same way, exit 1. The same input with no pause was written in 1 s
(`~/slates-work/w-win2-notes/a878-repro.sh`).

## Root cause
A CLI attachment outlives the process that made it for a term, renewed by every verb that names it (A-230, A-384).
`slates write` read all of stdin first. Then, for an input larger than one request, it staged the input in pages
(`StageBegin`, `StagePut`). Neither of those names the attachment. Only after that did it send `FsWriteStaged`, the
first verb that names the attachment. Nothing renewed the attachment between the attach and that verb. So an input that
took longer than the term to arrive, or a large one that took longer to stage, met a lapsed attachment. A fresh CLI
attachment's term is the daemon's failover delay, a few seconds (about three on a quiet laptop).

The holder that keeps an idle attachment alive already existed (`slates_client::Hold`, used by `slates attach --hold`),
but `slates write` did not use it. A holder must also know whether an attachment has a term at all. `Renew`, the only
verb that reported a term, refused an attachment with none (a mount's, an SDK client's) as a `BadRequest`, an
uncategorized refusal for a valid state.

## Impact
On every platform, a `slates write` whose input arrived later than the attachment's term, or whose staging took longer
than the term, was refused `AttachmentExpired`, and the user had to attach again and repeat the whole write. On
Windows, staging 256 MiB took longer than the term on both runners, so the extents file's commit give-back test was
red, and CI skipped it.

## Exact edits
- `crates/ipc/src/protocol.rs`: `ReplyBody::NoTerm` is appended (tag 85, published in `crates/ipc/tests/wire_order.txt`).
  `Renew`'s doc says it answers an attachment with no term with `NoTerm`, where it was refused `BadRequest`.
- `crates/server/src/verbs.rs` (`renew`): an attachment with no term is answered `NoTerm`. Nothing is renewed.
- `crates/client/src/client.rs`: `Client::renew` returns a `Renewal`: `Renewed { term_ns, renew_every_ns }` or `NoTerm`.
- `crates/client/src/hold.rs`: `Hold::new` returns `Ok(None)` for an attachment with no term, its renewer already ended.
  `Hold::end` stops and joins the renewer and returns the refusal that ended its renewals, which a drop discards. A
  renewal answered `NoTerm` after a term is a protocol break that ends the renewals typed.
- `crates/cli/src/verbs.rs`: `slates write` is served before the generic dispatch, with the instance its hold needs. It
  holds the attachment from before it reads stdin until the write is answered, logs any refusal that ended the hold,
  and reports the write's own outcome. A hold it cannot make ends it before it reads any input, with the hold's refusal
  named on stderr ("attachment N could not be held for the write, so none of its input was read: ...") and that
  refusal's exit code. `slates attach --hold` treats a `None` hold of the attachment it made in the CLI's form as a
  protocol break.
- `crates/cli/tests/cli.rs` and `content_extents_windows.rs`: `run_with_stdin` feeds the input beside its wait, and a
  feed the command cut short by ending first becomes part of the command's stderr rather than the test's panic, so a
  failed hold's message reaches the assertion. The stray write's test asserts that message.
- `crates/api/src/lib.rs`: the contract is 61, with its history line.
- Tests: `a_renewal_answers_no_term_for_an_attachment_with_none_and_renews_a_termed_one`
  (`crates/server/tests/attachment_term.rs`); `a_hold_of_an_attachment_with_no_term_is_none_and_an_ended_hold_returns_its_refusal`
  (`crates/client/tests/client.rs`); `a_write_whose_input_outlasts_its_attachments_term_holds_it`
  (`crates/cli/tests/cli.rs`, gated by `SLATES_TEST_CLI`). The existing term tests move to `Renewal`.
- `.github/workflows/ci.yml`: both Windows lanes run the commit give-back test again.
- `docs/cli.md`: `slates write` holds its attachment.

## Evidence
RED_AND_GREEN_PLACEHOLDER

## Siblings, reported
- The write's input is held whole in the CLI, up to the largest shard's metadata ledger, next to the daemon's staging
  buffer holding a second copy. Main ruled that the next piece is a stage opened without a length, reserved put by put
  within the ledger and bound to the attachment, with a peak-RSS red. The CLI's refusals of an oversize input and of a
  failed stdin read are still `BadRequest` strings. That piece removes both together with the buffer.
- A staging buffer expires one failover delay after its last put, as an attachment's term does, and no hold renews it.
  The length-less stage is bound to its attachment, so the same hold keeps both alive.
