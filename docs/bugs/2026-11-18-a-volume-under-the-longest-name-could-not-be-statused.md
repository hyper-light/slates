# A volume under the longest name a create accepts could not be statused (2026-11-18)

Goal g247 (every reply list bounded and paged), A-263 and A-267. Found sweeping every reply that carries a variable
field whole; reproduced by use on 6db3b095f (macOS arm64, kiln runner d1323898, 2026-11-18).

## Description

A reply is one bulk chunk, 4,096 bytes (§4.3). A-263 bounded a new volume's name so the volume's summary fits one
listing page alone: `longest_listable_name`, 4,012 bytes, `InvalidName` past it. A status names the volume too, beside
its transport report, its journal, its loss window, its mounts and its drift. So a volume created under a name near
that bound was listed, and its status was refused:

```
a 4012-byte name's status: Err(Refused(BadRequest { reason: "reply too large for the bulk area" }))
```

(`a_volume_under_the_longest_name_a_create_accepts_is_listed_and_statused` in `crates/server/tests/reply_bounds.rs`,
its first run.)

## Root cause

The bound was derived from one of the two replies that carry a volume's name whole: the listing's summary, and not
the status. The status's mounts also took a fixed quarter of the reply (`MOUNTS_REPORT_BYTES`), whatever the rest of
the report had taken.

## Impact

A volume named between 2,932 and 4,012 bytes could be created and listed, but `slates status`, the SDKs' status and
`slates-api`'s status of it answered `BadRequest`: its drift, its mounts and its loss window could not be read. On main
before A-263 a create took names up to 4,062 bytes, with the same result.

## Edits

- `crates/server/src/verbs.rs`: a create refuses a name past `longest_name()`, the lesser of `longest_listable_name()`
  and `longest_statused_name()`, read once. The status's bound is a reply chunk less `widest_unnamed_status()`: every
  optional field present, every number at its largest, the watcher's text at the longest a status writes
  (`WATCHER_SCRATCH`, `WATCHER_UNAVAILABLE`, `WATCHER_MERGE`, now the constants `drift_entries` and the merge service
  write), the base read and a takeover reported. The literal names every field, so a field appended to the report does
  not compile until it is given its widest. The status computes its mounts after the rest of its report, in the room
  it leaves (`mounts_of` takes its budget: the lesser of `MOUNTS_REPORT_BYTES` and that room), then its drift in what
  is left.
- `crates/server/src/transports.rs`: `widest_report()`, the transports at their widest on any host. That is each host
  fact at `HOST_FACT_BYTES` (255, macOS's `uname` field less its NUL; a fact past it is now cut at a character
  boundary), and the entries of whichever situation encodes widest (`every_situation`: each platform, each listener,
  each right), each entry refused with a reason. Another build differs only in its guest entries, by a reason at most.
- The longest name a create accepts is now 2,931 bytes: `reply_bounds.rs` finds it by asking and `volume_list.rs`'s
  `the_longest_name_a_create_carries_is_listed` agrees. It depends only on the wire's shapes, the chunk and
  `HOST_FACT_BYTES`, so it is the same on every host.

## Siblings

- **The status's mount budget (A-267, 2026-11-17) was not reproduced.** The 400-mount test met the partition's
  attachment table's own bound and was refused `TableAtCapacity { table: "attachments" }` at an attach, before any
  status. That bound is `PartitionCaps::attachments`, the admission limit: 4,000,000 requests a second at a 5 µs p99,
  which is 20 a partition on every host, and the rewritten test bound 20 (r1472). That happened in its "red" run on 2026-11-17 and again in
  the green run here, which is how it was found. At that bound, the path-only count overshot the quarter-reply budget
  by at most twelve bytes a mount. The drift is carried in whatever room is left, so no status was ever refused.
  Counting whole encodings is kept as an accounting correction. The test now binds mounts at 160-byte paths up to the
  table's bound, and the status carries some and counts the rest.
- No other reply carries a volume's name. Every reply list is paged, carried with a count, or bounded by a closed set
  (the sweep is in GAPS under A-267).
