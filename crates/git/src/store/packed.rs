//! An object read in place from a held pack (A-722's memory per push, main's ruling B, part 1; Ada's
//! MEMORY SIZING rule of 2026-11-20: "if we don't need to allocate or can architect things in such a way that we do not
//! allocate, we do not allocate").
//!
//! **Why.** A push of git/git's v1.5.0 (a 9.4 MiB pack, 34,296 objects) left the content store holding 309 MiB, every
//! object inflated (git counts 304.6 MiB: blobs 231.0, trees 69.1, commits 4.5), 33 times the pack (measured on
//! adas-mac-studio.local-2, release, 2026-11-20). The pack itself holds every object, compressed and as the client's
//! deltas; held once, an object is read from it where it lies.
//!
//! **One seam for where a pack is held** ([`PackSource`]; Ada's storage ruling of 2026-11-20: an in-flight push's pack
//! lives in RAM, a landed repository's content on disk, in w-store's store). A source lends a pack's bytes from an offset
//! a piece at a time; the content store is the first ([`ContentStore`]'s windows, a pushed pack held once by identity),
//! the disk store the next, with no second path here.
//!
//! **What.** [`read`] answers the object whose entry begins at a place in a held pack ([`InPack`]): its header read
//! across the source's pieces ([`entry::read`]); for a delta, its base followed down to a whole object, an OFS_DELTA's
//! base an earlier entry of the same pack and a REF_DELTA's the object its caller locates ([`Bases`]: an entry of a
//! pack, or an object it hands over whole); then the base inflated and each delta inflated and applied
//! going back up (`delta::apply_to`), into buffers the caller keeps and reuses ([`Scratch`]), so a read allocates nothing
//! once its scratch has grown to the largest object it read. Each entry's data is inflated from the windows it lies in,
//! a window lent at a time, never copied whole.
//!
//! **Bounds (§4.9).** Every size a header names is held to the caller's room before any buffer grows to it. An
//! OFS_DELTA's base lies before its entry ([`entry::read`] refuses one that does not), so a chain within one pack ends; a
//! REF_DELTA's base that leads back to an entry already on the chain is refused (`Cycle`), so no chain loops. A stream
//! that inflates to other than its header's size, or bytes no inflater takes, are refused where they begin.

use std::borrow::Cow;

use flate2::{Decompress, FlushDecompress, Status};
use slates_vcs::{ContentId, ContentStore};

use crate::admission::Admission;
use crate::delta;
use crate::entry::{self, Representation};
use crate::object_name::{Kind, ObjectFormat, ObjectId};

/// Where a pack is held (the module docs, "One seam"): its bytes lent a piece at a time.
pub trait PackSource {
  /// The pack `pack` (of `len` bytes) from `offset` on, as far as one lent piece reaches; none where it is not held or
  /// `offset` is past its end.
  fn piece(&self, pack: &ContentId, len: u64, offset: u64) -> Option<Cow<'_, [u8]>>;
}

/// A pack the content store holds, by its identity: a piece is the window `offset` lies in, from `offset` on.
impl PackSource for ContentStore {
  fn piece(&self, pack: &ContentId, len: u64, offset: u64) -> Option<Cow<'_, [u8]>> {
    let window = u64::try_from(self.window())
      .ok()
      .filter(|window| *window > 0)?;
    if offset >= len {
      return None;
    }
    let within = usize::try_from(offset.checked_rem(window)?).ok()?;
    match self.window_at(pack, len, offset.checked_div(window)?)? {
      Cow::Borrowed(lent) => lent.get(within..).map(Cow::Borrowed),
      Cow::Owned(copied) => copied.get(within..).map(|rest| Cow::Owned(rest.to_vec())),
    }
  }
}

/// Where an entry begins in a held pack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InPack {
  /// The pack's identity: its root content.
  pub pack: ContentId,
  /// The pack's length.
  pub len: u64,
  /// Where the entry begins in it.
  pub offset: u64,
}

/// Where a REF_DELTA's base is: an entry of a held pack, or an object its locator wrote whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Base {
  /// An entry of a pack.
  InPack(InPack),
  /// An object written whole into the buffer [`Bases::locate`] was handed: its type.
  Whole(Kind),
}

