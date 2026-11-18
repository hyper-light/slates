# Another principal's use renewed a CLI attachment's term

Found 2026-11-09 by fixtures' sweep, which the supervisor asked for after A-255. The sweep looked for any record found or
written by an identity that a request carries, rather than the one its session authenticated.

## Description

A CLI attachment (`Consumer::Termed`, A-230) holds a term: the write lease's, which is the failover SLO. Each verb that
names it renews the term once less than half is left, so a user at work keeps it. The renewal ran ahead of the verb's
own check that the caller is the attachment's principal. In `verbs::use_attachment`, called first by `detach` /
`detach_of`, `bind_mount`, `merge_service::advance` and `fs_verbs::with_writer`, any channel that named the id wrote an
`AttachmentRenewed` record and kept the attachment alive. The verb then refused it `Forbidden`.

## Root cause

`use_attachment` took the attachment id alone. It judged the term and wrote the renewal from the id and the clock, and
the principal check came after it in each caller.

## Impact

Another account could keep a lapsing write attachment of someone else's alive, and with it the volume's write lease, for
as long as it kept naming the id every half term. It could not read or write through the attachment, since every verb
still refused it `Forbidden`. It could also tell an expired id from an unknown one (`AttachmentExpired`): the partition
keeps expired ids, and a record that has left names no principal to judge against. That one remains, since it reveals
only that the id existed.

## Failing test

`a_cli_attachment_is_renewed_by_its_own_principals_use_alone` (`crates/server/tests/attachment_term.rs`).

- The owner attaches a volume for write in the CLI's form.
- A consumer enrolled under the same host account (another principal) names the attachment past half its term, by a
  write, a detach, a bind and an advance.
- The owner then writes through it past the first term's end, within the term a renewal would have given.

With the check out, the owner's write succeeded (adas-mac-studio.local-2, 2026-11-09, 9.95 s). After the fix, all four of
the consumer's uses are refused `Forbidden`, and the owner's write is refused `AttachmentExpired`.

## Exact edits

- `crates/server/src/verbs.rs`: `use_attachment(state, principal, attachment)`. It renews or ends only an attachment
  whose recorded principal is the caller's. Any other caller passes untouched to the verb's own check. Its four callers
  pass their principal.
- `crates/server/src/fs_verbs.rs` and `crates/server/src/merge_service.rs`: the callers.
- §4.7's A-230 status and amendment, GAPS.

## The sweep: where each identity is checked

| What | Found or written by | Checked |
|---|---|---|
| Completion records (local) | `(origin anchor, client, sequence)` from the request word | A-SEAT's door, `verbs::refuse_foreign_request`, refuses a word naming another client before `serve`, `BadRequest`, counted `client.foreign_request`; reply delivery keeps only the slot's own words (`send_reply`); the client id is proven by the session key (`session_key::proves`). Held through a takeover: `a_request_id_copied_into_another_session_reaches_no_completion_through_a_takeover` |
| Completion records (forwarded) | `(origin, client, sequence)` | `origin` is the forwarding peer's mutual-TLS identity (`serve_forward`), never a payload field, so keys are namespaced by node; the client is what that node's own door checked |
| Forwarded principal | the request's principal | `rehosted(origin)` (A-237): an account means something only on the authenticated node |
| Acknowledgement watermark | the session's `client_id` | `scatter_acknowledge` uses the slot's id |
| Attachments by id (`Detach`, `DetachOf`, `BindMount`, `Advance`, `Fs*`) | attachment id | the recorded principal against the caller's, plus `remote_attach::may_use`; the term's renewal now too (this record) |
| Direct maps (`MapSync`, `ReleaseMap`) | mapping id | looked up under the session's own client id (`direct_map::held_by`): another client's map is `NotFound` |
| Landing grants (`Land { grant }`) | grant id | the grant's binding includes the caller's principal key (`begin_granted`, `GrantRegistry::check`'s `unbound_field`); `Grant` itself needs the issuer's proof |
| Staged bytes (`StagePut`, `EditStaged`, `FsWriteStaged`) | token | keyed by (volume, principal, token) |
| Leases | none by id | a write lease belongs to its attachment |
| Observation subscriptions | none | `ChangedSince` and `Versions` are reads under the green's rights |
| `ShardsHolder` (Windows) | attachment id | a capability: the claim is judged against a holder's handle (`handle_route::claim`) and adds no authority |
| SDK, MCP, CLI | the client channel | each is a client of the daemon: `serve` takes the principal from the session's slot, and the door above checks the client id |
