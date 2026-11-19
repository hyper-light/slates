//! A-614's D(k) and reached sets as a type of derived data (A-647 part 2; A-645 "Derived data, one mechanism";
//! §4.17.2 "The repository in git's terms" item 3).
//!
//! **The type** ([`ReachSets`]). The value of a ref's version (its green and its index `k`) is two sets of positions:
//! the objects its head reaches, and D(k), what the version brought. Both are what A-614's admission computes, and the
//! type computes them with the same walks over the committed index, from the version's record and its parents' values
//! alone:
//! - a fast-forward: D(k) is the walk from the head that stops at every object version `k - 1` reached, and the reached
//!   set is that set grown by D(k);
//! - a rewrite or a create: the reached set is the set of the version it was remade from and the walk beyond it; D(k)
//!   is the walk that stops at version `k - 1`'s set, or, with no head before it, everything reached;
//! - a delete: both empty.
//!
//! Which version a set was remade from is found among the greens' heads when the move is admitted (the green whose head
//! the new head descends from, at its version then), so the store records it with the version (`Followed`): a
//! derivation reads only recorded history. A version's parents in this history (the `Graph` the repository is) are
//! version `k - 1` when it had a head, then the version its set was remade from.
//!
//! **Kept with no second walk, and the store's one copy** (A-647 part 3). The store's commit puts each version's sets,
//! as its admission walked them, as the version's value, held by the version's record; the store serves every
//! version's D(k) and reached set from it ([`Repository::brought`], [`Repository::reached`]) and keeps the sets nowhere
//! else. [`Repository::reach_value`] hands the owner a ref's newest value, which it keeps under (type, version) as it
//! commits (`Derivations::keep`), and a tier that lacks a value derives it with a fill, the same value by identity. A
//! derivation's walks are the admission's own and counted as any walk is ([`super::StoreCounters`]).
//!
//! **A value's bytes** (format 2): a derived node naming one object, the sets' bytes, with the object's length as its
//! payload (eight bytes, little-endian), so a value is as long as its sets need. Format 1 (part 2) kept the bytes in
//! the node itself, and a node is at most one window of the store, so a set of more runs than a window holds was no
//! value.
//! The sets' bytes are the reached set's runs, then D(k)'s, each a count (eight bytes, little-endian) and that many
//! runs, each a start and an end (eight bytes each, little-endian). A set keeps its runs one way (ascending, a gap
//! between each two), so the same sets are the same bytes, and bytes in any other form are refused.

use std::mem::size_of;

use slates_vcs::content::encode_derived;
use slates_vcs::derived::{DerivedType, Graph, Mode};
use slates_vcs::{ContentId, ContentRefused, ContentStore};

use super::{Followed, Repository, StoreRefused, Version};
use crate::object_name::ObjectId;
use crate::store::{Positions, Push};

/// Format: the type's name and its format's version (module docs: 2, the sets' bytes an object its node names).
const TYPE_NAME: &[u8] = b"slates-git reach sets 2";

/// Format: the bytes of a count of runs, and of a run's start and of its end, little-endian.
const WORD_BYTES: usize = size_of::<u64>();

/// Format: the bytes of one run: its start and its end.
const RUN_BYTES: usize = size_of::<(u64, u64)>();

/// Format: the first byte of a leaf's payload: the runs of one block of positions.
const LEAF: u8 = b'L';

/// Format: the first byte of an inner node's payload: its level, then a bit for each child slot.
const INNER: u8 = b'I';

/// A set's tree's shape (module docs), derived from the store's window: the positions a leaf covers and the children an
/// inner node names, each the most one window holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Shape {
  /// Positions a leaf covers: two for each run a leaf of one window holds, so a block of positions each apart fits.
  pub(crate) block: u64,
  /// Children an inner node names.
  pub(crate) fanout: u64,
}

