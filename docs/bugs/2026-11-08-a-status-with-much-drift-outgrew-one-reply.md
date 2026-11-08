# A status with much drift outgrew one reply

**Found** 2026-11-08 by the sibling sweep of `2026-11-08-a-volume-listing-outgrew-one-reply.md`: a volume's `status`
carried its whole drift list (every overlay entry whose disk no longer matches, or whose re-check the host refused).

**Reproduced by use** (`crates/server/tests/drift_pages.rs`: an overlay of 600 base files, pinned, every file changed
on disk): `status` was refused `BadRequest { reason: "reply too large for the bulk area" }` once the drift was seen. An
operator could not ask a drifted volume's state at all.

**Fix (A-267, as the supervisor chose: the reply carries what fits plus a count, and a paged read).**
- `status` stays one reply: its `drifted` holds the first entries, in path order, that fit beside the report's other
  fields, and `drifted_elided` counts the rest (`with_drift`).
- `Drifted { volume, from }` reads the whole list a page at a time from a position, `Drifted { entries, next,
  oversized }`; an entry too long for any page alone is passed over and counted. `Client::drifted` walks it.
- Every reader says which it holds. `slates status ID --drift` walks the paged read when the report elided any. The text
  form's `drifted: N` counts them all, and MCP's `status_json` and both SDKs carry `drifted_elided`. Both SDKs had
  documented `drifted` as the full list.

**Tests.** `a_volume_with_more_drift_than_one_reply_holds_still_answers_its_status` (`drift_pages.rs`): the status
answers, its entries plus its count make 600, and the paged read holds all 600 once, the status's its first.
`status_drift_names_every_drifted_entry_past_one_reply` (`crates/cli/tests/cli.rs`, by use): `--drift` names 600 and
the text counts 600. Red with the CLI printing the report's own list: "every drifted entry, once: 109 named".

**Owed:** an SDK binding for the paged drift read (the SDKs carry the count and the first entries).
