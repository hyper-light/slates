//! The read-only bare repository a git hook runs in (A-665; §4.17.2 "Hooks, in git's terms" and
//! "Pieces (git)" piece 10, part 1): a pure function of A-614's store, the refs a push read and
//! the push itself, answering a tree of paths a volume serves, each file's bytes made when a read
//! reaches them.
//!
//! **What it shows** (`fixtures/view`, made by stock git 2.50.1 with `make-view-fixtures.sh`).
//! The files and directories of a bare repository git reads: `HEAD`, a symbolic ref to the default
//! branch; `config`, as `git init --bare` writes it for the object format (for SHA-256,
//! `[extensions] objectformat` before `[core]`); `objects/info`, `objects/pack`, `refs/heads` and
//! `refs/tags`; `packed-refs`, every ref with a head at the version shown, sorted, each naming an
//! annotated tag followed by its peeled line; and in `objects/pack` the packs below, each
//! `pack-<name>.pack` with its index `pack-<name>.idx`, version 2. git takes a directory for a
//! bare repository by its `HEAD`, `objects` and `refs` (`setup.c` `is_git_directory`). Left out,
//! as nothing git does in a bare repository for a hook reads them: `description`, the hook samples,
//! `info/exclude`, and the `.rev` reverse index `index-pack` writes beside each pack by default,
//! which git builds in memory when a command needs it (`pack-revindex.c` `load_pack_revindex`).
//!
//! **The quarantine.** For `pre-receive` the push's objects are a pack of their own in
//! `objects/tmp_objdir-incoming-<six digits>/pack`, and [`HookView::environment`] answers the
//! variables receive-pack sets to it (the `environment` module's docs). For `update` the push's
//! pack lies in `objects/pack`, where receive-pack has moved the objects by then. Stock
//! receive-pack writes a push of fewer objects than `receive.unpackLimit` (100) as loose objects
//! and a larger one as a pack with a `.keep` and a `.rev`; git reads either, and the view shows one
//! pack and no `.keep`, which guards a pack from `repack`, a writer a read-only view never runs.
//!
//! **The packs.** git resolves a reference delta's base within the delta's own pack alone ("The
//! base entry _must_ be in the same pack", `packfile.c` `get_delta_base`), so each pack the view
//! shows is whole by itself:
//! - a green's base pack, its two parts copied byte for byte (it holds offset deltas alone);
//! - a version's two fragments copied byte for byte, then, held whole, each base their reference
//!   deltas name, which lies outside them in the version before: what `index-pack --fix-thin` does
//!   to a thin pack ("adding the excluded objects the deltified objects are based on to the pack",
//!   `git-index-pack.adoc`), so no fragment's bytes are rewritten;
//! - the rest, every object the repository holds that no pack taken holds (a version not yet
//!   encoded, an object a push brought that nothing reaches), held whole;
//! - the push's objects, held whole.
//!
//! An object held whole is a zlib stream of stored blocks (`stored`), so every offset of every pack
//! follows from lengths alone.
//!
//! **The cover.** At A-645's scale many greens' base packs and first fragments repeat one history
//! (a create's D(0) is everything its head reaches, A-614), so the view takes as few kept parts as
//! cover every object: the base packs, largest first, each holding an object no earlier one holds;
//! then, for each object still uncovered, the fragments of the version its row says brought it,
//! when they hold it; then the rest. Its cost is the parts taken and the rest, never the versions
//! no object needs; a part passed over is counted ([`ViewCounters::skipped`]).
//!
//! **Lazy, a window a read.** Opening reads records: the store's kept parts, the rows of the refs'
//! heads and of the objects held whole, and the tags a ref names; no other object's bytes. A
//! listing and a file's length need nothing more. A read of a pack copies the kept parts' and the
//! objects' bytes from the store a window at a time with the framing made between them, so it
//! costs the bytes asked. Only a pack's trailer, the Adler-32 ending each object held whole, and
//! its index follow from every byte of the pack: they are the pack's seal ([`Sealing`]), derived
//! once a pack in the owner's slices and kept by the caller, and a read that reaches them without
//! it is refused [`ReadRefused::Unsealed`] with the pack's name. A pack's name hashes what it is
//! made from (its parts' contents, its whole objects' names), not its bytes, so a listing names
//! every pack before any is sealed and a seal serves every view that shows its pack; git reads a
//! pack's hash from its name and never holds it to the trailer (`packfile.c` `add_packed_git`),
//! where `index-pack` names a pack by its trailer.
//!
//! **Holds.** The view holds every content it reads: [`HookView::open`] takes the holds and
//! [`HookView::close`] gives them back, so a base pack made again or a push let go while a hook
//! runs leaves the view's bytes as they were.
//!
//! **On the owner shard** (part 2): `slates-server`'s `git_hooks` opens the view over the
//! repository's store, seals its packs in the shard's slices, lending each seal the room
//! [`HookView::seal_len`] names, and hands the view to the hook driver with the variables above.
//!
//! **Owed** (GAPS, the §4.17 entry): part 3, the view presented at a path a hook runs in (a
//! read-only attachment the mount edges serve a read at a time); its seals kept as derived data
//! across views; a seal made where a version's fragments are encoded, when the bytes pass anyway;
//! `packed-refs` read from the register's sorted namespace by ranges for a repository of very
//! many refs; the cover kept as derived data, grown as versions are encoded, and the fragments
//! after a base pack merged into larger packs, so a hook's `git` searches few indexes; main's run
//! of stock git over a mounted view.

