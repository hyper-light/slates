# A green could not be shared

**Found** 2026-11-04 by the fs-server lab: a two-node fleet, node A in a Docker container on adacachy, node B on the
host. A green created on A, read from B through the fleet's forward, was refused `Forbidden` (B's user is another
principal), and `slates share GREEN uid:1000 --read` on A answered `NotFound` four times in four tries, while
`slates status GREEN` on A answered.

**Root cause.** `share` (`crates/server/src/verbs.rs`) found the volume through `find`, which needs the volume's slot
in `state.by_id`. A green has no slot: its content is its merge engine (`state.greens`), and its catalog record is all
the volume table holds. So `share` on any green answered `NotFound`, on one node or a fleet, while every merge verb
authorizes a green by the access list `share` writes (`merge_service::require_green`). Only a green's owner could ever
read it, work over it or submit to it.

**Impact.** No principal but a green's creator could use it: no second consumer on one host, and no user of another
fleet node, whose forwarded verbs run under that node's principal (§4.13). The merge plane's multi-agent purpose
(§4.16 "Role") was reachable only by agents sharing one principal.

**Fix.** `share` reads the volume's record (`merge_service::find_record`, which every role has) and keeps `find`'s
answer for a volume this shard has no record of (`absent`: `Overloaded` while a held copy awaits materialization,
`NotFound` otherwise). Test first: `a_shared_green_is_read_and_worked_by_the_principal_it_names`
(`crates/server/tests/daemon.rs`) failed on the old code (`Refused { refusal: NotFound }` at the share) and passes
after; the daemon suite passes 24 of 24 (2026-11-04, adacachy, `cargo +1.98.1 test -p slates-server --test daemon`).

**Siblings checked.** The other verbs that take a volume through `find` are store verbs a green refuses typed first
(`refuse_store_verb`) or attachment verbs a green serves through `attachable_record`. Not fixed here, reported: two
`slates green proj` calls on one two-shard daemon made two greens named `proj` (ids `…0001…0006` and `…0000…000e`),
since a name's uniqueness is checked on the creating shard only (the gotcha `crates/cli/tests/cli.rs` records for
`create`).
