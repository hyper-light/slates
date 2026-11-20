//! The packs the view shows (A-665): which of the store's kept parts become packs, what else each
//! pack holds whole, and where every byte of each pack comes from. The view's module docs say why
//! each choice is made; this module is the arithmetic.

use std::mem::size_of;

use slates_vcs::{ContentId, ContentStore};

use crate::entry::{self, Header, PACK_HEADER_LEN, Representation};
use crate::object_name::{Hasher, Kind, ObjectFormat, ObjectId};
use crate::pack::{self, idx};
use crate::store::repository::Kept;
use crate::store::{Placed, Positions, Push, Repository, kind_of};
use crate::{Admission, to_u64};

use super::stored::{self, Source as StreamSource};
use super::{ReadRefused, ViewCounters, ViewRefused};

/// Format: what a pack's name hashes first, so that no other hash the crate takes is taken over
/// the same bytes (module docs of the view: the names).
const NAME_CONTEXT: &[u8] = b"slates A-665 hook view pack\0";

/// Where a kept part is kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::store) enum Source {
  /// A green's base pack.
  Base {
    /// The green.
    green: u64,
  },
  /// A version's fragments.
  Version {
    /// The green.
    green: u64,
    /// Its version.
    version: usize,
  },
}

impl Source {
  /// The part of blobs (`blobs`) or of the other kinds the store keeps there now.
  pub(in crate::store) fn kept(self, repository: &Repository, blobs: bool) -> Option<&Kept> {
    match self {
      Source::Base { green } => repository.base_part(green, blobs).map(|(_, kept)| kept),
      Source::Version { green, version } => repository.fragment_part(green, version, blobs),
    }
  }

  /// Every position the source's two parts hold; `None` when the store keeps no such parts.
  fn positions(self, repository: &Repository) -> Option<Positions> {
    let objects = self.kept(repository, false)?;
    let blobs = self.kept(repository, true)?;
    Some(objects.positions.union(&blobs.positions))
  }
}

/// What a pack of the view is made from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackKind {
  /// A green's base pack, its two parts copied.
  BasePack,
  /// A version's two fragments copied, then the bases of their reference deltas whole.
  Fragments,
  /// The objects no kept part the view shows holds, whole.
  Rest,
  /// The push's objects, whole.
  Pushed,
}

impl PackKind {
  /// What a pack's name hashes for what it is made from, after [`NAME_CONTEXT`]: a word each, each
  /// ended by a NUL so no word is another's start.
  fn word(self) -> &'static [u8] {
    match self {
      PackKind::BasePack => b"base pack\0",
      PackKind::Fragments => b"fragments\0",
      PackKind::Rest => b"rest\0",
      PackKind::Pushed => b"pushed\0",
    }
  }
}

/// A kept part a pack copies byte for byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::store) struct Part {
  /// Where the store keeps it.
  pub(in crate::store) source: Source,
  /// Whether it is the part of blobs.
  pub(in crate::store) blobs: bool,
  /// Its bytes, held by the view.
  pub(in crate::store) content: ContentId,
  /// Its length.
  pub(in crate::store) len: u64,
  /// Where it begins in its pack.
  pub(in crate::store) start: u64,
}

/// An object a pack holds whole, as a zlib stream of stored blocks (`stored`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::store) struct Whole {
  /// Its name.
  pub(in crate::store) id: ObjectId,
  /// Its type.
  pub(in crate::store) kind: Kind,
  /// Its length.
  pub(in crate::store) len: u64,
  /// Its bytes' root content, held by the view.
  pub(in crate::store) content: ContentId,
  /// Where its entry begins in its pack.
  pub(in crate::store) at: u64,
  /// Its entry header's length.
  pub(in crate::store) header: u64,
}

impl Whole {
  /// Its entry's header.
  fn entry_header(&self) -> Header {
    Header {
      representation: Representation::Whole(self.kind),
      size: self.len,
    }
  }

  /// Where its entry ends in its pack.
  pub(in crate::store) fn end(&self) -> u64 {
    self
      .at
      .saturating_add(self.header)
      .saturating_add(stored::stream_len(self.len))
  }
}