/// Where a REF_DELTA's base is found by its name: the repository's index, and the push's own objects.
pub trait Bases {
  /// The object `id` names, where it is held: an entry of a pack, or, for one held whole outside every pack, its bytes
  /// written into `whole` (a buffer of the reader's scratch, reused); none for one held nowhere or past `room`.
  fn locate(&self, id: &ObjectId, whole: &mut Vec<u8>, room: u64) -> Option<Base>;
}

/// Why an object could not be read from its pack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackedRefused {
  /// The store does not hold the bytes at this offset of the pack.
  Unread {
    /// Where.
    at: u64,
  },
  /// An entry's header is not one (`entry::read`'s refusal).
  Entry(entry::Refusal),
  /// An entry's data does not inflate.
  Inflate {
    /// Where its entry begins.
    at: u64,
  },
  /// An entry's data inflates to other than its header's size.
  Size {
    /// Where its entry begins.
    at: u64,
  },
  /// A size past the caller's room.
  PastRoom {
    /// The size.
    size: u64,
  },
  /// A delta does not apply to its base.
  Delta(delta::Refusal),
  /// A REF_DELTA's base is held nowhere.
  MissingBase {
    /// Its name.
    id: ObjectId,
  },
  /// A REF_DELTA's base leads back to an entry already on the chain.
  Cycle {
    /// Where the entry it led back to begins.
    at: u64,
  },
}

/// One delta on a chain: where its data begins in its pack, and the size it inflates to.
#[derive(Clone, Copy, Debug)]
struct Link {
  /// Its entry.
  entry: InPack,
  /// Where its compressed data begins.
  data: u64,
  /// The bytes it inflates to.
  size: u64,
}

/// The buffers a reader keeps between reads (the module docs): the object built so far, the next one, a delta's data,
/// the chain followed, and an entry's header's bytes. Each grows to the largest it held and is reused; [`Scratch::shrink`]
/// gives back what the work no longer needs.
#[derive(Debug, Default)]
pub struct Scratch {
  /// The object built so far, up the chain.
  built: Vec<u8>,
  /// The next one, a delta applied to `built`.
  next: Vec<u8>,
  /// A delta's inflated data.
  delta: Vec<u8>,
  /// The deltas followed, from the object read down to the last before its base.
  chain: Vec<Link>,
  /// An entry's header's bytes, gathered across windows.
  header: Vec<u8>,
}

impl Scratch {
  /// Gives back every buffer's room past `keep` bytes: the work's working set, learned by its caller (MEMORY SIZING:
  /// nothing stays resident because a past object was large).
  pub fn shrink(&mut self, keep: usize) {
    for buffer in [&mut self.built, &mut self.next, &mut self.delta] {
      buffer.clear();
      buffer.shrink_to(keep);
    }
    self.chain.clear();
    self.chain.shrink_to_fit();
    self.header.clear();
  }
}

/// Reads the object whose entry begins at `place` (the module docs) into `out`, its buffer handed over whole rather than
/// copied: its type. Every size held to `room`.
pub fn read(
  store: &dyn PackSource,
  (place, format): (InPack, ObjectFormat),
  bases: &dyn Bases,
  (scratch, room): (&mut Scratch, u64),
  out: &mut Vec<u8>,
) -> Result<Kind, PackedRefused> {
  scratch.chain.clear();
  let mut at = place;
  let (kind, base) = loop {
    let (header, data) = header_at(store, at, format, (&mut scratch.header, room))?;
    match header.representation {
      Representation::Whole(kind) => {
        break (
          kind,
          Some(Link {
            entry: at,
            data,
            size: header.size,
          }),
        );
      }
      Representation::OfsDelta { base } => {
        scratch.chain.push(Link {
          entry: at,
          data,
          size: header.size,
        });
        at = InPack { offset: base, ..at };
      }
      Representation::RefDelta { base } => {
        scratch.chain.push(Link {
          entry: at,
          data,
          size: header.size,
        });
        match bases.locate(&base, &mut scratch.built, room) {
          Some(Base::InPack(entry)) => {
            if scratch.chain.iter().any(|link| link.entry == entry) {
              return Err(PackedRefused::Cycle { at: entry.offset });
            }
            at = entry;
          }
          Some(Base::Whole(kind)) => break (kind, None),
          None => return Err(PackedRefused::MissingBase { id: base }),
        }
      }
    }
  };
  // A base whole in a pack is inflated now; one held whole outside every pack is in `built` already.
  if let Some(link) = base {
    inflate_into(store, link, room, &mut scratch.built)?;
  }
  let Scratch {
    built,
    next,
    delta: delta_bytes,
    chain,
    ..
  } = scratch;
  for link in chain.iter().rev() {
    inflate_into(store, *link, room, delta_bytes)?;
    next.clear();
    delta::apply_to(built, delta_bytes, next).map_err(PackedRefused::Delta)?;
    core::mem::swap(built, next);
  }
  core::mem::swap(out, built);
  Ok(kind)
}