impl Shape {
  /// The shape for a store of `window`; `None` for a window too short for a leaf of one run or an inner node of two
  /// children.
  pub(crate) fn of(window: usize) -> Option<Shape> {
    let leaf = encode_derived(&[], &[LEAF])?.len();
    let runs = window.checked_sub(leaf)?.checked_div(RUN_BYTES)?;
    let block = u64::try_from(runs.checked_mul(2)?).ok()?;
    let fanout = u64::try_from(fanout_for(window)?).ok()?;
    (block >= 2 && fanout >= 2).then_some(Shape { block, fanout })
  }

  /// The least level whose node, from block 0, covers block `block`: the root's level of a set whose last block it is.
  pub(crate) fn level_of(self, block: u64) -> u32 {
    let (mut level, mut covered) = (0_u32, 1_u64);
    while covered <= block {
      covered = covered.saturating_mul(self.fanout);
      level = level.saturating_add(1);
    }
    level
  }
}

/// An inner node's payload's length for `fanout` children: its tag, its level, and a bit for each child slot.
fn inner_payload_len(fanout: usize) -> Option<usize> {
  fanout.div_ceil(8).checked_add(2)
}

/// The most children an inner node of one `window` names, by the store's own layout of a derived node.
fn fanout_for(window: usize) -> Option<usize> {
  let count = encode_derived(&[], &[])?.len();
  let name = encode_derived(&[ContentId([0; 32])], &[])?
    .len()
    .checked_sub(count)?;
  let bytes = |fanout: usize| {
    count
      .checked_add(name.checked_mul(fanout)?)?
      .checked_add(inner_payload_len(fanout)?)
  };
  let mut fanout = window.checked_div(name)?;
  while fanout > 0 && bytes(fanout).is_none_or(|len| len > window) {
    fanout = fanout.saturating_sub(1);
  }
  Some(fanout)
}

/// Why the type refused to derive a version. Each leaves the store as it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReachRefused {
  /// The repository holds no version `version` of `green`.
  NoVersion {
    /// The green.
    green: u64,
    /// The version's index.
    version: u64,
  },
  /// The parents' values given are not the ones the version's record names: too few, or too many.
  Parents {
    /// The green.
    green: u64,
    /// The version's index.
    version: u64,
  },
  /// The value `node` does not read as the type's two sets.
  Malformed {
    /// The node.
    node: ContentId,
  },
  /// A walk met `id`, which the index does not hold: the version's objects are not all committed.
  Unindexed {
    /// The object.
    id: ObjectId,
  },
  /// The store refused a walk.
  Store(StoreRefused),
  /// The content store refused the value.
  Content(ContentRefused),
}

/// A version's two sets: what its head reaches, and D(k), what it brought.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sets {
  /// The positions its head reaches.
  pub reached: Positions,
  /// D(k): the positions it brought.
  pub brought: Positions,
}

impl Sets {
  /// The sets' bytes, the object a value names (module docs).
  fn encode(&self) -> Vec<u8> {
    let runs = self
      .reached
      .runs()
      .saturating_add(self.brought.runs())
      .saturating_mul(RUN_BYTES);
    let mut payload = Vec::with_capacity(runs.saturating_add(WORD_BYTES.saturating_mul(2)));
    for set in [&self.reached, &self.brought] {
      let pairs = set.run_pairs();
      let count = u64::try_from(pairs.len()).unwrap_or(u64::MAX);
      payload.extend_from_slice(&count.to_le_bytes());
      for (start, end) in pairs {
        payload.extend_from_slice(&start.to_le_bytes());
        payload.extend_from_slice(&end.to_le_bytes());
      }
    }
    payload
  }

  /// The sets `bytes` hold; `None` for bytes in any other form.
  fn decode(bytes: &[u8]) -> Option<Sets> {
    let (reached, rest) = decode_set(bytes)?;
    let (brought, rest) = decode_set(rest)?;
    rest.is_empty().then_some(Sets { reached, brought })
  }

