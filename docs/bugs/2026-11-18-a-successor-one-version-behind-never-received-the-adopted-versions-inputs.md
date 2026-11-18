# A successor one version behind never received the adopted version's inputs (2026-11-18, §4.16, §4.8, AUD-14, GAP-A9-7)

## Description

When a green's owner dies, the survivor that rendezvous ranks first for the green takes it over (§4.8 "Promotion and
takeover"): every holder reports the highest record it holds, the successor adopts the newest one, re-commits it at the
quorum, and then rebuilds the green from its own accepted merge records and the inputs it holds
(`fleet::materialize_pending_greens`, `merge_service::recover_green_inputs`, AUD-14).

A version commits at `f + 1`: with three nodes at `f = 1`, the owner and one holder. So when the owner dies, one holder
may hold the newest version and the other not yet. If the one that lacks it is the successor, it adopts the newest record
(the other holder reports it) but holds neither that version's inputs nor any way to get them: the owner, the only node
that ships them, is gone. `recover_green_inputs` found the inputs not held and the green stayed pending every period,
counted `merge.takeover_incomplete`. Before the bound it stayed so for as long as the daemon ran; with it (bug-sweep's
98c5a166, on main since 2026-11-18 as a603bd047) it is given up after the failover SLO's attempts, and every verb on it
answers `NotRecovered { cause: TakeoverIncomplete { waited_on: "chain" } }`. Either way the green was lost on the
successor although the other survivor held every byte of it, against the design's failure matrix: "Green owner loss in
a fleet: Degraded for the membership horizon, then Masked after takeover on a candidate that holds the ledger".

## How it showed

`a_share_of_a_green_made_after_its_last_version_survives_a_takeover` (`crates/server/tests/fleet.rs`) failed "the
successor materialized the taken-over green at version 3" in 1 of 2 runs of main 1ed67337's fleet suite on adacachy2,
and 1 of 3 of the supervisor's stack gate (GAPS 2026-11-09). Unlike its sibling takeover tests it does not wait for
both holders to recompute version 3 before the owner dies (`holders_before_owner_death`), so a run where the successor
was still behind met it.

## Measurement

diag-clients-green-1 (98c5a166 with the failure print, CI, 2026-11-09): the share test 10 times per lane at nice 19
beside two spinners a CPU. ubuntu-24.04-arm (4 CPUs, load 13.7 to 14.0): 2 of 10 red; ubuntu-latest: 0 of 10. Both reds
were the `chain` branch: the successor's holder state at version 2 and the other survivor's at 3,
`merge.takeover_incomplete` 97 and 100, then `merge.takeover_given_up` 1; the runs took 1,403 s and 1,388 s against 165
to 258 s for the passes. The successor's takeover counters show the adoption itself succeeded
(`fleet.takeover.adopted` 2: the green and its catalog register).

## Root cause

The takeover adopts one record per object, the newest, and a green's state is its whole ledger, not its newest record.
A successor whose own accepted prefix ends below the adopted version has the adopted record (it re-committed it) but not
that version's inputs, and nothing fetched them: `recover_green_inputs` only reads what this node holds. The general
ledger-prefix transfer was recorded as owed under GAP-A9-7 since 2026-09-18.

## Edits

- `crates/server/src/takeover.rs`: `record_adoption` keeps, with a green it queues, the members other than this node
  that reported the adopted position in the round (`PendingGreen::sources`). Each accepted that version, and a holder
  accepts a merge record only once it holds the inputs it names (A-244), so they hold every input the successor may
  lack. The adopted placement's candidates were the first choice; in a fleet larger than three they need not include
  the holders the dead owner shipped to.
- `crates/server/src/merge_service.rs`: `green_ledger_values` (this node's accepted ledger positions for the green with
  the adopted record at its version), shared by `recover_green_inputs` and the new `unheld_green_inputs`: each version
  whose record this node knows and whose inputs manifest it does not hold, lowest first; `None` when a record itself is
  unknown.
- `crates/server/src/fleet.rs`: `materialize_green` fetches what it lacks when the chain is short, then gathers again.
  `fetch_green_inputs` fetches each unheld manifest by identity from the sources, through `fetch_manifest_into_hold`:
  the volume takeover's own hedged, verified fetch (`fetch_into_hold`, now a wrapper), staged as placed for the
  input's version so the retention rule keeps it by that version's ledger position. Each input held whole is counted
  `merge.takeover_inputs_fetched`; an attempt starts no new fetch past one commit deadline. An input still not held
  after the attempt is its own wait, `merge.takeover_waits_inputs`, named `inputs` when the bound gives the green up;
  `chain` now names only a record not known here.
- `crates/server/src/state.rs`: `PendingGreen::record_unheld` records the inputs still unheld after each attempt's
  fetches. Fewer than ever before is progress, which starts the green's attempts again, so a slow fetch of a long
  chain is not given up while it advances, and an input fetched, released and fetched again cannot hold the green
  pending without end: the count starts again at most once per input the ledger names.
- `crates/ipc/src/protocol.rs`: `RecoveryCause::TakeoverIncomplete`'s `waited_on` documents `inputs`.

## Tests

`crates/server/tests/fleet.rs`
`a_successor_one_version_behind_fetches_the_versions_inputs_and_serves_the_green` (new): three nodes at `f = 1`; a green
advanced to version 2 and recomputed by both holders; the successor-to-be then refuses every content put
(`MergeFault::refuse_content_puts`), so version 3 commits at the owner and the other holder alone; the owner dies. The
successor must have been at version 2 and the other holder at 3; it must place the green at version 3, never give it
up, count a fetched input, keep the owner's chain and serve version 1 and the head's bytes. The refusal stands
throughout, so only the fetch can bring version 3's inputs.

Results (2026-11-18, runner Adas-MacBook-Pro-3: macOS arm64, 15 cores, load 32 to 60, debug build, the change at
78c7fee3 before these lines): the new test passed 2 of 2 (5.7 s alone). With the fetch cut out of a scratch copy
(`materialize_green` answering `inputs` where it fetches) it failed: the successor waited on its inputs 100 times, one a
period, and gave the green up (`given up 1, fetched 0`; the daemon logged "a taken-over green ... was not materialized
after 100 attempts ... the last waiting on its inputs"). The nine green takeover tests beside it passed 10 of 10 with
it in 36.3 s: the bound's two, the share test, AUD-14's, the isolated owner's, A-255's three resent-submit tests and the
green-locate test. `cargo test -p slates-server --lib` 245 of 245, `-p slates-ipc` whole.

Owed to the supervisor's lanes (not run here, per CLAUDE.md §8): the share test 10 times beside two spinners a CPU on
ubuntu-24.04-arm, diag-clients-green-1's recipe, where main gave the green up 2 of 10.

## Siblings

- A successor more than one version behind lacks the intermediate records too: phase one reports each holder's
  highest record only, so no fetch by identity can find their inputs. It is still given up `chain` at the bound
  (GAP-A9-7's record half, open).
- A taken-over volume's materialization (`fleet::materialize_pending`, the `fleet.materialize.waits.*` counters) already
  fetches its content from the recorded holders; its waits had no bound (GAPS 2026-11-09, sibling bullet).
- `fleet::adopt_pending_tombstones` retries a tombstone its owner shard refuses (the partition's volume capacity full)
  every period with no bound.
- `verbs::materialize_taken_over_green` holds a `debug_assert!` in production code, which CLAUDE.md's banned list
  forbids; flagged, not changed here.