use slates_vcs::{ContentId, ContentRefused, ContentStore};

use crate::entry;
use crate::object_name::{CollisionAttack, ObjectFormat, ObjectId};
use crate::{Admission, PastAdmission, refname, to_u64};

use super::{Push, Repository, StoreRefused};

mod environment;
pub(in crate::store) mod seal;
mod stored;
mod text;
pub(in crate::store) mod units;

pub use seal::{Quantum, Seal, SealRefused, Sealing, Seals};
pub use units::PackKind;

use text::PackedRefs;
use units::{Known, Unit, content_range, copy};

/// Format: the bare repository's names git reads (`fixtures/view/bare.listing.txt`).
const HEAD: &[u8] = b"HEAD";
/// Format: the repository's configuration.
const CONFIG: &[u8] = b"config";
/// Format: the packed refs.
const PACKED_REFS: &[u8] = b"packed-refs";
/// Format: the object directory.
const OBJECTS: &[u8] = b"objects";
/// Format: its `info` directory.
const INFO: &[u8] = b"info";
/// Format: its `pack` directory, and the quarantine's.
const PACK: &[u8] = b"pack";
/// Format: the refs' directory.
const REFS: &[u8] = b"refs";
/// Format: its branches' directory.
const HEADS: &[u8] = b"heads";
/// Format: its tags' directory.
const TAGS: &[u8] = b"tags";
/// Format: a pack file's name before its name's digits (`index-pack.c` `final`).
const PACK_PREFIX: &[u8] = b"pack-";
/// Format: a pack's own file's suffix.
const PACK_SUFFIX: &[u8] = b".pack";
/// Format: its index's suffix.
const IDX_SUFFIX: &[u8] = b".idx";

/// One ref the view shows: its full name, its green, and the version of the green shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShownRef<'n> {
  /// The full name, under `refs/`.
  pub name: &'n [u8],
  /// Its green in the store.
  pub green: u64,
  /// The version shown.
  pub version: usize,
}

/// What the view shows of the repository: its refs, at the versions a push read, and `HEAD`'s
/// target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot<'s> {
  /// The refs, in any order.
  pub refs: &'s [ShownRef<'s>],
  /// `HEAD`'s target, a full name under `refs/` (the default branch, whether or not it exists).
  pub head: &'s [u8],
}

/// Where the view shows a push's objects (module docs: the quarantine).
#[derive(Clone, Copy, Debug)]
pub enum Pushed<'p> {
  /// In git's quarantine, for `pre-receive`.
  Quarantined(&'p Push),
  /// Among the repository's packs, for `update`, run after receive-pack moves the objects.
  Migrated(&'p Push),
}

impl<'p> Pushed<'p> {
  /// The push.
  fn push(self) -> &'p Push {
    match self {
      Pushed::Quarantined(push) | Pushed::Migrated(push) => push,
    }
  }
}

/// What a path names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Node {
  /// A directory.
  Directory,
  /// A file `len` bytes long.
  File {
    /// Its length.
    len: u64,
  },
}

/// One entry of a directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
  /// Its name.
  pub name: Vec<u8>,
  /// What it is.
  pub node: Node,
}