  /// The sets the value `root` holds, read from `store`: the one object its node names, as long as the node says;
  /// `None` when it is no value of the type.
  pub fn read(store: &ContentStore, root: &ContentId) -> Option<Sets> {
    let node = store.derived(root)?;
    let mut names = node.names();
    let (Some(object), None) = (names.next(), names.next()) else {
      return None;
    };
    let len = <[u8; WORD_BYTES]>::try_from(node.payload()).ok()?;
    let bytes = store.object_bytes(&object, u64::from_le_bytes(len))?;
    Sets::decode(&bytes)
  }

  /// Puts the sets as the type's value (module docs): their bytes as an object of any length, and the node naming it,
  /// held once by the caller. Refused, nothing is kept.
  pub(super) fn put(&self, store: &mut ContentStore) -> Result<ContentId, ContentRefused> {
    let bytes = self.encode();
    let len = u64::try_from(bytes.len()).map_err(|_| ContentRefused::CountExhausted)?;
    let object = store.put_object(&bytes)?;
    let node = store.put_derived(&[object], &len.to_le_bytes());
    let released = store.release(&object);
    match (node, released) {
      (Ok(node), Ok(_)) => Ok(node.identity),
      (Ok(node), Err(refused)) => {
        store.release(&node.identity)?;
        Err(refused)
      }
      (Err(refused), _) => Err(refused),
    }
  }
}

/// One set read from the head of `bytes`, and what follows it; the count checked against the bytes before anything is
/// allocated.
fn decode_set(bytes: &[u8]) -> Option<(Positions, &[u8])> {
  let (count, mut rest) = bytes.split_first_chunk::<WORD_BYTES>()?;
  let count = usize::try_from(u64::from_le_bytes(*count)).ok()?;
  if count > rest.len().checked_div(RUN_BYTES)? {
    return None;
  }
  let mut runs = Vec::with_capacity(count);
  for _ in 0..count {
    let (start, after) = rest.split_first_chunk::<WORD_BYTES>()?;
    let (end, after) = after.split_first_chunk::<WORD_BYTES>()?;
    runs.push((u64::from_le_bytes(*start), u64::from_le_bytes(*end)));
    rest = after;
  }
  Some((Positions::from_runs(runs)?, rest))
}

/// The type (module docs), declared eager or lazy by its owner.
#[derive(Clone, Copy, Debug)]
pub struct ReachSets {
  mode: Mode,
}

impl ReachSets {
  /// The type, computed by `mode`.
  pub fn declare(mode: Mode) -> ReachSets {
    ReachSets { mode }
  }
}

impl Graph for Repository {
  type Version = (u64, u64);

  fn parents(&self, (green, version): (u64, u64)) -> Option<Vec<(u64, u64)>> {
    let (record, before) = self.reach_record(green, version)?;
    Some(parents_of((green, version), record, before))
  }

  fn name(&self, (green, version): (u64, u64), out: &mut Vec<u8>) {
    out.extend_from_slice(&green.to_be_bytes());
    out.extend_from_slice(&version.to_be_bytes());
  }
}

/// A version's parents in the type's history (module docs): version `k - 1` when `before` (it had a head) and the
/// version's set follows from it, then the version its set was remade from. A delete has none.
fn parents_of((green, version): (u64, u64), record: &Version, before: bool) -> Vec<(u64, u64)> {
  let previous = version
    .checked_sub(1)
    .filter(|_| before)
    .map(|previous| (green, previous));
  match (record.followed, record.head) {
    (Followed::Emptied, _) | (_, None) => Vec::new(),
    (Followed::Grown, Some(_)) => previous.into_iter().collect(),
    (Followed::Remade { base }, Some(_)) => {
      let base = base.and_then(|(green, at)| Some((green, u64::try_from(at).ok()?)));
      previous.into_iter().chain(base).collect()
    }
  }
}

impl DerivedType for ReachSets {
  type Source = Repository;
  type Refused = ReachRefused;

  fn name(&self) -> &[u8] {
    TYPE_NAME
  }

  fn mode(&self) -> Mode {
    self.mode
  }

