//! A-636 part 2a's tests of the register class (§4.8 "Data model" and "Promotion and takeover"; AUD-29-17; A-642): a
//! repository's state logs are registers of their own, placed and taken over as the repository is, and every register
//! of an object names its class and leads back to its object.

use std::collections::{BTreeMap, BTreeSet};

use super::{
  HostId, MAX_OBJECT_COUNTER, ObjectId, Quorum, RegionalConfiguration, RegisterClass,
  candidates_for,
};
use crate::catalog::StateLog;

/// Shape: the hosts of the region the tests place in, an owner and eight co-holders: four copysets at `f = 1`, so
/// objects spread over copysets and a shared placement is no accident of the region's size.
const MEMBERS: u64 = 9;
/// Shape: the scatter width, every other member in the owner's neighbourhood.
const SCATTER: u64 = 8;
/// Shape: the host that mints the tests' ids, and is retired.
const CREATOR: HostId = HostId(1);
/// Shape: the partition the tests' ids name.
const PARTITION: u16 = 3;
/// Shape: the counters the tests mint repositories at: a few small ones and the largest an id may carry.
const COUNTERS: [u64; 4] = [1, 2, 17, MAX_OBJECT_COUNTER];
/// Shape: the objects minted beside a repository to show the region places objects apart: with four copysets, all of
/// them land with the repository with probability 4⁻³².
const PROBES: u64 = 32;
/// The four state logs.
const STATE_LOGS: [StateLog; 4] = [
  StateLog::Names,
  StateLog::Phases,
  StateLog::Markers,
  StateLog::Locks,
];
/// Format: the byte of an id that carried the catalog's bit before the state logs' class (AUD-29-17).
const CLASS_BYTE: usize = 10;
/// Format: the catalog's bit as AUD-29-17 laid it, the counter's top bit.
const CATALOG_BIT: u8 = 0x80;
/// Format: the counter's second bit, which the state logs' class takes (A-636 part 2a).
const STATE_BIT: u8 = 0x40;

/// An id minted as `slates_server::verbs::fresh_volume_id` mints a volume's or a repository's: the creator in the high
/// eight bytes, then the partition, then the counter.
fn minted(counter: u64) -> ObjectId {
  // Format: the partition is bytes 8-9, the 16 bits above the counter's 48.
  ObjectId::new(CREATOR, (u64::from(PARTITION) << 48) | counter)
}

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

/// Where `object` is placed: its candidates among its creator's neighbourhood.
fn placed(regional: &RegionalConfiguration, object: ObjectId) -> Vec<HostId> {
  let neighbourhood = regional
    .neighbourhoods
    .get(&CREATOR)
    .map(|neighbourhood| neighbourhood.hosts.clone())
    .unwrap_or_default();
  candidates_for(
    CREATOR,
    &neighbourhood,
    &BTreeMap::new(),
    object,
    Quorum { f: 1 },
  )
}

/// The registers of `repository`'s four state logs.
fn state_registers(repository: ObjectId) -> [ObjectId; 4] {
  STATE_LOGS.map(|log| repository.in_class(RegisterClass::State(log)))
}

/// The state logs' registers of `repositories` that `follows` says do not follow their repository.
fn apart(repositories: &[ObjectId], follows: impl Fn(ObjectId, ObjectId) -> bool) -> Vec<ObjectId> {
  repositories
    .iter()
    .flat_map(|repository| {
      state_registers(*repository)
        .into_iter()
        .filter(|register| !follows(*repository, *register))
        .collect::<Vec<_>>()
    })
    .collect()
}

/// Whether some object minted beside `repository` is placed elsewhere, or taken over by another, by `same`: the
/// region tells objects apart, so a state log's following its repository is no accident.
fn region_tells_apart(repository: ObjectId, same: impl Fn(ObjectId, ObjectId) -> bool) -> bool {
  (1..=PROBES)
    .map(|offset| minted(offset.saturating_add(COUNTERS[2])))
    .any(|other| !same(repository, other))
}

