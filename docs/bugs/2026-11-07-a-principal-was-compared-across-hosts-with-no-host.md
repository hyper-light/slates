# A principal was compared across hosts with no host (2026-11-07, §4.13, A-237)

## Description

A verb forwarded to a volume's owner on another node carries the requester's principal "as the origin node
authenticated it" (`crates/server/src/verbs.rs` `ForwardedRequest`). The owner dispatched a read under it, and ran a
write under it, as is (`serve_forward`); the origin's host keyed only the completion record. `rights_of` grants a
principal equal to a volume's owner or an access entry, and a host account carried no host
(`slates_db::catalog::Principal::Uid { uid }`). So, across a forward:

- one uid on two hosts, usually two people, was one principal;
- every Windows channel is recorded as uid 0 (the bootstrap object's mode is the authentication; `slates_ipc::rendezvous`'s
  `current_uid` is 0 on Windows), so any two Windows users on different nodes were one principal;
- a Windows user was Unix root's principal, a root-run daemon's volumes theirs and the other way round.

Not affected: grants and landings (issued on the local control channel), remote attachments (`Consumer::Remote { host,
client }`), enrolled consumers (found by id on their record's partition).

Found while checking a wrong premise: that a Windows consumer could never attest. It can
(`crates/server/tests/consumers.rs`, green on ubuntu x86_64 and arm64, Windows x86_64 and arm64 and macOS, diag
`consumers-1`).

## Measurement

`crates/server/tests/fleet.rs` `a_forwarded_read_by_the_same_uid_on_another_host_is_forbidden_without_a_share`, three
in-process daemons of one test process (one uid), each its own region, on `agents/clients-r3b` (`450bd76c`): a client of
node b read the `Status` of a volume created on node a; it was served (22 s, macOS arm64, 2026-11-07).

## Root cause

A local account was treated as meaningful on every host.

## Edits

A-237 (the amendment lists every file): a uid, a SID and a consumer carry the host whose account they are, by its
certificate anchor (stable across restarts; never the per-boot member id); a forwarded principal is rescoped by its TLS
session's anchor; owner and access entries record (host, account); a refused forward is `NotShared` naming the share that
would let it in; `slates status` prints the anchor and `slates share` takes `@ANCHOR`; wire major 2, segment layout 5.

## Tests

`crates/server/tests/fleet.rs`
  `a_forwarded_read_by_the_same_uid_on_another_host_is_forbidden_without_a_share` (three in-process daemons, one uid,
  each its own region: before, a client of node b read node a's volume, served in 22 s; after, refused `NotShared`
  naming node b's anchor, 7.7 s, macOS arm64, 2026-11-07); `crates/anchor/tests/anchor.rs`
  `a_segment_from_before_host_scoped_principals_is_refused_at_start_saying_to_re_enroll`; the CLI's share parse
  (`@ANCHOR`); and every fleet test that reaches a volume from another node now shares it with that node's account by
  its anchor (`share_with_node`), the two takeover tests before the owner dies, so a survivor enforces the scoped owner.
  The sealing tenant's test derives each tenant from its owner (`Principal::tenant`), since a tenant is no longer the
  bare uid. The two-host test's second half shares the volume with exactly the principal the refusal named, and node
  b's read is then served. Takeover: `a_dead_owners_volume_refuses_an_unshared_host_through_its_successor` (the
  survivor whose share was withdrawn is refused `NotShared`, naming its own anchor, through the successor) and
  `a_restarted_peer_is_learned_on_contact_under_its_fresh_identity` (the owner's account, through its restarted host,
  reaches its volume on the successor, since a restart keeps the anchor). Windows→Unix-root by rule:
  `verbs::tests::uid_zero_on_another_host_is_not_this_hosts_root`. CI (diag `clients-host-5`, on main `e86fb417`,
  2026-11-07): wire, db, anchor, the server's unit tests, consumers, daemon, virtiofs and the CLI green on ubuntu x86_64
  and arm64; the fleet suite 94 of 95 on both, the one a test new on main whose forward needed its share
  (`green_and_work_on`, fixed after); Windows x86_64 and arm64 green but for the two landing tests that fail on main
  there too.

## Siblings

- The sealing tenant (`Principal::tenant`, A-92) was the bare uid too, so one uid on two hosts shared a tenant; it now
  hashes the host in.
- A fleet account mapping (one person's accounts on several hosts as one principal) is an open design item for Ada.
- On Windows the account is still recorded as uid 0, now host-qualified; naming it by its SID is held.