/// What opening a view did, for the tests' non-vacuity checks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ViewCounters {
  /// Base packs shown as packs.
  pub base_packs: u64,
  /// Versions' fragments shown as packs.
  pub fragments: u64,
  /// Base packs passed over, every object they hold shown already.
  pub skipped: u64,
  /// Objects held whole beside fragments: the bases their reference deltas name.
  pub completed: u64,
  /// Objects no kept part taken holds, held whole.
  pub rest: u64,
  /// The push's objects, held whole.
  pub pushed: u64,
}

/// Why a view could not be opened. Each leaves the store as it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewRefused {
  /// `HEAD`'s target is no name under `refs/` git takes.
  HeadName,
  /// A ref's name is no name under `refs/` git takes; by its place in the snapshot.
  RefName {
    /// Its place.
    at: u64,
  },
  /// A ref's name given twice; by the place of one of them.
  Duplicate {
    /// Its place.
    at: u64,
  },
  /// The snapshot names a version the store does not hold.
  NoVersion {
    /// The green.
    green: u64,
    /// The version.
    version: u64,
  },
  /// The push is of another object format than the repository.
  ForeignPush,
  /// The push holds an object by a name or a type it never admits.
  PushUnreadable,
  /// The store refused a read.
  Store(StoreRefused),
  /// A kept part the view took is no longer kept.
  Unkept,
  /// A tag met peeling a ref does not parse, or names no object.
  Tag {
    /// The tag.
    id: ObjectId,
  },
  /// A pack would hold more objects than its header counts.
  TooMany {
    /// The objects.
    count: u64,
  },
  /// An object's entry header cannot be written (a length past what git reads).
  Entry(entry::Refusal),
  /// A pack's name met SHA-1's collision detection.
  Collision(CollisionAttack),
  /// The view's tables past the caller's admission.
  PastAdmission(PastAdmission),
  /// The store refused a hold.
  Content(ContentRefused),
}

impl From<PastAdmission> for ViewRefused {
  fn from(refused: PastAdmission) -> Self {
    ViewRefused::PastAdmission(refused)
  }
}

/// Why a read could not be answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadRefused {
  /// No file at that path: a directory, or nothing.
  NotAFile,
  /// The bytes asked reach what only the seal of the pack named `pack` knows, which the caller
  /// does not hold (module docs: lazy, a window a read).
  Unsealed {
    /// The pack.
    pack: ObjectId,
  },
  /// The seal held for the pack named `pack` is not of the length the view gives its index.
  SealMismatch {
    /// The pack.
    pack: ObjectId,
  },
  /// The store does not hold bytes the view holds.
  Unreadable,
  /// A whole object's entry header cannot be written.
  Entry(entry::Refusal),
}

/// What a path of the view names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Place {
  Root,
  Head,
  Config,
  PackedRefs,
  Objects,
  Info,
  Packs,
  Quarantine,
  QuarantinePacks,
  Refs,
  Heads,
  Tags,
  /// A pack's file: its pack, and whether it is the index.
  Pack {
    unit: usize,
    index: bool,
  },
}

/// The read-only bare repository a hook runs in (module docs).
#[derive(Debug)]
pub struct HookView {
  format: ObjectFormat,
  head: Vec<u8>,
  config: Vec<u8>,
  refs: PackedRefs,
  /// The packs, by name.
  units: Vec<Unit>,
  /// The quarantined push's pack and the quarantine's directory, for `pre-receive`.
  quarantine: Option<(usize, Vec<u8>)>,
  held: Vec<ContentId>,
  counters: ViewCounters,
}

