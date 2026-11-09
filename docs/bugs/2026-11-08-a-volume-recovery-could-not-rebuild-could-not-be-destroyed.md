# A volume recovery could not rebuild could not be destroyed

**Found** 2026-11-06 as the sibling of `2026-11-06-a-listed-volume-recovery-could-not-rebuild-answered-not-found.md`
(GAPS, "A volume recovery could not rebuild", Open). Fixed 2026-11-08 (slabs).

**Description.** A volume whose catalog record survived a restart but which recovery could not rebuild (its base gone,
on the daemon's own mount, its image refused) answered every verb `NotRecovered`, `destroy` too. Its catalog record,
its snapshots' records, its attachments' records and its name stayed for good, and an operator had no way to get rid of
it: growth no verb could undo (CLAUDE.md §2 item 8). Two sibling verbs did not answer as the volume's state is: `share`
reads only the catalog and went through, changing the access list of a volume nothing serves, and `volume list` named
the volume without saying it was not rebuilt.

**Root cause.** `verbs::destroy` began with `find`, which needs the volume's live slot and refuses a volume
`ShardState::unrecovered` names. The destroy itself is written for a slot: it marks the volume `Destroying` and leaves
the rest to `step_destroys`, which walks only volumes with slots. `merge_service::find_record`, the lookup of the verbs
that need only the record (a share, a green's reads), never looked at `unrecovered`.

**Impact.** Every unrecovered volume kept its catalog slot (the partition's volume cap), its name and its snapshots'
records until someone edited the catalog by hand. A share of one was recorded and did nothing.

**The edits.**
- `crates/server/src/verbs.rs`: `destroy_unrecovered`, ahead of `find` in `destroy`. With the admin right, and unless a
  green holds the volume's snapshot as its origin (A-217, as for a rebuilt volume), it records the destroy in one step,
  as a merge volume's is: each of the volume's attachments ends (`end_attachment`), then `Destroying`, the lease's release
  and `VolumeDestroyed`, which takes the volume's snapshots' records, lineage and keys and leaves the tombstone a fleet's
  holders release their copies by. Then the volume leaves `unrecovered`, and its name alias, its pin on an origin's
  snapshot and its live shipping are given back (`origin_pin_of` and `release_origin_pin`, shared with `step_destroys`).
  Nothing is materialized: such a volume holds nothing in this process.
- `crates/server/src/merge_service.rs`: `find_record` refuses `NotRecovered` with the cause, so `share`, the direct map
  and every other catalog-only lookup refuse such a volume typed.
- `crates/ipc/src/protocol.rs`: `VolumeSummary::not_recovered`, the cause, appended. `crates/cli/src/verbs.rs` prints
  `not_recovered=<kind>` at the end of the volume's `volume list` line; the JSON listing (`crates/mcp/src/lib.rs`
  `summary_json`, shared by the CLI's `--json` and MCP) carries `"not_recovered"`, the kind or `null`
  (`recovery_cause_kind`).
- Tests (`crates/cli/tests/cli.rs`), each red on macOS before its fix:
  - `a_volume_recovery_could_not_rebuild_is_destroyed_and_stays_gone`: the destroy succeeds, the list and `status` no
    longer have the volume, its name is given to a new one, and after a second restart nothing is reported not rebuilt.
    Red before: `refused: NotRecovered { cause: BaseMissing { .. } }`.
  - `a_volume_recovery_could_not_rebuild_is_listed_and_its_status_names_why`, widened: the list line names
    `not_recovered=BaseMissing`, and `status`, `volume snapshot` and `share` are refused `NotRecovered`. Red before at the
    list line; with `find_record`'s check taken out, red at the share, which answered `shared`. The snapshot was refused
    already (`find`); the test now holds it.

**Siblings found, not fixed here.**
- On Linux, a FUSE mount the predecessor kept of a volume recovery could not rebuild is never adopted, so its device is
  let go and the kernel's mount stays, answering `ENOTCONN`; recovery's stale-mount unmount (`reconcile_lost`) is skipped
  for such a volume. The destroy now removes the mount's record but does not unmount it. From reading
  `crates/server/src/fuse.rs` and `verbs::rebuild_recovered`; not yet run.
- The Python and Node SDKs' listings carry neither `live_shipped` (A-200) nor `not_recovered`.
