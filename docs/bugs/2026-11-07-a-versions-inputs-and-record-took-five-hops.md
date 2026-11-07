# A version's inputs and record took five hops

Found 2026-11-07 while measuring A-235 against goal 4's 1 ms bound. A single submit at `f = 1` was answered in 6.7 ms
at p50 on a quiet adacachy (AMD Ryzen 9 9950X3D2, Linux 7.2.9-2-cachyos), release build, 500 sequential submits on a
settled three-node fleet. It took 15 ms at load 130.

## Cause, from the commit's own timestamps

Temporary prints (never committed) timed each step of the commit in the 20-submit f=1 fleet test, release build, load
about 130. Microseconds, median over 20 versions:

| step | median |
|---|---|
| submit verb → merge record enqueued | 47 |
| enqueued → the nudged record plane starts the inputs put | 2,928 |
| inputs put (`put_content`: an offer, then the chunk transfer) | 6,166 |
| put ended → record ship starts (a second nudge) | 3,085 |
| record ship (`commit_record_on`) | 3,097 |
| ship ended → answer | about 0 |

The work itself was negligible. A version crossed five hops: the nudge, the offer, the transfer, the second nudge
and the ship. Each cost a wake and an exchange, about 3 ms at load 130 and about 1 ms quiet.

## Failing test

`a_merge_version_reaches_each_holder_with_its_inputs_in_one_exchange` (`crates/server/tests/fleet.rs`). Its setup is a
settled three-node fleet at `f = 1` and twenty works over one green, submitted one after another. It expects every
submit accepted, and every version's inputs carried to a holder inside its merge record (`merge.inputs_carried`).
Before the change, the holders answered 43 and 46 separate content puts for the 20 versions, and the counter did not
exist.

## Exact edits

- `crates/cluster/src/lib.rs`: `commit_record_sent_as`, the record commit with the bytes each holder is sent chosen by
  the caller. The acknowledgement still binds the record alone. `commit_record_on` calls it with the record's encoding.
- `crates/server/src/merge_service.rs`:
  - A version's record ships with its inputs while a target lacks them (`carries_inputs`). The message is a shipment
    (`encode_shipment`, `decode_shipment`): the record, then the content exchange's offer and every chunk of the inputs.
  - `put_inputs` and `MergeWork::PutInputs` are gone. "Placed before referenced" now holds by construction: a holder
    accepts a merge record only once it holds the inputs it names, so a record that commits at `f + 1` has its inputs
    held at `f + 1`.
  - An acknowledgement places the inputs as well as the record (`fold_acknowledgements`).
  - `merge.inputs_unplaced` still counts each ship that carries inputs not yet placed.
- `crates/server/src/fleet.rs`: a holder serves a shipment (`serve_merge_shipment`). Its content frames go through the
  content plane's own path (`serve_content_request`, the content stream's handler factored out, with the authority,
  retention and verification it always had), then the record is accepted. The holder counts `merge.inputs_carried`.
- Unit tests: when a record carries its inputs, the shipment round trip, and hostile shipments (truncated, a count of
  `u32::MAX`, a frame length past the bytes, bytes left over).
- §4.16 status, A-244, GAPS.

## Verification

Release, adacachy, 2026-11-07. Together with the retention fix
(`2026-11-07-retention-decoded-every-merge-record-per-put.md`), 500 sequential submits at `f = 1` at load about 10 gave
p50 265 µs, p99 731 µs, p999 and max 1.13 ms. Before, at load 29: p50 6.7 ms, p99 34.7 ms, max 62.4 ms. With this
change alone, the first 50 submits' median was 247 µs, and per-version retention cost dominated after that.