/// One pack of the view: its name, what it is made from, its kept parts in order, then the
/// objects it holds whole; its header's count, its length and its index's length.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::store) struct Unit {
  /// Its name, `pack-<name>.pack` and `.idx`.
  pub(in crate::store) name: ObjectId,
  /// What it is made from.
  pub(in crate::store) kind: PackKind,
  /// Its kept parts, in order.
  pub(in crate::store) parts: Vec<Part>,
  /// The objects it holds whole, after the parts.
  pub(in crate::store) wholes: Vec<Whole>,
  /// The objects its header counts.
  pub(in crate::store) count: u32,
  /// Its length, trailer included.
  pub(in crate::store) len: u64,
  /// Its index's length.
  pub(in crate::store) idx_len: u64,
}

/// One object to hold whole: what its entry needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::store) struct Object {
  /// Its name.
  pub(in crate::store) id: ObjectId,
  /// Its type.
  pub(in crate::store) kind: Kind,
  /// Its length.
  pub(in crate::store) len: u64,
  /// Its bytes' root content.
  pub(in crate::store) content: ContentId,
}

/// What the view's packs are made from: the kept parts taken, in order, and the positions none of
/// them holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Cover {
  /// The kept parts taken.
  pub(super) sources: Vec<Source>,
  /// The positions no part taken holds.
  pub(super) rest: Positions,
}

/// Chooses the kept parts the view shows (the view's module docs, "The cover"): the base packs,
/// largest first, each that holds a position no part taken before it holds; then, for each
/// position still uncovered, the fragments of the version that brought it when they hold it.
/// What is left is the rest, held whole.
pub(super) fn choose(
  repository: &Repository,
  admission: &mut Admission,
  counters: &mut ViewCounters,
) -> Result<Cover, ViewRefused> {
  let mut cover = Cover::default();
  let mut covered = Positions::new();
  for (green, positions) in bases(repository) {
    if positions.is_subset(&covered) {
      counters.skipped = counters.skipped.saturating_add(1);
      continue;
    }
    covered = covered.union(&positions);
    admission.charge_items::<Source>(1)?;
    cover.sources.push(Source::Base { green });
  }
  let mut uncovered = Positions::span(0, repository.next_position()).difference(&covered);
  let mut rest = Vec::new();
  let mut from = 0;
  while let Some(position) = uncovered.next_from(from) {
    if let Some((source, positions)) = bringer(repository, position)? {
      uncovered = uncovered.difference(&positions);
      admission.charge_items::<Source>(1)?;
      cover.sources.push(source);
    } else {
      admission.charge_items::<u64>(1)?;
      rest.push(position);
      from = position.saturating_add(1);
    }
  }
  cover.rest = Positions::of(rest);
  Ok(cover)
}

/// Each green's base pack as the positions it holds, the largest first, then by green.
fn bases(repository: &Repository) -> Vec<(u64, Positions)> {
  let mut bases: Vec<(u64, Positions)> = repository
    .greens()
    .filter_map(|green| Some((green, Source::Base { green }.positions(repository)?)))
    .collect();
  bases.sort_by(|left, right| right.1.len().cmp(&left.1.len()).then(left.0.cmp(&right.0)));
  bases
}

/// The fragments of the version that brought `position` (the repository's landing map's), with
/// every position they hold, when they are kept and hold it.
fn bringer(
  repository: &Repository,
  position: u64,
) -> Result<Option<(Source, Positions)>, ViewRefused> {
  let Some((green, version)) = repository.bringer_of(position) else {
    return Ok(None);
  };
  let Ok(version) = usize::try_from(version) else {
    return Ok(None);
  };
  let source = Source::Version { green, version };
  Ok(
    source
      .positions(repository)
      .filter(|positions| positions.contains(position))
      .map(|positions| (source, positions)),
  )
}

/// The packs of `cover`, then the push's when there is one, each laid out (`lay_out`).
pub(super) fn build(
  repository: &Repository,
  store: &ContentStore,
  cover: &Cover,
  push: Option<&Push>,
  admission: &mut Admission,
  counters: &mut ViewCounters,
) -> Result<Vec<Unit>, ViewRefused> {
  let format = repository.format();
  admission.charge_items::<Unit>(cover.sources.len().saturating_add(2))?;
  let mut units = Vec::with_capacity(cover.sources.len().saturating_add(2));
  for source in &cover.sources {
    units.push(kept_unit(repository, store, *source, admission, counters)?);
  }
  if !cover.rest.is_empty() {
    let objects = objects_at(repository, store, &cover.rest)?;
    counters.rest = counters.rest.saturating_add(to_u64(objects.len()));
    units.push(lay_out(format, PackKind::Rest, &[], objects, admission)?);
  }
  if let Some(push) = push {
    let objects = pushed(push)?;
    counters.pushed = counters.pushed.saturating_add(to_u64(objects.len()));
    units.push(lay_out(format, PackKind::Pushed, &[], objects, admission)?);
  }
  Ok(units)
}

