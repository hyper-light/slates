# A redial went around the port mapping: the record plane was announced at its bound port (2026-11-18)

## Description

The first gated run of `crates/cli/tests/live_chaos.rs` (the integrator, main bfc34cace, kiln r1246 on the Mac Studio
d1323898, macOS arm64, release, `--test-threads=1`) failed StaleNeverCommits in all three flows that cut a running
owner off at the fault relay: both ways (`an_owner_cut_off_while_running_is_superseded_and_serves_nothing_stale`, relay
seed 12133670962905179836), deafened (`..._that_hears_nothing_while_it_still_speaks_...`, 7234295468856885196) and
muted (`..._no_peer_hears_while_it_hears_them_all_...`, 61546732998387044). The owner went on acknowledging writes
sent to it after the cut, each in 0.5 to 0.9 ms, from the first one 0.1 ms into the cut until its candidates took the
volume over about two seconds later: 2,086, 3,528 and 2,412 of them. The same three flows failed the same way on
GitHub's Linux runners, x86_64 and arm64 (diag run 44269183565 at main 69cd38b22).

## Root cause

The writes were not stale: every one was durable on a candidate when it was acknowledged. The partition was not there.
The relay never carried the record plane, the one a live write is made durable on.

- A fleet node announces itself to every peer it reaches through discovery (§4.8, `crates/server/src/discovery.rs`),
  and a peer prefers that announcement to its manifest at every dial (`fleet::resolve_peer_address`).
- `fleet::run_membership` built the announcement's record address from the advertised IP and the port the record
  socket was bound to (`record_socket.local_addr()`). Behind a port mapping (A-277's `bind`) that is the inner port, not
  the advertised one. A-277 states that the node "still announces only" its advertised address; its implementation
  carried the bound port for the record plane.
- Formation redials every record link once: when the peer's fresh member id is learned, the seed id's session is dropped
  (`fleet::refresh_record_identity_in`) and the link dials again through `fleet::client_for`, by then with the peer's
  announcement in hand. So from formation on, every record link of a relayed fleet dialed the peer's bound socket
  directly, around the relay. Any later redial (`live_ship::lose_links` after an unanswered exchange) did the same.

The probe plane was announced correctly, so the relay's cuts did silence each owner's probes: its lease lapsed (every
read through it was refused `LeaseUnconfirmed`) and its candidates declared it dead and took the volume over. Its
record links went on meanwhile, so it shipped every write to a candidate that answered, until the takeover's phase one
raised their fences and its writes were refused `StaleEpoch`.

The evidence, all from the integrator's logs:

- NoLoss on the successor held for every acknowledged write, those acknowledged during the cut included: they were
  durable on the candidates.
- The cut-off flow: 2,143 acknowledged writes, and 1,377 datagrams of every plane toward the owner crossed the relay in
  the whole run (paths 1->0 and 2->0, forwarded and cut). During the cut, 2,086 writes were acknowledged while 41
  datagrams toward the owner reached the relay at all, every one cut. Each acknowledgement needs a candidate's answer.
- The deafened flow: 3,578 acknowledged writes against 2,058 datagrams toward the owner in the whole run. On Linux
  x86_64, the cut-off flow: 4,219 acknowledged against 368.
- Every relayed flow opened exactly twelve flows (formation's probe and record links) and retired none, so no redial
  ever reached the relay.
- The lossy flow lost one datagram in the whole run, at 1 % and 5 % loss on every path: its writes never crossed it.

## Impact

- **The chaos flows behind the relay were vacuous on the record plane** from the day they landed (f1a1af32e,
  2026-11-17): loss, holds, one-way and two-way cuts reached only the probes. The candidates-in-turn, slow-candidate,
  lossy and nemesis flows passed without their faults ever touching a live write.
- **In the field, behind a real port mapping** (a container runtime's published ports, a NAT's port forward: A-277's
  purpose), a peer's every record redial would dial the advertised host at the inner port, which the mapping does not
  forward, so a record link once lost would never come back. Nothing unsafe: a write is acknowledged only at a
  candidate's answer, so a link that cannot reach a candidate stalls writes rather than lose them.
- No safety defect of the write path: the owner acknowledged only what a candidate held, and stopped at the fence.

## Exact edits (A-330)

- `crates/server/src/fleet.rs`: `FleetTransport::advertise_record`, the record-plane address a node publishes;
  `run_membership` hands both advertised addresses to discovery and no longer reads the bound port.
- `crates/server/src/discovery.rs`: `Discovery::new` takes the two advertised addresses and announces them as given.
- `crates/server/src/deploy.rs`: the plan sets `advertise_record` to the advertised block's record port
  (`serve_address`), never the bind. Its A-277 test now holds both nodes' announced record addresses.
- `crates/server/tests/fleet.rs`: `a_candidate_behind_a_port_mapping_is_redialed_through_it_so_a_cut_there_holds_its_record_plane`
  (a three-node fleet whose two candidates sit behind a `PortMapping` each; a live write while both mappings' record
  planes are cut must not be acknowledged, and must be once healed); `start_mesh_bound_and_advertised` starts a mesh
  whose nodes bind and advertise different pairs.
- `crates/cli/tests/support/relay.rs`: each path counts the record plane's datagrams apart (`PathCount::records`).
- `crates/cli/tests/live_chaos.rs`: every relayed flow ends with `judge_relayed`, which requires the relay to have
  carried at least one record-plane datagram per acknowledged write: a stream sends its next write only once the last
  is answered, so each acknowledgement is a distinct answer from a candidate.
- §4.8 "Deployment", §4.10's chaos status, A-330, GAPS.

## Sibling sweep

Every other address a node publishes to its peers was checked. The probe plane's announcement is the advertised
address. The network export's port is read from its listener but announced to no peer. The NFS port is reported to a
local client only. No other `local_addr` reaches a peer.
