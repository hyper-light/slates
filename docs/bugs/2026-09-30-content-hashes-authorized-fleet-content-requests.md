# Content hashes authorized fleet content requests (AUD-29-45, in part)

## Description

The fleet's content plane answered any enrolled host by content hash alone.

- **Offer.** `ContentHold::serve` answered an offer's missing set against the holder's whole chunk store,
  so a host could learn whether guessed chunks existed for any object.
- **Fetch.** A fetch named only a manifest hash and returned its whole archive.
- **Put.** A put was held without checking that the sending host had authority over the object it named.
- **Leaning on other objects.** A put could lean on chunks held for other objects without shipping them.

## Root cause

- **No authority check.** Mutual TLS identifies a host, but nothing bound a content request to the
  object it concerns or checked the host's authority over it.
- **Global lookups.** The hold answered from one global store.

## Fix

- **Per-object hold (`slates_cluster::content::ContentHold`).**
  - Each object's manifests are held with per-object chunk references. `missing_of`, `hold`, `archive_of`
    and `forget_manifest` take the object.
  - Chunk bytes stay once in the shared store, one store reference per object holding them.
  - `serve` takes the authority decision as a closure over `(ContentAccess, ObjectId)` and asks it before
    any lookup. A refusal is counted (`unauthorized`) and draws the same empty reply an unheld request does.
- **The wire.** `ContentMessage::Fetch` carries its object.
- **The server's predicate (`fleet::content_authorized`).**
  - The peer must be a member.
  - The acting owner is the owner this holder's records name (else the object's creator), or that owner's
    takeover successor once it departed.
  - A placement needs the acting owner as peer and this node among its candidates for the object.
  - A read needs the acting owner, one of its candidates, or a host of the departed owner's recovery cohort.
- **Callers.** Materialization, merge inputs and the daemon's test hooks name their object.

## Evidence

- **`another_objects_content_is_neither_revealed_nor_lent`.** One object's content is invisible to
  another object's offer, put and fetch; the bytes are stored once; forgetting one object's copy keeps
  the other's.
- **`a_refused_authority_draws_the_empty_reply_and_is_counted`.** The check is asked with the right access
  and object, nothing is stored, and three refusals are counted.
- **`content_authority_follows_the_objects_owner_and_placement`.** Owner, candidate, outsider, non-member,
  and the successor after a takeover.
- **Suites.** The cluster suite passes, the server suite passes (12 binaries; fleet 59/59, including
  content replication and takeover content serving).

## Owed

- **Consumer scope across hosts.** The requesting consumer's scope is not yet carried between hosts; that
  comes with the fleet delegation of §4.13. Consumers are separated on the requesting host, whose verbs
  check access lists before any content request.
- **Timing residual.** A put of a chunk already held for another object skips one allocation, a
  sub-microsecond difference masked by the verification of every shipped chunk.
