//! A pack's seal (A-665): its trailer, its index, and the Adler-32 of each object it holds whole,
//! derived once from every byte before the trailer and kept by the caller as derived data.
//!
//! **Why a seal.** git reads a pack's trailer when it opens the pack and holds it to the copy its
//! index keeps ("packfile %s does not match index", `packfile.c` `open_packed_git_1`), and its
//! `verify-pack` and `fsck` hold the trailer to the checksum of every byte before it. An index
//! ends with its own checksum, which git maps with the copy of the trailer beside it. So a pack's
//! last bytes and every byte of its index follow from every byte of the pack: the one place a
//! read of the view cannot be a window's work. The seal is that work, done once for a pack and
//! kept: a pack made from the same kept parts and objects has the same name (`units`), so a seal
//! serves every view that shows its pack. Everything else of a pack is read without it.
//!
//! **The pass** ([`Sealing`]) reads the pack from its first byte to its trailer a window of the
//! content store at a time, hashing every byte for the trailer; it takes each whole object's
//! Adler-32 from the object's own bytes before passing its entry, and each such entry's CRC32 as
//! the entry passes. Each unit of work is one window, so the owner runs it in its shard's slices
//! ([`Quantum`], §4.3). Then [`Sealing::finish_with`] writes the index as git 2.50.1 writes one
//! (A-591's `idx::write`): the kept parts' entries from the records the store keeps of them, each
//! offset moved to where its part lies in the pack, and the whole objects' from the pass. The
//! index's bytes are kept as content (charged to the store), the Adler-32s charged beside them.

use std::collections::BTreeMap;
use std::mem::size_of;

use slates_vcs::{ContentId, ContentRefused, ContentStore};

use crate::object_name::{CollisionAttack, Hasher, ObjectId};
use crate::pack::idx;
use crate::store::learn::Clock;
use crate::store::{Placed, Repository};
use crate::{Admission, PastAdmission, to_u64};

use super::ReadRefused;
use super::stored::Adler32;
use super::units::{Known, Part, Segment, Unit, content_range};

/// One turn of a seal's work: the owner's clock and the nanoseconds the turn may spend (§4.3's
/// step budget, the owner's to derive); one unit of work is done whatever it is.
pub struct Quantum<'q> {
  /// The owner's clock.
  pub clock: &'q mut dyn Clock,
  /// The nanoseconds the turn may spend.
  pub ns: u64,
}

/// A pack's seal (module docs): what reads of its trailer, its index and its whole objects'
/// Adler-32s need.
#[derive(Debug, PartialEq, Eq)]
pub struct Seal {
  pack: ObjectId,
  trailer: ObjectId,
  idx: ContentId,
  idx_len: u64,
  adlers: Vec<u32>,
  charged: u64,
}

impl Seal {
  /// The pack it seals, by name.
  pub fn pack(&self) -> ObjectId {
    self.pack
  }

  /// The pack's trailer: the checksum of every byte before it.
  pub fn trailer(&self) -> ObjectId {
    self.trailer
  }

  /// What a read of the pack needs of it.
  pub(in crate::store) fn known(&self) -> Known<'_> {
    Known {
      adlers: &self.adlers,
      trailer: Some(&self.trailer),
    }
  }

  /// The index's bytes as content, and their length.
  pub(in crate::store) fn index(&self) -> (&ContentId, u64) {
    (&self.idx, self.idx_len)
  }

  /// Gives back what the seal holds: its index's content and the charge of its Adler-32s.
  pub fn release(self, store: &mut ContentStore) -> Result<(), ContentRefused> {
    let released = store.release(&self.idx).map(drop);
    released.and(store.discharge(self.charged))
  }
}

/// Where a read finds the seals it needs, by pack name: the caller's derived data.
pub trait Seals {
  /// The seal of the pack named `pack`, if the caller holds one.
  fn seal(&self, pack: &ObjectId) -> Option<&Seal>;
}

impl Seals for BTreeMap<ObjectId, Seal> {
  fn seal(&self, pack: &ObjectId) -> Option<&Seal> {
    self.get(pack)
  }
}

/// Where a seal finds a kept part's entries: the store's records of the part, which the store
/// holds once for every part it keeps.
pub(in crate::store) trait PartEntries {
  /// The entries of `part`, in offset order, when what is kept there now is the part the view
  /// holds.
  fn placed(&self, part: &Part) -> Option<&[Placed]>;
}

impl PartEntries for Repository {
  fn placed(&self, part: &Part) -> Option<&[Placed]> {
    let kept = part.source.kept(self, part.blobs)?;
    (kept.content == part.content && kept.len == part.len).then_some(kept.placed.as_slice())
  }
}

