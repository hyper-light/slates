# A clone's name does not route to the partition that holds it

Date: 2026-09-13. Status: open (found by the recovery oracle's clone read; outside the recovery
path, reported for a separate fix). Design: §4.8 "Lookup" (ids route to owners, no global index,
D-14), §4.6 "Chosen path".

## Description

`Clone { volume, snapshot, name }` is a volume-bound verb: it routes by the origin volume's id
(`verbs::volume_of`), runs on the origin's partition (`verbs::clone`: `find(state, volume)`), checks
the name's uniqueness only against that partition's catalog (`volume_by_name`), and creates the
clone there — necessarily, because a clone shares its origin's copy-on-write tree and must live in
the same store. The NFS host root resolves a name by `owner_of_name` (an FNV-1a hash over the
partitions), and `Create` routes a name the same way, so a clone whose name hashes to another
partition is (a) unreachable by name over the mount transport (`MNT /<name>` and a root `LOOKUP`
answer `NOENT` — observed 2026-09-13 in `crates/server/tests/recovery.rs` on two shards with the
name `kept-at-snapshot`, status 2), and (b) not unique per host: a later `Create` of the same name
lands on the hashed partition and succeeds.

## Root cause

Two owners for one name: creation by hash (`Create`) versus creation by origin (`Clone`). The
routing invariant the module doc of `crates/server/src/nfs.rs` states — "a name is created on the
one partition `owner_of_name` routes it to, which is also the partition the volume's id encodes" —
holds for created volumes only.

## Impact

`slates mount <clone-name>` and `cd <clone-name>` under the host root fail for roughly
`(partitions − 1) / partitions` of clone names on a multi-shard daemon; the clone is listed by the
root `READDIR` gather but cannot be entered. Duplicate names across partitions are possible.

## Candidate corrections (for the owner of §4.8 "Lookup")

- Refuse a clone name whose hashed owner is not the origin's partition (typed `InvalidName` with the
  reason), keeping one owner per name at the cost of a naming rule the user sees.
- Or route a name miss on the hashed partition to a bounded scatter over the other partitions (the
  root listing already gathers every shard), keeping ids-route-to-owners and no global index.

The recovery oracle sidesteps it by choosing a clone name that hashes to the origin's partition
(`clone_name_on_origin_partition`), so it reads the snapshot's content through the route that
exists and stays honest about why.