impl HookView {
  /// Opens the view of `repository` at `snapshot`, with the push's objects where `pushed` says:
  /// chooses its packs and lays them out (module docs) and holds every content it reads. Its own
  /// tables are charged to `admission` first. Refused, nothing is held.
  pub fn open(
    repository: &Repository,
    store: &mut ContentStore,
    snapshot: &Snapshot<'_>,
    pushed: Option<Pushed<'_>>,
    admission: &mut Admission,
  ) -> Result<HookView, ViewRefused> {
    if !snapshot.head.starts_with(text::REFS) || !refname::is_valid(snapshot.head) {
      return Err(ViewRefused::HeadName);
    }
    let push = pushed.map(Pushed::push);
    if push.is_some_and(|push| push.format() != repository.format()) {
      return Err(ViewRefused::ForeignPush);
    }
    let mut counters = ViewCounters::default();
    let refs = PackedRefs::of(repository, store, snapshot.refs, admission)?;
    let cover = units::choose(repository, store, admission, &mut counters)?;
    let mut units = units::build(repository, store, &cover, push, admission, &mut counters)?;
    units.sort_by(|left, right| left.name.cmp(&right.name));
    // One name is one pack: a name hashes everything a pack is made from, so two packs of one
    // name are one pack's bytes, and a directory lists a name once.
    units.dedup_by(|later, earlier| later.name == earlier.name);
    let quarantine = match pushed {
      Some(Pushed::Quarantined(_)) => units
        .iter()
        .enumerate()
        .find(|(_, unit)| unit.kind == PackKind::Pushed)
        .map(|(at, unit)| (at, environment::quarantine_name(&unit.name))),
      _ => None,
    };
    let held = hold(store, &units, admission)?;
    Ok(HookView {
      format: repository.format(),
      head: text::head(snapshot.head),
      config: text::config(repository.format()),
      refs,
      units,
      quarantine,
      held,
      counters,
    })
  }

  /// Gives back every hold the view took. It goes on past a refusal and answers the first.
  pub fn close(self, store: &mut ContentStore) -> Result<(), ContentRefused> {
    let mut first = Ok(());
    for content in &self.held {
      first = first.and(store.release(content).map(drop));
    }
    first
  }

  /// What opening the view did.
  pub fn counters(&self) -> ViewCounters {
    self.counters
  }

  /// What `path` names (`/`-separated from the view's root, the root itself empty); `None` for
  /// nothing.
  pub fn node(&self, path: &[u8]) -> Option<Node> {
    Some(match self.locate(path)? {
      Place::Head => Node::File {
        len: to_u64(self.head.len()),
      },
      Place::Config => Node::File {
        len: to_u64(self.config.len()),
      },
      Place::PackedRefs => Node::File {
        len: self.refs.len(),
      },
      Place::Pack { unit, index } => {
        let unit = self.units.get(unit)?;
        Node::File {
          len: if index { unit.idx_len } else { unit.len },
        }
      }
      _ => Node::Directory,
    })
  }

  /// The entries of the directory `path` names, sorted by name; `None` when it names no
  /// directory.
  pub fn list(&self, path: &[u8]) -> Option<Vec<Listed>> {
    let directory = |name: &[u8]| Listed {
      name: name.to_vec(),
      node: Node::Directory,
    };
    Some(match self.locate(path)? {
      Place::Root => [HEAD, CONFIG, OBJECTS, PACKED_REFS, REFS]
        .into_iter()
        .filter_map(|name| self.entry(&[name]))
        .collect(),
      Place::Objects => {
        let mut listed = vec![directory(INFO), directory(PACK)];
        listed.extend(self.quarantine.as_ref().map(|(_, name)| directory(name)));
        listed
      }
      Place::Refs => vec![directory(HEADS), directory(TAGS)],
      Place::Quarantine => vec![directory(PACK)],
      Place::Packs => self.pack_files(false),
      Place::QuarantinePacks => self.pack_files(true),
      Place::Info | Place::Heads | Place::Tags => Vec::new(),
      Place::Head | Place::Config | Place::PackedRefs | Place::Pack { .. } => return None,
    })
  }

  /// The entry `components` name under the root, as a listing shows it.
  fn entry(&self, components: &[&[u8]]) -> Option<Listed> {
    let path = components.join(&b'/');
    Some(Listed {
      name: components.last()?.to_vec(),
      node: self.node(&path)?,
    })
  }

  /// The files of the packs in the quarantine's `pack` directory (`quarantined`) or the
  /// repository's: each pack's index, then its own file, by name.
  fn pack_files(&self, quarantined: bool) -> Vec<Listed> {
    let mut listed = Vec::new();
    for (at, unit) in self.units.iter().enumerate() {
      if self.is_quarantined(at) != quarantined {
        continue;
      }
      for (suffix, len) in [(IDX_SUFFIX, unit.idx_len), (PACK_SUFFIX, unit.len)] {
        listed.push(Listed {
          name: file_name(&unit.name, suffix),
          node: Node::File { len },
        });
      }
    }
    listed
  }