/// Why a seal could not be made. Each leaves the store as it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SealRefused {
  /// The view shows no pack of that name.
  NoPack,
  /// The pass has not reached the trailer yet.
  Unfinished,
  /// A read of the pack's bytes failed.
  Read(ReadRefused),
  /// What the store keeps where a part was kept is not the part the view holds: a base pack made
  /// again after the view was opened. The view is opened again.
  Changed {
    /// The pack.
    pack: ObjectId,
  },
  /// The index could not be written (A-591's refusal).
  Index(idx::WriteRefusal),
  /// The trailer's input met SHA-1's collision detection.
  Collision(CollisionAttack),
  /// The index written is not the length the view gave it.
  Length {
    /// The pack.
    pack: ObjectId,
    /// The length the view gave.
    expected: u64,
    /// The length written.
    written: u64,
  },
  /// The seal's transient tables past the caller's admission.
  PastAdmission(PastAdmission),
  /// The store refused to keep the index or to charge the Adler-32s.
  Content(ContentRefused),
}

impl From<PastAdmission> for SealRefused {
  fn from(refused: PastAdmission) -> Self {
    SealRefused::PastAdmission(refused)
  }
}

impl From<ContentRefused> for SealRefused {
  fn from(refused: ContentRefused) -> Self {
    SealRefused::Content(refused)
  }
}

/// Where a pass stands.
#[derive(Clone, Copy, Debug)]
enum Stage {
  /// The pack's bytes from `at` are next.
  Pack {
    /// The next byte.
    at: u64,
  },
  /// Whole object `whole`'s Adler-32, taken up to its byte `at`.
  Adler {
    /// The object, by its place among the pack's whole objects.
    whole: usize,
    /// Its next byte.
    at: u64,
    /// The sums so far.
    sum: Adler32,
  },
  /// Every byte before the trailer is hashed.
  Hashed,
}

/// A seal being made (module docs): the pass, a turn at a time
/// ([`HookView::seal_step`](super::HookView::seal_step)), then the index
/// ([`HookView::seal`](super::HookView::seal)).
pub struct Sealing {
  pack: ObjectId,
  stage: Stage,
  hasher: Hasher,
  adlers: Vec<u32>,
  crcs: Vec<u32>,
  crc: crc32fast::Hasher,
}

impl Sealing {
  /// A pass over `unit` from its first byte.
  pub(in crate::store) fn of(unit: &Unit) -> Sealing {
    Sealing {
      pack: unit.name,
      stage: Stage::Pack { at: 0 },
      hasher: Hasher::raw(unit.name.format()),
      adlers: Vec::new(),
      crcs: Vec::new(),
      crc: crc32fast::Hasher::new(),
    }
  }

  /// The pack it seals, by name.
  pub fn pack(&self) -> ObjectId {
    self.pack
  }

  /// Does units of work on `unit` (the pack this sealing was begun on) until `quantum` is spent,
  /// at least one: whether every byte before the trailer is hashed.
  pub(in crate::store) fn step_on(
    &mut self,
    unit: &Unit,
    store: &ContentStore,
    quantum: Quantum<'_>,
  ) -> Result<bool, SealRefused> {
    let Quantum { clock, ns } = quantum;
    let begun = clock.now_ns();
    loop {
      if self.unit_of_work(unit, store)? {
        return Ok(true);
      }
      if clock.now_ns().saturating_sub(begun) >= ns {
        return Ok(false);
      }
    }
  }

  /// One unit of work: a window of the pack hashed, or of an object's bytes taken into its
  /// Adler-32. Whether every byte before the trailer is hashed.
  fn unit_of_work(&mut self, unit: &Unit, store: &ContentStore) -> Result<bool, SealRefused> {
    match self.stage {
      Stage::Hashed => Ok(true),
      Stage::Adler { whole, at, sum } => {
        self.take_adler(unit, store, (whole, at, sum))?;
        Ok(false)
      }
      Stage::Pack { at } => self.pass(unit, store, at),
    }
  }

  /// A window of whole object `whole`'s bytes from `at` taken into `sum`; the Adler-32 kept and
  /// the pass resumed at the object's entry once its last byte is taken.
  fn take_adler(
    &mut self,
    unit: &Unit,
    store: &ContentStore,
    (whole, at, mut sum): (usize, u64, Adler32),
  ) -> Result<(), SealRefused> {
    let object = unit
      .wholes
      .get(whole)
      .ok_or(SealRefused::Read(ReadRefused::Unreadable))?;
    let to = at.saturating_add(to_u64(store.window())).min(object.len);
    let mut bytes = Vec::new();
    content_range(store, &object.content, object.len, (at, to), &mut bytes)
      .map_err(SealRefused::Read)?;
    sum.update(&bytes);
    self.stage = if to >= object.len {
      self.adlers.push(sum.value());
      Stage::Pack { at: object.at }
    } else {
      Stage::Adler { whole, at: to, sum }
    };
    Ok(())
  }