  fn derive(
    &mut self,
    repository: &Repository,
    store: &mut ContentStore,
    (green, version): (u64, u64),
    parents: &[ContentId],
  ) -> Result<ContentId, ReachRefused> {
    let (record, before) = repository
      .reach_record(green, version)
      .ok_or(ReachRefused::NoVersion { green, version })?;
    let mut values = Vec::with_capacity(parents.len());
    for root in parents {
      values.push(Sets::read(store, root).ok_or(ReachRefused::Malformed { node: *root })?);
    }
    let sets = repository.follow(store, (green, version), (record, before), &values)?;
    sets.put(store).map_err(ReachRefused::Content)
  }
}

impl Repository {
  /// Version `version` of `green`, and whether the version before it had a head.
  fn reach_record(&self, green: u64, version: u64) -> Option<(&Version, bool)> {
    let chain = self.chains.get(&green)?;
    let at = usize::try_from(version).ok()?;
    let record = chain.versions.get(at)?;
    let before = at
      .checked_sub(1)
      .and_then(|previous| chain.versions.get(previous))
      .is_some_and(|previous| previous.head.is_some());
    Some((record, before))
  }

  /// A version's sets from its record and its parents' values, in its parents' order (module docs).
  fn follow(
    &self,
    store: &ContentStore,
    (green, version): (u64, u64),
    (record, before): (&Version, bool),
    values: &[Sets],
  ) -> Result<Sets, ReachRefused> {
    let wrong = ReachRefused::Parents { green, version };
    let head = match (record.followed, record.head) {
      (Followed::Emptied, _) | (_, None) => {
        return if values.is_empty() {
          Ok(Sets::default())
        } else {
          Err(wrong)
        };
      }
      (_, Some(position)) => self
        .id_at(store, position)
        .ok_or(ReachRefused::NoVersion { green, version })?,
    };
    let mut values = values.iter();
    let previous = if before {
      Some(values.next().ok_or(wrong)?)
    } else {
      None
    };
    let sets = match record.followed {
      Followed::Grown => {
        let previous = previous.ok_or(wrong)?;
        let brought = self.reach_walk(store, head, &previous.reached)?;
        Sets {
          reached: previous.reached.union(&brought),
          brought,
        }
      }
      Followed::Remade { base } => {
        let base = match base {
          Some(_) => values.next().ok_or(wrong)?.reached.clone(),
          None => Positions::new(),
        };
        let reached = base.union(&self.reach_walk(store, head, &base)?);
        let brought = match previous {
          Some(previous) => self.reach_walk(store, head, &previous.reached)?,
          None => reached.clone(),
        };
        Sets { reached, brought }
      }
      Followed::Emptied => Sets::default(),
    };
    if values.next().is_some() {
      return Err(wrong);
    }
    Ok(sets)
  }

  /// What the committed index reaches from `head`, stopping at every position `stop` holds: the admission's own walk,
  /// over a push that holds nothing.
  fn reach_walk(
    &self,
    store: &ContentStore,
    head: ObjectId,
    stop: &Positions,
  ) -> Result<Positions, ReachRefused> {
    let members = self
      .walk(store, &Push::new(self.format), head, stop)
      .map_err(ReachRefused::Store)?;
    if let Some(id) = members.new.first() {
      return Err(ReachRefused::Unindexed { id: *id });
    }
    Ok(members.indexed)
  }

  /// `green`'s newest version and its value of [`ReachSets`]: the very value the store serves the version's sets from,
  /// put as it committed with the sets its admission computed, held once more by the caller: what the owner keeps under
  /// (type, version) as it commits, with no second walk (module docs).
  pub fn reach_value(
    &self,
    store: &mut ContentStore,
    green: u64,
  ) -> Result<(u64, ContentId), StoreRefused> {
    let chain = self
      .chains
      .get(&green)
      .ok_or(StoreRefused::NoVersion { green, version: 0 })?;
    let newest = chain
      .versions
      .last()
      .ok_or(StoreRefused::NoVersion { green, version: 0 })?;
    store.hold(&newest.value)?;
    let version = u64::try_from(chain.versions.len().saturating_sub(1)).unwrap_or(u64::MAX);
    Ok((version, newest.value))
  }
}