  /// Appends the bytes `from..from + len` of the file `path` names (clipped to the file) to
  /// `out`: made as the read reaches them, a pack's from the store a window at a time, what only
  /// a pack's seal knows from `seals`.
  pub fn read(
    &self,
    store: &ContentStore,
    seals: &dyn Seals,
    path: &[u8],
    (from, len): (u64, u64),
    out: &mut Vec<u8>,
  ) -> Result<(), ReadRefused> {
    let range = (from, from.saturating_add(len));
    match self.locate(path).ok_or(ReadRefused::NotAFile)? {
      Place::Head => copy(&self.head, range, out),
      Place::Config => copy(&self.config, range, out),
      Place::PackedRefs => self.refs.fill(range, out),
      Place::Pack { unit, index } => {
        let unit = self.units.get(unit).ok_or(ReadRefused::Unreadable)?;
        let seal = seals.seal(&unit.name);
        if index {
          return read_index(store, unit, seal, range, out);
        }
        let known = seal.map_or(
          Known {
            adlers: &[],
            trailer: None,
          },
          Seal::known,
        );
        unit.fill(store, known, range, out)?;
      }
      _ => return Err(ReadRefused::NotAFile),
    }
    Ok(())
  }

  /// The packs the view shows: each one's name and what it is made from. Each needs its seal
  /// before its trailer, its index or an object it holds whole is read whole.
  pub fn packs(&self) -> impl Iterator<Item = (ObjectId, PackKind)> + '_ {
    self.units.iter().map(|unit| (unit.name, unit.kind))
  }

  /// A seal of the pack named `pack` begun; `None` when the view shows no such pack.
  pub fn sealing(&self, pack: &ObjectId) -> Option<Sealing> {
    self.unit(pack).map(Sealing::of)
  }

  /// The bytes a seal of the pack named `pack` keeps (the `seal` module's docs): its index, held
  /// as content, and four for the Adler-32 of each object the pack holds whole; the store's own
  /// framing of an index longer than its window aside. What the owner makes room for before it
  /// makes the seal (A-665 part 2). `None` when the view shows no such pack.
  pub fn seal_len(&self, pack: &ObjectId) -> Option<u64> {
    let unit = self.unit(pack)?;
    let sum = to_u64(std::mem::size_of::<u32>());
    let sums = to_u64(unit.wholes.len()).saturating_mul(sum);
    Some(unit.idx_len.saturating_add(sums))
  }

  /// Does units of `sealing`'s pass until `quantum` is spent, at least one: whether it has hashed
  /// every byte before the pack's trailer.
  pub fn seal_step(
    &self,
    sealing: &mut Sealing,
    store: &ContentStore,
    quantum: Quantum<'_>,
  ) -> Result<bool, SealRefused> {
    let unit = self.unit(&sealing.pack()).ok_or(SealRefused::NoPack)?;
    sealing.step_on(unit, store, quantum)
  }

  /// Ends `sealing` once its pass is done: the pack's seal, its index kept as content in `store`
  /// (the seal's docs), its transient tables charged to `admission` first.
  pub fn seal(
    &self,
    sealing: Sealing,
    repository: &Repository,
    store: &mut ContentStore,
    admission: &mut Admission,
  ) -> Result<Seal, SealRefused> {
    let unit = self.unit(&sealing.pack()).ok_or(SealRefused::NoPack)?;
    sealing.finish_with(unit, repository, store, admission)
  }

  /// What a hook running in this view, rooted at `root` (the absolute path the hook sees it at),
  /// is handed beside its own input: `GIT_DIR`, and for a quarantined push the variables
  /// receive-pack sets to its quarantine (the `environment` module's docs), sorted by name.
  pub fn environment(&self, root: &[u8]) -> Vec<(&'static str, Vec<u8>)> {
    environment::environment(
      root,
      self.quarantine.as_ref().map(|(_, name)| name.as_slice()),
    )
  }

  /// The pack named `pack`, found by halving.
  fn unit(&self, pack: &ObjectId) -> Option<&Unit> {
    let at = self
      .units
      .binary_search_by(|unit| unit.name.cmp(pack))
      .ok()?;
    self.units.get(at)
  }

  /// Whether pack `at` lies in the quarantine.
  fn is_quarantined(&self, at: usize) -> bool {
    self
      .quarantine
      .as_ref()
      .is_some_and(|(quarantined, _)| *quarantined == at)
  }

  /// What `path` names.
  fn locate(&self, path: &[u8]) -> Option<Place> {
    let mut components = path
      .split(|byte| *byte == b'/')
      .filter(|component| !component.is_empty());
    let Some(first) = components.next() else {
      return Some(Place::Root);
    };
    let rest: Vec<&[u8]> = components.collect();
    match first {
      HEAD => rest.is_empty().then_some(Place::Head),
      CONFIG => rest.is_empty().then_some(Place::Config),
      PACKED_REFS => rest.is_empty().then_some(Place::PackedRefs),
      OBJECTS => self.locate_objects(&rest),
      REFS => locate_refs(&rest),
      _ => None,
    }
  }

  /// What a path under `objects` names.
  fn locate_objects(&self, rest: &[&[u8]]) -> Option<Place> {
    match rest {
      [] => Some(Place::Objects),
      [INFO] => Some(Place::Info),
      [PACK] => Some(Place::Packs),
      [PACK, file] => self.locate_pack(file, false),
      [directory, tail @ ..] => {
        let (_, quarantine) = self.quarantine.as_ref()?;
        if *directory != quarantine.as_slice() {
          return None;
        }
        match tail {
          [] => Some(Place::Quarantine),
          [PACK] => Some(Place::QuarantinePacks),
          [PACK, file] => self.locate_pack(file, true),
          _ => None,
        }
      }
    }
  }

  /// The pack file named `file` in the quarantine's `pack` directory (`quarantined`) or the
  /// repository's.
  fn locate_pack(&self, file: &[u8], quarantined: bool) -> Option<Place> {
    let named = file.strip_prefix(PACK_PREFIX)?;
    let (digits, suffix) = match named.strip_suffix(IDX_SUFFIX) {
      Some(digits) => (digits, IDX_SUFFIX),
      None => (named.strip_suffix(PACK_SUFFIX)?, PACK_SUFFIX),
    };
    let name = ObjectId::from_hex(self.format, digits).ok()?;
    let unit = self
      .units
      .binary_search_by(|unit| unit.name.cmp(&name))
      .ok()?;
    // The name exactly as a listing writes it, its digits in lower case: one file, one name.
    let listed = file_name(&name, suffix) == file;
    (listed && self.is_quarantined(unit) == quarantined).then_some(Place::Pack {
      unit,
      index: suffix == IDX_SUFFIX,
    })
  }
}

