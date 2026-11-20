//! A-636 part 2b-2's tests of family ids (§4.8 "Lookup" and "Promotion and takeover"; D-14; A-642): a repository and
//! every green minted to join it carry the repository's family in their ids, so all of them, and every register of
//! each, are placed on one set of holders and taken over by one successor, by computation from the id alone (§2 item
//! 12: no per-object entry anywhere).

use std::collections::BTreeMap;

use super::{
  HostId, MAX_FAMILY_TAG, ObjectId, Quorum, RegionalConfiguration, RegisterClass, candidates_for,
};
use crate::catalog::StateLog;

/// Shape: the hosts of the region the tests place in, an owner and eight co-holders: four copysets at `f = 1`, so
/// objects spread over copysets and a shared placement is no accident of the region's size.
const MEMBERS: u64 = 9;
/// Shape: the scatter width, every other member in the owner's neighbourhood.
const SCATTER: u64 = 8;
/// Shape: the host that owns the tests' objects, and is retired.
const OWNER: HostId = HostId(1);
/// Shape: the hosts that mint a family's ids: the owner, and two that minted members after taking a family over.
const CREATORS: [HostId; 3] = [OWNER, HostId(4), HostId(7)];
/// Shape: the partition the tests' ids name.
const PARTITION: u16 = 3;
/// Shape: the family the tests mint in, and the numbers its repository and members take.
const TAG: u16 = 17;
/// Shape: the numbers of a family's ids: the repository's, then its members', the largest a number may be among them.
const NUMBERS: [u32; 4] = [17, 18, 4_113, u32::MAX];
/// Shape: the other families minted beside the tests' one to show the region places families apart: with four
/// copysets, all of them land with the family with probability 4⁻³².
const PROBES: u16 = 32;

/// The region at `f = 1`.
fn region() -> RegionalConfiguration {
  RegionalConfiguration::formed(
    (1..=MEMBERS).map(HostId).collect(),
    Quorum { f: 1 },
    BTreeMap::new(),
    SCATTER,
    false,
  )
}

/// Where `object` is placed: its candidates among its owner's neighbourhood.
fn placed(regional: &RegionalConfiguration, object: ObjectId) -> Vec<HostId> {
  let neighbourhood = regional
    .neighbourhoods
    .get(&OWNER)
    .map(|neighbourhood| neighbourhood.hosts.clone())
    .unwrap_or_default();
  candidates_for(
    OWNER,
    &neighbourhood,
    &BTreeMap::new(),
    object,
    Quorum { f: 1 },
  )
}

/// Every id of family `tag`: each creator's at each number, the first number the repository's.
fn family(tag: u16) -> Vec<ObjectId> {
  CREATORS
    .iter()
    .flat_map(|creator| {
      NUMBERS
        .iter()
        .filter_map(move |number| ObjectId::family(*creator, PARTITION, tag, *number))
    })
    .collect()
}

/// Every register of `objects`: each one's own, its catalog and its four state logs'.
fn registers(objects: &[ObjectId]) -> Vec<ObjectId> {
  let classes = [
    RegisterClass::Own,
    RegisterClass::Catalog,
    RegisterClass::State(StateLog::Names),
    RegisterClass::State(StateLog::Phases),
    RegisterClass::State(StateLog::Markers),
    RegisterClass::State(StateLog::Locks),
  ];
  objects
    .iter()
    .flat_map(|object| classes.map(|class| object.in_class(class)))
    .collect()
}

/// The registers of `registers` that `same` says do not go with `first`.
fn apart(
  first: ObjectId,
  registers: &[ObjectId],
  same: impl Fn(ObjectId, ObjectId) -> bool,
) -> Vec<ObjectId> {
  registers
    .iter()
    .filter(|register| !same(first, **register))
    .copied()
    .collect()
}