/// The pack of a kept source: its two parts, then, whole, every base their reference deltas name
/// that lies outside them (a version's fragments name the objects the version before held; a base
/// pack, made with offset deltas alone, names none).
fn kept_unit(
  repository: &Repository,
  store: &ContentStore,
  source: Source,
  admission: &mut Admission,
  counters: &mut ViewCounters,
) -> Result<Unit, ViewRefused> {
  let unkept = ViewRefused::Unkept;
  let objects = source.kept(repository, false).ok_or(unkept)?;
  let blobs = source.kept(repository, true).ok_or(unkept)?;
  let parts = [(source, false, objects), (source, true, blobs)];
  let kind = match source {
    Source::Base { .. } => {
      counters.base_packs = counters.base_packs.saturating_add(1);
      PackKind::BasePack
    }
    Source::Version { .. } => {
      counters.fragments = counters.fragments.saturating_add(1);
      PackKind::Fragments
    }
  };
  let held = objects.positions.union(&blobs.positions);
  let named = objects.references.union(&blobs.references);
  let bases = objects_at(repository, store, &named.difference(&held))?;
  counters.completed = counters.completed.saturating_add(to_u64(bases.len()));
  lay_out(repository.format(), kind, &parts, bases, admission)
}

/// The objects at `positions`, in position order.
fn objects_at(
  repository: &Repository,
  store: &ContentStore,
  positions: &Positions,
) -> Result<Vec<Object>, ViewRefused> {
  positions
    .iter()
    .map(|position| {
      let (id, row, content) = repository
        .stored_at(store, position)
        .map_err(ViewRefused::Store)?;
      Ok(Object {
        id,
        kind: row.kind,
        len: row.len,
        content,
      })
    })
    .collect()
}

/// The push's objects, in their names' order.
fn pushed(push: &Push) -> Result<Vec<Object>, ViewRefused> {
  push
    .held()
    .map(|(id, number, content, len)| {
      let format = push.format();
      Ok(Object {
        id: ObjectId::from_bytes(format, id).ok_or(ViewRefused::PushUnreadable)?,
        kind: kind_of(number).ok_or(ViewRefused::PushUnreadable)?,
        len,
        content,
      })
    })
    .collect()
}

/// Lays a pack out: its header, `parts` in order, `objects` whole after them, its trailer; the
/// count its header carries, its length and its index's length, all without reading a byte; and
/// its name. The view's tables are charged to `admission` first.
pub(in crate::store) fn lay_out(
  format: ObjectFormat,
  kind: PackKind,
  parts: &[(Source, bool, &Kept)],
  objects: Vec<Object>,
  admission: &mut Admission,
) -> Result<Unit, ViewRefused> {
  admission.charge_items::<Part>(parts.len())?;
  admission.charge_items::<Whole>(objects.len())?;
  let mut at = PACK_HEADER_LEN;
  let (mut count, mut large) = (0_u64, 0_usize);
  let mut laid = Vec::with_capacity(parts.len());
  for (source, blobs, kept) in parts {
    large = large.saturating_add(large_in(&kept.placed, at));
    count = count.saturating_add(to_u64(kept.placed.len()));
    laid.push(Part {
      source: *source,
      blobs: *blobs,
      content: kept.content,
      len: kept.len,
      start: at,
    });
    at = at.saturating_add(kept.len);
  }
  let mut wholes = Vec::with_capacity(objects.len());
  for object in objects {
    let whole = place(object, at)?;
    large = large.saturating_add(usize::from(idx::is_large(at)));
    count = count.saturating_add(1);
    at = whole.end();
    wholes.push(whole);
  }
  let count = u32::try_from(count).map_err(|_| ViewRefused::TooMany { count })?;
  let name = name_of(format, kind, &laid, &wholes)?;
  Ok(Unit {
    name,
    kind,
    parts: laid,
    wholes,
    count,
    len: at.saturating_add(to_u64(format.raw_len())),
    idx_len: idx::len_of(format, usize::try_from(count).unwrap_or(usize::MAX), large),
  })
}