/// What a path under `refs` names.
fn locate_refs(rest: &[&[u8]]) -> Option<Place> {
  match rest {
    [] => Some(Place::Refs),
    [HEADS] => Some(Place::Heads),
    [TAGS] => Some(Place::Tags),
    _ => None,
  }
}

/// A pack's file name: `pack-`, the name's digits, the suffix.
fn file_name(name: &ObjectId, suffix: &[u8]) -> Vec<u8> {
  let mut file = PACK_PREFIX.to_vec();
  name.write_hex(&mut file);
  file.extend_from_slice(suffix);
  file
}

/// Appends the bytes `from..to` of `unit`'s index to `out`, from its seal's content.
fn read_index(
  store: &ContentStore,
  unit: &Unit,
  seal: Option<&Seal>,
  range: (u64, u64),
  out: &mut Vec<u8>,
) -> Result<(), ReadRefused> {
  let seal = seal.ok_or(ReadRefused::Unsealed { pack: unit.name })?;
  let (content, len) = seal.index();
  if len != unit.idx_len || seal.pack() != unit.name {
    return Err(ReadRefused::SealMismatch { pack: unit.name });
  }
  content_range(store, content, len, range, out)
}

/// Holds every content `units` read, charged to `admission` first: their parts' and their whole
/// objects'. Refused, the holds taken are given back.
fn hold(
  store: &mut ContentStore,
  units: &[Unit],
  admission: &mut Admission,
) -> Result<Vec<ContentId>, ViewRefused> {
  let contents: Vec<ContentId> = units
    .iter()
    .flat_map(|unit| {
      let parts = unit.parts.iter().map(|part| part.content);
      parts.chain(unit.wholes.iter().map(|whole| whole.content))
    })
    .collect();
  admission.charge_items::<ContentId>(contents.len())?;
  for (taken, content) in contents.iter().enumerate() {
    if let Err(refused) = store.hold(content) {
      for given in contents.iter().take(taken) {
        store.release(given).map_err(ViewRefused::Content)?;
      }
      return Err(ViewRefused::Content(refused));
    }
  }
  Ok(contents)
}