/// Whether some other family's repository is placed elsewhere, or taken over by another, by `same`: the region tells
/// families apart, so a family's going as one is no accident.
fn region_tells_apart(first: ObjectId, same: impl Fn(ObjectId, ObjectId) -> bool) -> bool {
  (1..=PROBES)
    .filter_map(|offset| ObjectId::family(OWNER, PARTITION, TAG.saturating_add(offset), NUMBERS[0]))
    .any(|other| !same(first, other))
}

/// A-636 part 2b-2 (§4.8 "Promotion and takeover"; D-14; A-642): a repository's family, its own id and every green
/// minted to join it, by whichever host, is placed and taken over as one. Do: a region of nine hosts at `f = 1`, four
/// copysets; one family's ids minted by three creators at four numbers, the largest a number may be among them, and
/// every register of each (own, catalog, the four state logs); then retire host 1. Expect: every register on the
/// repository's candidates, while other families' repositories land elsewhere; and once host 1 is retired, every
/// register taken over by the repository's successor, while other families go to others.
#[test]
fn a_repositorys_family_is_placed_and_taken_over_as_one() {
  let mut regional = region();
  let ids = family(TAG);
  let repository = ids.first().copied().unwrap_or(ObjectId([0; 16]));
  let every = registers(&ids);
  assert_eq!(every.len(), CREATORS.len() * NUMBERS.len() * 6);
  let same_place = |a: ObjectId, b: ObjectId| placed(&regional, a) == placed(&regional, b);
  assert!(
    region_tells_apart(repository, same_place),
    "the region places families apart"
  );
  assert_eq!(
    apart(repository, &every, same_place),
    Vec::new(),
    "every register of the family placed on its repository's candidates"
  );
  assert!(regional.take_over(OWNER, SCATTER).is_some());
  let same_successor = |a: ObjectId, b: ObjectId| {
    let successor = regional.successor(OWNER, a);
    successor.is_some() && successor == regional.successor(OWNER, b)
  };
  assert!(
    region_tells_apart(repository, same_successor),
    "the retirement hands families to several successors"
  );
  assert_eq!(
    apart(repository, &every, same_successor),
    Vec::new(),
    "every register of the family taken over by its repository's successor"
  );
}

/// The partition `id` names, bytes 8-9 (`slates_server::verbs::fresh_volume_id`).
fn partition_of(id: ObjectId) -> u16 {
  let [_, _, _, _, _, _, _, _, high, low, ..] = id.0;
  u16::from_be_bytes([high, low])
}

/// A-636 part 2b-2 (§4.8 "Lookup"): a family id names its creator, partition, family and number, and is told from every
/// id of no family, which keeps its placement. Do: mint family ids at the corners of each field, read each back, and
/// mint one at a tag past the largest. Expect: each field read back as minted and every register of the id leading back
/// to it; ids of no family read as none and are placed by themselves (their line key their placement key); a tag past
/// the largest refused.
#[test]
fn a_family_id_reads_back_what_it_was_minted_with() {
  for (tag, number) in [(0, 0), (TAG, NUMBERS[1]), (MAX_FAMILY_TAG, u32::MAX)] {
    let id = ObjectId::family(HostId(u64::MAX), u16::MAX, tag, number);
    let read = id.map(|id| {
      (
        id.creator(),
        partition_of(id),
        id.family_tag(),
        id.family_number(),
      )
    });
    assert_eq!(
      read,
      Some((HostId(u64::MAX), u16::MAX, Some(tag), Some(number))),
      "tag {tag}, number {number}"
    );
    let leads_back = id.is_some_and(|id| registers(&[id]).iter().all(|r| r.placement_key() == id));
    assert!(leads_back, "every register of {id:?} leads back to it");
  }
  let ordinary = ObjectId::new(
    OWNER,
    (u64::from(PARTITION) << 48) | super::MAX_OBJECT_COUNTER,
  );
  assert_eq!(
    (ordinary.family_tag(), ordinary.family_number()),
    (None, None)
  );
  assert_eq!(ordinary.line_key(), ordinary.placement_key());
  assert_eq!(
    ObjectId::family(OWNER, PARTITION, MAX_FAMILY_TAG.saturating_add(1), 0),
    None
  );
}
