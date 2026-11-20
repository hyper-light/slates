# A live volume given up while its log's holder was away was never served after the holder came back (A-775)

**Found:** w-forming, 2026-11-20, chasing main's item (b) ("the one-off takeover stall with no survivor serving"). One
run of `live_chaos`'s `a_member_that_never_held_the_log_never_stands_in_for_its_holder` (debug, `taskpolicy -b`,
macOS arm64, Adas-Mac-Studio-4) failed "a takeover once the holder answers: no survivor served the volume within the
fleet wait". The anchor log is kept at `~/.cache/w-forming-runs/takeover-stall-pr15-run1.log` on that machine.

**What the log shows.** The successor (`chaos-fresh-member-e`) adopted the registers; the flow then stopped the holder
with the log (`chaos-fresh-member-b`) for 3 s. The successor logged "a taken-over volume e9b17dffbe59e6a1… was not
materialized after 30 attempts without progress, one a period (the failover SLO's worth), the last waiting on its live;
every verb on it answers NotRecovered". Its counters: `fleet.materialize.given_up` 1, `fleet.materialize.waits.catalog`
22, `fleet.live.takeover_waits_quorum` 11, `fleet_takeovers_pending` 0. The holder came back (every member logged it
Dead → Alive, incarnation 1) holding the log at head 741 with every acknowledged write, and stayed at fence 1: nobody
fetched from it again.

**Root cause.** A taken-over volume's materialization is bounded (2026-11-18, CLAUDE.md §2 item 8): past the failover
SLO's worth of attempts without progress (`fleet::takeover_attempts`, 30 periods here) it is given up, recorded
unrecovered on its owner shard, and dropped from `pending_materializations` with its adopted catalog register. The
design judged a given-up object afresh only at a new configuration (§4.8, the promise wait's paragraph). A log holder
heard again is no configuration change, so the volume answered `NotRecovered` for good. The waits that spent the
attempts were all the holder's absence: its catalog's cohorts and its live log's quorum both need it.

**Impact.** Any live volume whose successor met a holder of its log paused or partitioned for about the failover SLO
(a long GC or scheduling stall, a short network partition, a slow machine under load) stayed unserved for the daemon's
life, though its every acknowledged write was durable on that holder and the holder came back.

**Reproduced in process.** `crates/server/tests/fleet.rs`
`a_live_volume_given_up_while_its_holder_was_away_is_served_once_the_holder_is_heard_again`: three nodes; the
non-successor holder leaves the successor's live reports and fetches unanswered (`Daemon::inject_live_silence`, new) and,
once the successor adopted the registers, goes unheard by it on the probe plane; the owner stops. Red on main: "not
materialized after 23 attempts without progress … the last waiting on its live", then no service through the serve
deadline (415 s), no file read back.

**The fix (A-775).** A volume given up while it waited on its takeover cohorts' members (`VolumeWait::Live`, `Catalog`,
`CatalogPlacement`, `Content`) is kept (`ShardState::given_up_volumes`) with its pending record, its adopted catalog
register and the incarnation each cohort member was heard at. Once one of them is heard alive at a later incarnation
(`fleet::judge_given_up_afresh`, each period before the materializations), its owner shard's `TakeoverIncomplete` record
goes, so verbs wait for its materialization again (A-548), and it is pending again with its catalog and its attempts from
zero (`fleet.materialize.judged_afresh`). A record gone another way (the volume destroyed) or a volume adopted again
meanwhile is not taken up. Bounded as the takeover round's own fresh judgement (`HostTakeover::heard_since`): once per
member's later incarnation. After: given up after 23 attempts, judged afresh once, served, all 24 files read back
(8.25 s).

**Siblings, not changed.** A taken-over green given up on (`materialize_pending_greens`) is still final until a new
configuration; its waits on inputs a returning holder could serve would want the same rule.