/// The header of the entry at `place`, and where its data begins: its bytes gathered across the windows they lie in into
/// `header`, at most the longest a header can be.
fn header_at(
  store: &dyn PackSource,
  place: InPack,
  format: ObjectFormat,
  (header, room): (&mut Vec<u8>, u64),
) -> Result<(entry::Header, u64), PackedRefused> {
  header.clear();
  let longest = entry::longest_header(format);
  each_window_from(store, place, |piece| {
    let want = longest.saturating_sub(header.len());
    header.extend_from_slice(piece.get(..want).unwrap_or(piece));
    header.len() < longest
  })?;
  let mut admission = Admission::new(room);
  let read =
    entry::read(header, place.offset, format, &mut admission).map_err(|refused| match refused {
      entry::Refusal::PastAdmission(_) => PackedRefused::PastRoom { size: room },
      other => PackedRefused::Entry(other),
    })?;
  let (found, len) = read.ok_or(PackedRefused::Unread { at: place.offset })?;
  Ok((
    found,
    place
      .offset
      .saturating_add(u64::try_from(len).unwrap_or(u64::MAX)),
  ))
}

/// Hands `each` the pack's bytes from `place`'s offset on, a piece lent at a time, until it answers `false` or the pack
/// ends. Refused where a piece is not held.
fn each_window_from(
  store: &dyn PackSource,
  place: InPack,
  mut each: impl FnMut(&[u8]) -> bool,
) -> Result<(), PackedRefused> {
  let mut at = place.offset;
  while at < place.len {
    let lent = store
      .piece(&place.pack, place.len, at)
      .ok_or(PackedRefused::Unread { at })?;
    if lent.is_empty() {
      return Err(PackedRefused::Unread { at });
    }
    if !each(&lent) {
      return Ok(());
    }
    at = at.saturating_add(u64::try_from(lent.len()).unwrap_or(u64::MAX));
  }
  Ok(())
}

/// Inflates `link`'s data into `out`, exactly its size.
fn inflate_into(
  store: &dyn PackSource,
  link: Link,
  room: u64,
  out: &mut Vec<u8>,
) -> Result<(), PackedRefused> {
  if link.size > room {
    return Err(PackedRefused::PastRoom { size: link.size });
  }
  let size = usize::try_from(link.size).map_err(|_| PackedRefused::PastRoom { size: link.size })?;
  out.clear();
  out.resize(size, 0);
  let at = link.entry.offset;
  let mut inflater = Decompress::new(true);
  let (mut filled, mut ended, mut failed) = (0usize, false, false);
  each_window_from(
    store,
    InPack {
      offset: link.data,
      ..link.entry
    },
    |piece| {
      let mut input = piece;
      loop {
        let (before_in, before_out) = (inflater.total_in(), inflater.total_out());
        let mut spare = [0u8; 1];
        let room: &mut [u8] = match out.get_mut(filled..) {
          Some(rest) if !rest.is_empty() => rest,
          _ => &mut spare,
        };
        let status = inflater.decompress(input, room, FlushDecompress::None);
        let wrote =
          usize::try_from(inflater.total_out().saturating_sub(before_out)).unwrap_or(usize::MAX);
        let took =
          usize::try_from(inflater.total_in().saturating_sub(before_in)).unwrap_or(usize::MAX);
        filled = filled.saturating_add(wrote);
        input = input.get(took..).unwrap_or_default();
        match status {
          Ok(Status::StreamEnd) => {
            ended = true;
            return false;
          }
          Ok(_) if took == 0 && wrote == 0 => return true,
          Ok(_) if filled > size => {
            failed = true;
            return false;
          }
          Ok(_) => {}
          Err(_) => {
            failed = true;
            return false;
          }
        }
      }
    },
  )?;
  if failed {
    return Err(PackedRefused::Inflate { at });
  }
  if !ended || filled != size {
    return Err(PackedRefused::Size { at });
  }
  Ok(())
}