  /// A window of the pack from `at` hashed, or the Adler-32 of the whole object whose entry begins
  /// there begun. Whether the pass has reached the trailer.
  fn pass(&mut self, unit: &Unit, store: &ContentStore, at: u64) -> Result<bool, SealRefused> {
    let trailer = unit.trailer_at();
    if at >= trailer {
      self.stage = Stage::Hashed;
      return Ok(true);
    }
    let unreadable = SealRefused::Read(ReadRefused::Unreadable);
    let (segment, start, stop) = unit.segment_at(at).ok_or(unreadable)?;
    if let Segment::Whole(index) = segment
      && at == start
      && self.adlers.len() == index
    {
      self.stage = Stage::Adler {
        whole: index,
        at: 0,
        sum: Adler32::default(),
      };
      return Ok(false);
    }
    let to = at
      .saturating_add(to_u64(store.window()))
      .min(stop)
      .min(trailer);
    let mut bytes = Vec::new();
    let known = Known {
      adlers: &self.adlers,
      trailer: None,
    };
    unit
      .fill(store, known, (at, to), &mut bytes)
      .map_err(SealRefused::Read)?;
    self.hasher.update(&bytes);
    if let Segment::Whole(_) = segment {
      self.crc.update(&bytes);
      if to >= stop {
        let crc = std::mem::replace(&mut self.crc, crc32fast::Hasher::new());
        self.crcs.push(crc.finalize());
      }
    }
    self.stage = Stage::Pack { at: to };
    Ok(false)
  }

  /// Ends the seal of `unit` once the pass has hashed every byte before its trailer: the trailer,
  /// then the index (module docs), its kept parts' entries read through `entries`. The index is
  /// kept as content in `store`, the Adler-32s charged there; the index's entries, while it is
  /// written, and its bytes are charged to `admission` first.
  pub(in crate::store) fn finish_with(
    self,
    unit: &Unit,
    entries: &dyn PartEntries,
    store: &mut ContentStore,
    admission: &mut Admission,
  ) -> Result<Seal, SealRefused> {
    if !matches!(self.stage, Stage::Hashed) || self.pack != unit.name {
      return Err(SealRefused::Unfinished);
    }
    let trailer = self.hasher.finish().map_err(SealRefused::Collision)?;
    let count = usize::try_from(unit.count).unwrap_or(usize::MAX);
    admission.charge_items::<idx::Entry>(count)?;
    let mut list = Vec::with_capacity(count);
    for part in &unit.parts {
      let placed = entries
        .placed(part)
        .ok_or(SealRefused::Changed { pack: unit.name })?;
      list.extend(placed.iter().map(|entry| idx::Entry {
        id: entry.id,
        crc32: entry.crc,
        offset: part.start.saturating_add(entry.offset),
      }));
    }
    list.extend(
      unit
        .wholes
        .iter()
        .zip(&self.crcs)
        .map(|(whole, crc)| idx::Entry {
          id: whole.id,
          crc32: *crc,
          offset: whole.at,
        }),
    );
    if list.len() != count {
      return Err(SealRefused::Changed { pack: unit.name });
    }
    let bytes = index_bytes(unit, &mut list, trailer, admission)?;
    keep(unit, self.adlers, trailer, &bytes, store)
  }
}

/// The index of `unit` over `list`, as git writes it, held to the length the view gave it.
fn index_bytes(
  unit: &Unit,
  list: &mut [idx::Entry],
  trailer: ObjectId,
  admission: &mut Admission,
) -> Result<Vec<u8>, SealRefused> {
  admission.charge(unit.idx_len)?;
  let mut bytes = Vec::with_capacity(usize::try_from(unit.idx_len).unwrap_or(0));
  idx::write(list, trailer, &mut bytes).map_err(SealRefused::Index)?;
  let written = to_u64(bytes.len());
  if written != unit.idx_len {
    return Err(SealRefused::Length {
      pack: unit.name,
      expected: unit.idx_len,
      written,
    });
  }
  Ok(bytes)
}

/// The seal of `unit`: its index's `bytes` kept as content and its Adler-32s charged, or neither.
fn keep(
  unit: &Unit,
  adlers: Vec<u32>,
  trailer: ObjectId,
  bytes: &[u8],
  store: &mut ContentStore,
) -> Result<Seal, SealRefused> {
  let idx = store.put_object(bytes)?;
  let charged = to_u64(adlers.len().saturating_mul(size_of::<u32>()));
  if let Err(refused) = store.charge(charged) {
    store.release(&idx)?;
    return Err(SealRefused::Content(refused));
  }
  Ok(Seal {
    pack: unit.name,
    trailer,
    idx,
    idx_len: unit.idx_len,
    adlers,
    charged,
  })
}
