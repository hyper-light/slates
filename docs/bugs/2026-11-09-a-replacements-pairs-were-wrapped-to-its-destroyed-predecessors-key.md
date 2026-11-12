# A replacement's pairs were wrapped to its destroyed predecessor's key

**Status:** fixed 2026-11-09 (live-durability, `agents/live-durability-recipient-1`). One failing test showed it before
the fix: `fleet::tests::a_recipient_learned_from_a_predecessor_is_not_used_for_its_replacement`.

**Severity:** a sealing defect on main since pair delivery (A-92 piece 4b). A node replaced with fresh RAM at its anchor
could open no pair key its survivors sent it, for as long as they ran. On main this stayed hidden while the
replacement was not needed. When it was ranked a live volume's successor, it took the volume over but could not open
its key: mode A of the store-destroyed takeover flow (1 to 2 of 6 release runs on the Mac). On a tree that counts a
pair only once the current member accepted it (A-271), every write to a volume the replacement was a candidate of
waited instead (48 of 48 runs).

## What happened

A node's **stable anchor** is its certificate's hash, and its member id is derived from the anchor and a boot nonce
(`deploy::member_id`). A node replaced with fresh RAM (its store destroyed, the same certificate) therefore keeps its
anchor, gets a new member id, and makes a new sealing recipient.

An owner's shard delivers each candidate its pair key wrapped to that candidate's recipient public key. It asks the
candidate for the key once and keeps it in `ShardState::peer_recipients`. That cache was keyed by the anchor alone, and
nothing evicted it. `pairs_delivered` did notice the new member and owed it the pair again, but each delivery was
wrapped to the destroyed node's key. The replacement's `seal_keys::accept_pair` could not unwrap it and refused it:
every delivery, for good.

## Evidence

diag-live-durability-72 (A-271 with council-wait-1 and A-272 merged, CI, both arches, 12 runs per leg): 48 of 48 runs
stalled after the replacement rejoined. Before the owner was killed, the stream could not make 40 more writes durable
(`live_chaos.rs:1413`). Every run's dump read the same way:

- **The replacement:** `fleet.seal.pair_served` 1,583 and `fleet.seal.pair_refused` 1,583, so it refused every
  delivery it was sent.
- **The owner:** `pair_unanswered` and `pair_undelivered` about 1,580 each, and writes held back for keys
  (`fleet.live.held_back.keys`, about 800). The volume was moving from {c, the destroyed b} to {c, the destroyed b,
  the replacement}.
- **The other survivor:** its own deliveries to the replacement went unanswered the same way (788).

diag-live-durability-74 on A-271 merged with B9 (76394ddb) stalled the same way in 48 of 48 runs.

## Root cause

`fleet::peer_recipient` returned whatever key `peer_recipients` held for the anchor, without asking which member it was
learned from.

## Impact

- **Main.** The predecessor's pair was counted for its replacement by anchor, so writes went on and the head named the
  replacement covered. But the replacement never held a key it could open. Ranked successor after the owner's loss, it
  adopted the volume and could not unwrap its lineage (`fleet.live.takeover_refused.lineage`, 76 to 291 a run in the
  Mac's mode A runs). This is the likely root of mode A: the replacement never received a pair it could open.
- **A-271's tree.** A-271 counts a pair only once the current member accepted it, which is right. With this cache it
  never was, so a volume whose candidate was replaced held every write back for keys.

## The fix

The cached key carries the member it was learned from (`anchor -> (member, key)`). `fleet::kept_recipient` returns it
only for that member; another member at the anchor is asked for its own key. `fleet::keep_recipient` then replaces the
anchor's entry even with the cache at the fleet's peer capacity; new anchors are bounded as before, as
`pairs_delivered` is.

## Sweep: what is kept per anchor or certificate

The class: a replacement keeps its certificate and anchor but loses its store, so anything keyed by anchor or
certificate that was derived from the peer's store goes stale. A same-id restart (A-261: a warm restart that keeps the
store and the member id) keeps what the store holds, so for it keying by member is enough. Each map in `fleet`, the
transport and `seal_keys`, with the shard state they keep:

| What | Keyed by | What it holds | Why a replacement is safe |
|---|---|---|---|
| `peer_recipients` (state) | anchor | the peer's recipient public key, made in its store | **This bug.** Now carries the member; another member is asked again |
| `pairs_delivered` (state) | anchor | the member the pair was delivered to | keyed by member in effect: another member at the anchor is owed the pair again |
| `member_anchors` (state) | member | the member's anchor | keyed by member; refreshed each record period |
| `learned_members` (state) | anchor | the authenticated member id and nonce | replaced when another nonce announces; the old id retires |
| `returned_members`, `authenticated_members` (state) | member | the incarnation held dead; current ids | keyed by member |
| `record_sessions` (state) | member | the record session to the peer | evicted when the anchor's learned member changes (`fleet.rs`, the link task), with its client and discovery cursor |
| `served_sessions` (state) | anchor | this node's own congestion report of a session the peer dialed | observed here, not the peer's store; replaced on the peer's next dial |
| `link_waiters` (state) | anchor | the waker of a pending exchange | transient; removed when the exchange ends |
| `peer_paths` (state) | member | the path's round-trip estimate | keyed by member; a network measure, not the peer's store |
| `node_regions`, `council_death_watch`, `death_reports`, `host_takeovers` (state) | member | region; death votes; a retired host's takeover round | keyed by member |
| `PlaneState::peers`, `announced` (`member_task`) | anchor | the plane address; the epoch this node announced as dialer | an address is the network's; the announcement is this node's, made again on each new connection (`announced_on`) |
| `PlaneState::announced_on`, `folded`, `keyed`, `announced_from` (`member_task`) | member | per-connection announcement, liveness, keying, the process nonce | keyed by member |
| `Discovery::pins`, `scopes`, `records` (`discovery`) | anchor | the pinned certificate; its region and domain; its latest announcement | the certificate and scope are the operator's, the same for a replacement; the announcement is replaced by each authenticated one |
| `ForwardLanes::lanes` (`forward_lane`) | member | forwards queued and open to the peer | keyed by member; served-forward slots are per serve loop, so per connection |
| `PromotionRound::cursors` (`mirror`), `HostTakeover` pages (`takeover`) | member | a round's per-member cursor or page | keyed by member, per round |
| `HeadValue::content_holders` (`head`) | member | who acknowledged holding the content | keyed by member: a replacement is not counted |
| `HeadKey` (`head`), `SealKeyOwner::Pair` (`seal_keys`, owner side) | anchor | the owner's pair key per candidate, and the lineage key wrapped under it | the secret is the owner's own, not the peer's; it is delivered again to each new member at the anchor, wrapped to that member's recipient (with the fix) |
| `SealKeyOwner::PairReceived`, `PairRetired` (`seal_keys`, candidate side) | anchor and shard | a pair key an owner delivered | a different key from the anchor is a fresh boot there: it replaces the recorded one, the old kept retired while something held is sealed under it (fixed 2026-11-07) |
| `Demux::by_peer` (transport) | certificate | the peer's current session and the endpoints it still owns | a new session from the certificate replaces the current one and closes it (`Demux::bind`) |
| `Demux::by_source`, `resets_sent_at` (transport) | source address | the session under handshake from a source; reset pacing | the fleet dials from an ephemeral port (`fleet.rs`, `UdpSocket::bind` at port 0), so a replacement dials from a new source; a restart onto an old port is answered by the session's stateless-reset token (A-261) |
| `Demux::by_id`, connection maps (transport) | connection id or stream | one connection's state | ends with the connection |

The cluster crate's membership, lease renewal and consensus maps are keyed by member id.