/// `object` placed whole at `at`.
fn place(object: Object, at: u64) -> Result<Whole, ViewRefused> {
  let mut whole = Whole {
    id: object.id,
    kind: object.kind,
    len: object.len,
    content: object.content,
    at,
    header: 0,
  };
  whole.header = entry::written_len(whole.entry_header(), at).map_err(ViewRefused::Entry)?;
  Ok(whole)
}

/// The entries of a part placed at `start` whose offsets go to an index's 8-byte table: its
/// entries are in offset order, so the first such one is found by halving.
fn large_in(placed: &[Placed], start: u64) -> usize {
  let small = placed.partition_point(|entry| !idx::is_large(start.saturating_add(entry.offset)));
  placed.len().saturating_sub(small)
}

/// A pack's name: its object format's hash of [`NAME_CONTEXT`], what it is made from, each part's
/// content and length, and each whole object's name, so the same parts and objects are always
/// the same name and nothing is read to make it.
fn name_of(
  format: ObjectFormat,
  kind: PackKind,
  parts: &[Part],
  wholes: &[Whole],
) -> Result<ObjectId, ViewRefused> {
  let mut hasher = Hasher::raw(format);
  hasher.update(NAME_CONTEXT);
  hasher.update(kind.word());
  for part in parts {
    hasher.update(&part.content.0);
    hasher.update(&part.len.to_be_bytes());
  }
  for whole in wholes {
    hasher.update(whole.id.as_bytes());
  }
  hasher.finish().map_err(ViewRefused::Collision)
}

/// What a read of a pack has of its seal: each whole object's Adler-32 known so far, in order, and
/// the trailer once known.
#[derive(Clone, Copy, Debug)]
pub(in crate::store) struct Known<'a> {
  /// The Adler-32s.
  pub(in crate::store) adlers: &'a [u32],
  /// The trailer.
  pub(in crate::store) trailer: Option<&'a ObjectId>,
}

/// What a byte of a pack is part of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Segment {
  /// The header.
  Header,
  /// A kept part.
  Part(usize),
  /// A whole object's entry.
  Whole(usize),
  /// The trailer.
  Trailer,
}

impl Unit {
  /// Where the trailer begins.
  pub(in crate::store) fn trailer_at(&self) -> u64 {
    self
      .len
      .saturating_sub(to_u64(self.name.format().raw_len()))
  }

  /// The segment holding the pack's byte `at`, where it begins and where it ends.
  pub(super) fn segment_at(&self, at: u64) -> Option<(Segment, u64, u64)> {
    if at < PACK_HEADER_LEN {
      return Some((Segment::Header, 0, PACK_HEADER_LEN));
    }
    for (index, part) in self.parts.iter().enumerate() {
      let end = part.start.saturating_add(part.len);
      if (part.start..end).contains(&at) {
        return Some((Segment::Part(index), part.start, end));
      }
    }
    let after = self.wholes.partition_point(|whole| whole.end() <= at);
    if let Some(whole) = self.wholes.get(after)
      && whole.at <= at
    {
      return Some((Segment::Whole(after), whole.at, whole.end()));
    }
    let trailer = self.trailer_at();
    (trailer..self.len)
      .contains(&at)
      .then_some((Segment::Trailer, trailer, self.len))
  }

