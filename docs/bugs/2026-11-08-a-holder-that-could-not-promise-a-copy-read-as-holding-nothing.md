# A holder that could not promise a copy read as holding nothing

**Found.** Review of A-251's takeover rule (raft-chaos, 2026-11-08, after raft's `a-holder-restarted-warm-lost-its-copies-routing`).

**Root cause.** A host takeover counts a holder's complete answer as a promise for every object it did not list: listing
nothing means holding nothing (§4.8). `takeover::list_page` swallowed every refusal except a higher fence
(`Err(_) => {}`), so a holder whose copy is under a generation newer than the asker's answered a complete page, and the
asker counted that as an empty promise. The successor could then adopt an older record. It is a swallowed error
(banned item 9) and a NoLoss hole that needs no failure.

`Ok(None)` is not part of it: `Acceptor::prepare` reports the highest of the records it holds for that object, so `None` means
it holds none, and that empty promise is correct. A test pins this.

**Impact.** A takeover racing a generation change could adopt an older record than a holder held. No run has shown it.

**Edits.**
- `crates/db/src/register.rs`: `HostPromise::unpromised`, the count of objects this answer could not promise for any reason
  but a higher fence, on the wire.
- `crates/server/src/takeover.rs`: `note_refusal` records a refusal and counts `fleet.takeover.page_unpromised`; `page_of` does
  not call a page complete while `unpromised` is above zero.

**Tests.** `a_holder_that_could_not_promise_a_copy_has_not_answered_for_it` drives a real acceptor holding a newer generation's
record, red with today's completion rule. `a_holder_lists_every_copy_it_holds_and_only_those` pins the empty promise. The
wire round trip and truncation tests of `HostPromise` carry the new field.

**Not closed here.** This is step (c) of A-251. A page can still omit a held copy through `list_page`'s routing filter; the
counting rule and the per-object question are (a) and (b).