/// A-636 part 2a (§4.8 "Data model" and "Promotion and takeover"; A-642): a repository's state logs are registers of
/// its own class, placed and taken over as the repository is, so the repository's state moves as one, as a volume's
/// catalog moves with its volume. Do: a region of nine hosts at `f = 1`, four copysets; repositories minted by host 1
/// at four counters, the largest an id may carry among them; their state logs' registers; then retire host 1. Expect:
/// every state log placed on its repository's candidates, while objects minted beside them land elsewhere; and once
/// host 1 is retired, every state log taken over by its repository's successor, while objects minted beside them go to
/// others.
#[test]
fn a_repositorys_state_logs_are_placed_and_taken_over_as_the_repository_is() {
  let mut regional = region();
  let repositories = COUNTERS.map(minted);
  let same_place = |a: ObjectId, b: ObjectId| placed(&regional, a) == placed(&regional, b);
  assert!(
    region_tells_apart(repositories[0], same_place),
    "the region places objects apart"
  );
  assert_eq!(
    apart(&repositories, same_place),
    Vec::new(),
    "every state log placed on its repository's candidates"
  );
  assert!(regional.take_over(CREATOR, SCATTER).is_some());
  let same_successor = |a: ObjectId, b: ObjectId| {
    let successor = regional.successor(CREATOR, a);
    successor.is_some() && successor == regional.successor(CREATOR, b)
  };
  assert!(
    region_tells_apart(repositories[0], same_successor),
    "the retirement hands objects to several successors"
  );
  assert_eq!(
    apart(&repositories, same_successor),
    Vec::new(),
    "every state log taken over by its repository's successor"
  );
}

/// Every register of `object`, each with the class it was made in: its own, its catalog and its four state logs'.
fn registers_of(object: ObjectId) -> Vec<(RegisterClass, ObjectId)> {
  let mut classes = vec![RegisterClass::Own, RegisterClass::Catalog];
  classes.extend(STATE_LOGS.map(RegisterClass::State));
  classes
    .into_iter()
    .map(|class| (class, object.in_class(class)))
    .collect()
}

/// The registers of `registers` that do not name the class they were made in, do not lead back to `object`, or are
/// not made again from every other register of it.
fn misnamed(object: ObjectId, registers: &[(RegisterClass, ObjectId)]) -> Vec<ObjectId> {
  registers
    .iter()
    .filter(|(class, register)| {
      register.class() != *class
        || register.placement_key() != object
        || registers
          .iter()
          .any(|(_, other)| other.in_class(*class) != *register)
    })
    .map(|(_, register)| *register)
    .collect()
}

/// A-636 part 2a (AUD-29-17; A-642): every register of an object names its class and leads back to its object. Do: for
/// objects minted at four counters, the largest an id may carry among them, each register: its own, its catalog and
/// its four state logs'; then the same for an id minted one past the largest counter. Expect: six distinct registers an
/// object, each naming the class it was made in, leading back to its object and made again from any other; and some
/// register of the id past the largest counter naming another class or leading elsewhere, which is why a minter
/// refuses past it.
#[test]
fn every_register_of_an_object_names_its_class_and_leads_back_to_it() {
  for object in COUNTERS.map(minted) {
    let registers = registers_of(object);
    let distinct: BTreeSet<ObjectId> = registers.iter().map(|(_, register)| *register).collect();
    assert_eq!(
      distinct.len(),
      registers.len(),
      "{object:?}'s registers are distinct"
    );
    assert_eq!(misnamed(object, &registers), Vec::new(), "{object:?}");
  }
  let past = minted(MAX_OBJECT_COUNTER.saturating_add(1));
  assert_ne!(
    misnamed(past, &registers_of(past)),
    Vec::new(),
    "an id past the largest counter has registers that do not lead back to it"
  );
}

/// Whether the catalog class treats the id whose class byte is `byte` as it did before the state logs' class: the id
/// with the top bit cleared is its object's own register, placed by itself; the id is that object's catalog exactly when
/// its top bit is set; and its catalog is a catalog register leading back to it.
fn catalog_as_before(byte: u8) -> bool {
  let mut bytes = [byte; 16];
  let id = ObjectId(bytes);
  if let Some(class_byte) = bytes.get_mut(CLASS_BYTE) {
    *class_byte &= !CATALOG_BIT;
  }
  let object = ObjectId(bytes);
  let catalog = object.catalog();
  object.class() == RegisterClass::Own
    && object.placement_key() == object
    && id.placement_key() == object
    && id.is_catalog() == (byte & CATALOG_BIT != 0)
    && catalog.is_catalog()
    && catalog.placement_key() == object
}

/// AUD-29-17 kept (A-636 part 2a): the catalog class is what it was for every id it named before the state logs' class
/// took the counter's second bit: an id with that bit clear, whatever its other bits, as every id minted under the
/// earlier cap and the server's tests' ids are. Do: for each value of the class byte with the second bit clear, an id of
/// that byte throughout. Expect: each its object's own register placed by itself, or that object's catalog when its top
/// bit is set, and its object's catalog a catalog register leading back to it.
#[test]
fn the_catalog_class_is_as_it_was_for_every_id_it_named_before() {
  let changed: Vec<u8> = (u8::MIN..=u8::MAX)
    .filter(|byte| byte & STATE_BIT == 0 && !catalog_as_before(*byte))
    .collect();
  assert_eq!(changed, Vec::new(), "class bytes whose catalog changed");
}