  /// Appends the pack's bytes `from..to` (clipped to the pack) to `out`: the header made here, the
  /// parts' bytes and the whole objects' bytes read from the store a window at a time, and what
  /// only the seal knows (each Adler-32, the trailer) from `known`, refused [`ReadRefused::Unsealed`]
  /// when the bytes reach what it lacks.
  pub(in crate::store) fn fill(
    &self,
    store: &ContentStore,
    known: Known<'_>,
    (from, to): (u64, u64),
    out: &mut Vec<u8>,
  ) -> Result<(), ReadRefused> {
    let end = to.min(self.len);
    let mut at = from;
    while at < end {
      let (segment, start, stop) = self.segment_at(at).ok_or(ReadRefused::Unreadable)?;
      let upto = stop.min(end);
      let within = (at.saturating_sub(start), upto.saturating_sub(start));
      match segment {
        Segment::Header => {
          // Format: a pack's header is three 32-bit words: `PACK`, the version and the object
          // count (gitformat-pack).
          let mut header = Vec::with_capacity(size_of::<[u32; 3]>());
          pack::put_header(self.count, &mut header);
          copy(&header, within, out);
        }
        Segment::Part(index) => {
          let part = self.parts.get(index).ok_or(ReadRefused::Unreadable)?;
          content_range(store, &part.content, part.len, within, out)?;
        }
        Segment::Whole(index) => self.fill_whole(store, known, index, within, out)?,
        Segment::Trailer => {
          let trailer = known
            .trailer
            .ok_or(ReadRefused::Unsealed { pack: self.name })?;
          copy(trailer.as_bytes(), within, out);
        }
      }
      at = upto;
    }
    Ok(())
  }

  /// Appends the bytes `from..to` of whole object `index`'s entry: its header, then its stream.
  fn fill_whole(
    &self,
    store: &ContentStore,
    known: Known<'_>,
    index: usize,
    (from, to): (u64, u64),
    out: &mut Vec<u8>,
  ) -> Result<(), ReadRefused> {
    let whole = self.wholes.get(index).ok_or(ReadRefused::Unreadable)?;
    let mut header = Vec::new();
    entry::write(whole.entry_header(), whole.at, &mut header).map_err(ReadRefused::Entry)?;
    copy(&header, (from, to), out);
    let header_len = to_u64(header.len());
    if to <= header_len {
      return Ok(());
    }
    let mut source = WholeBytes {
      store,
      whole,
      adler: known
        .adlers
        .get(index)
        .copied()
        .ok_or(ReadRefused::Unsealed { pack: self.name }),
    };
    stored::fill(
      whole.len,
      (
        from.max(header_len).saturating_sub(header_len),
        to.saturating_sub(header_len),
      ),
      &mut source,
      out,
    )
  }
}

/// A whole object's bytes, read from the store, and its Adler-32 as the seal knows it.
struct WholeBytes<'a> {
  store: &'a ContentStore,
  whole: &'a Whole,
  adler: Result<u32, ReadRefused>,
}

impl StreamSource for WholeBytes<'_> {
  fn data(&mut self, from: u64, to: u64, out: &mut Vec<u8>) -> Result<(), ReadRefused> {
    content_range(
      self.store,
      &self.whole.content,
      self.whole.len,
      (from, to),
      out,
    )
  }

  fn adler(&mut self) -> Result<u32, ReadRefused> {
    self.adler
  }
}

/// Appends the bytes `from..to` of `bytes` (clipped to them) to `out`.
pub(super) fn copy(bytes: &[u8], (from, to): (u64, u64), out: &mut Vec<u8>) {
  let first = usize::try_from(from).unwrap_or(usize::MAX);
  let last = usize::try_from(to).unwrap_or(usize::MAX).min(bytes.len());
  out.extend_from_slice(bytes.get(first..last).unwrap_or_default());
}

/// Appends the bytes `from..to` of the content `len` bytes long whose root is `root` to `out`, a
/// window of the store at a time, each found down the one list a level that names it
/// (`ContentStore::window_at`): no window before `from` is read.
pub(in crate::store) fn content_range(
  store: &ContentStore,
  root: &ContentId,
  len: u64,
  (from, to): (u64, u64),
  out: &mut Vec<u8>,
) -> Result<(), ReadRefused> {
  let window = to_u64(store.window());
  let end = to.min(len);
  let mut at = from;
  while at < end {
    let index = at.checked_div(window).ok_or(ReadRefused::Unreadable)?;
    let bytes = store
      .window_at(root, len, index)
      .ok_or(ReadRefused::Unreadable)?;
    let base = index.saturating_mul(window);
    let first = usize::try_from(at.saturating_sub(base)).unwrap_or(usize::MAX);
    let last = usize::try_from(end.saturating_sub(base))
      .unwrap_or(usize::MAX)
      .min(bytes.len());
    let taken = bytes
      .get(first..last)
      .filter(|taken| !taken.is_empty())
      .ok_or(ReadRefused::Unreadable)?;
    out.extend_from_slice(taken);
    at = at.saturating_add(to_u64(taken.len()));
  }
  Ok(())
}
