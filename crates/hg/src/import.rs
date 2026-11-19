//! An existing Mercurial repository's store imported as the repository's base (A-630 parts 2 and 3; §4.17.3 piece 3,
//! "an existing repository's store imported as a base"; §4.15, a repository directory taken by a read; 7.2's
//! `localrepo.makestore`, `store.py`, `revlog.py`, `revlogutils/docket.py`, `changegroup.py` and `verify.py` \[C\]).
//!
//! **Read-only, through one seam.** The import reads a repository's `.hg` directory through [`RepositoryFiles`]: a
//! file's length, then its bytes by ranges (A-649 part 3), by its path's names, which the server answers from §4.15's
//! base plane: its host seam opens one name at a time, never a path string, and has no verb that writes. Nothing is written anywhere but the index the
//! import returns and the content store that holds its texts. Every name the import hands the host is plain, never `.`
//! or `..` and holding no NUL; a name built from bytes that could spell either (a file index docket's id) is refused
//! before the host is asked.
//!
//! **The walk, as 7.2 opens a store.** First the requirements: `requires`, and under `share-safe` the store's own
//! `store/requires` (7.2's `localrepo.makelocalrepository`), each read, beside the point of the logs, or refused by kind
//! ([`crate::store::Layout`]); they say how the store lists its file logs and names its files. Then the listing: the
//! fncache's revlog files by revlog, as 7.2's `_gather_revlog` groups them (any other file passed over, as 7.2 passes it
//! over), or the file index's paths ([`crate::file_index`]); every path sorted, once. Then each revlog, opened by its
//! index file's own header as 7.2's `_loadindex` opens one: version 1, inline or with its data file, or a docket naming
//! its index and data files (revlog version 2, and the changelog's version 2). An absent changelog or manifest log is
//! empty, as 7.2 reads one (a store has neither before its first commit); a listed file log whose index file is absent
//! is refused.
//!
//! **In 7.2's order, the whole kept or refused.** The changelog, then the manifest log, then each file log in its
//! path's order, as a changegroup brings them and 7.2's `addgroup` takes them ([`HgIndex::append_changesets`],
//! [`HgIndex::append_revisions`]). Each revision's text is rebuilt from its delta chain and held to its node by the
//! revlog reader before anything of it is kept; each is kept once as content, a file revision's metadata block apart
//! from its bytes, and appended at once, numbered its revlog's own (A-649 part 3: no log's group is held). The import
//! builds a new index, and any refusal releases it and gives back every hold the import took, so a refused import
//! leaves the content store as it found it.
//!
//! **In slices** (A-649 part 3). An [`Import`] is advanced a turn at a time under the owner's quantum, §4.3's step budget
//! ([`Import::advance`], [`Turn`]; git's upload is the model), each turn doing units of work until the quantum is spent
//! on the owner's [`Clock`], at least one: a log opened, one revision read, kept and appended, one changeset's manifest
//! checked, the repository's state read. Between turns it holds no borrow of the host or the store. [`import`] is its
//! thin facade, one turn that never ends before its work.
//!
//! **Checks across the logs**, the cheap ones 7.2's `verify` makes: each manifest and file revision's link names a
//! changeset; each changeset's manifest is the null node or one the manifest log holds ("changeset refers to unknown
//! revision"); each path a changeset names has a file log the listing names and that holds a revision ("empty or
//! missing"), so a damaged fncache or file index cannot drop a file's history unseen; and every revision keeps its
//! revlog's own number, since a node a log holds twice would be passed over and shift the numbers after it.
//!
//! **The repository's state beside its history** (A-649 part 2), read after the logs as 7.2 reads it: the phase roots
//! (`phaseroots`, as `phases._readroots` reads it: a line it cannot read refused, as it aborts; a root whose node the
//! changelog lacks dropped and counted, as it drops one; a public root passed over, since it changes nothing), the
//! bookmarks (`.hg/bookmarks`, or the store's under `bookmarksinstore`, as `bookmarks.bmstore` reads them: a malformed
//! line passed over and counted, as it warns and passes over, a node the changelog lacks dropped and counted, a name
//! given twice its later node), and the obsolescence markers (`obsstore`, checked whole as `obsolete.obsstore` reads it,
//! empty when empty). The phase roots and bookmarks are the repository's mutable state, which the register records
//! (A-642's logs; a row of the index is never rewritten), so they come back typed in [`Imported`]; the markers are facts
//! never changed, kept as the index's marker rows in the obsstore's order, each alone in the obsstore's format.
//!
//! **Refused by kind**, where 7.2 would read on: a censored revision (its text a tombstone), an ellipsis revision (a
//! narrow clone's shortened history) and an externally stored one (lfs), whose texts do not hash to their nodes and
//! whose pieces come later (§4.17.3 pieces 10 and 11); and, as 7.2 refuses them, revision flags 7.2 does not know
//! ("incompatible revision flag").
//!
//! **Bounds.** `room` is the most the import holds at once, which its caller derives from the request's budget: the
//! requirements and the listing for the whole walk; one revlog's group while that log is read; one text's rebuild at a
//! time, beside the text before it. No revlog is held whole (A-649 part 3): a split revlog's index and data, and a
//! docket's files, are read by ranges, an entry or a chunk a read through the revlog reader's one algorithm over a
//! ranged source (`revlog::Outline`, `revlog::Source`), and a text is rebuilt from the text before it where its chain
//! passes through that one, as 7.2's revision cache does, rather than from its chain's base; only an inline revlog,
//! which 7.2 keeps under 128 KiB, is read whole. Each scope is charged to a copy of the room its enclosing scope left, since what it held is
//! freed when it ends. The texts kept are charged to the content store's budget, and the rows to the same budget by the
//! shared index, whose tail is sealed when due after each group, as the shared rule says.
//!
//! **Owed** (GAPS): the register's first entry recording the phase roots and bookmarks the import returns (until then
//! an import is not served); a unit a host's pending read refused retried at the next turn rather than ending the
//! import; the listing and the obsstore read in slices too; and the server's adapter from §4.15's host seam.

use std::borrow::Cow;
use std::collections::BTreeMap;

use slates_vcs::{ContentId, ContentRefused, ContentStore, IndexRefused};

use crate::changeset::{self, Changeset};
use crate::file_index;
use crate::filerev;
use crate::index::{self, HgIndex, Log, NewChangeset, NewRevision};
use crate::node::{self, CollisionAttack};
use crate::obsmarkers;
use crate::phases::{Phase, PhaseHead};
use crate::read::to_u64;
use crate::revlog::{self, Entry, NONE, Outline, Slices, Source};
use crate::store::{self, FncacheRefused, Layout, Listing, RequiresRefused, Shape};
use crate::{Admission, NULL_NODE, Node, PastAdmission};

/// Format: the requirements file's name, under `.hg` and under `.hg/store`.
const REQUIRES: &[u8] = b"requires";

/// Format: the store's directory under `.hg`.
const STORE: &[u8] = b"store";

/// Format: the fncache's name at the store's root, opened unencoded, as 7.2's `fncache` opens it.
const FNCACHE: &[u8] = b"fncache";

/// Format: the changelog's radix.
const CHANGELOG: &[u8] = b"00changelog";

/// Format: the manifest log's radix (a flat manifest's; tree manifests are refused by their requirement).
const MANIFEST: &[u8] = b"00manifest";

/// Format: the directory of every file log's radix.
const DATA: &[u8] = b"data/";

/// Format: the extension of a revlog's index file, or of its docket.
const INDEX_EXTENSION: &[u8] = b".i";

/// Format: the extension of a version 1 revlog's data file.
const DATA_EXTENSION: &[u8] = b".d";

/// Format: the extension of a docketed revlog's index file, after `-` and its id (`docket.index_filepath`).
const DOCKET_INDEX: &[u8] = b".idx";

/// Format: the extension of a docketed revlog's data file, after `-` and its id (`docket.data_filepath`).
const DOCKET_DATA: &[u8] = b".dat";

/// Format: what joins a docketed revlog's radix to its files' ids, and what ends the radix before a long extension
/// (`_split_revlog_ext`).
const ID_SEPARATOR: u8 = b'-';

/// Format: what ends the radix before a short extension.
const PERIOD: u8 = b'.';

/// Format: a path's separator.
const SLASH: u8 = b'/';

/// Format: the extensions of a revlog's files (`REVLOG_FILES_EXT`); 7.2's fncache walk passes over any other file.
const REVLOG_EXTENSIONS: [&[u8]; 7] = [b".i", b".idx", b".d", b".dat", b".n", b".nd", b".sda"];

/// Format: the extensions whose files carry `-` and an id after their radix (`REVLOG_FILES_LONG_EXT`).
const LONG_EXTENSIONS: [&[u8]; 4] = [b".nd", b".idx", b".dat", b".sda"];

/// Format: the names a path's component may not be: the directory itself and its parent.
const DOT_NAMES: [&[u8]; 2] = [b".", b".."];

/// Format: the bytes no tracked path holds: a manifest line's NUL and line end, and the carriage return 7.2's
/// `splitlines` also ends a line at.
const FORBIDDEN: &[u8] = b"\0\n\r";

/// Format: an index file's header, its first four bytes (`>I`; `INDEX_HEADER`).
const HEADER_BYTES: usize = 4;

/// Format: a revlog's version, the header's low 16 bits.
const VERSION_MASK: u32 = 0xFFFF;

/// Format: version 1 (`REVLOGV1`).
const VERSION_1: u32 = 1;

/// Format: the versions whose `.i` is a docket: revlog version 2 (`REVLOGV2`, `0xDEAD`) and the changelog's version 2
/// (`CHANGELOGV2`, `0xD34D`).
const DOCKETED: [u32; 2] = [0xDEAD, 0xD34D];

/// Format: version 1's inline flag (`FLAG_INLINE_DATA`): each chunk follows its entry in the index file.
const FLAG_INLINE: u32 = 1 << 16;

/// Format: a censored revision (`REVISION_FLAG_CENSORED`, `interfaces/repository.py`).
const CENSORED: u16 = 1 << 15;

/// Format: an ellipsis revision (`REVISION_FLAG_ELLIPSIS`).
const ELLIPSIS: u16 = 1 << 14;

/// Format: an externally stored revision (`REVISION_FLAG_EXTSTORED`).
const EXTSTORED: u16 = 1 << 13;

/// Format: every revision flag 7.2 knows (`REVISION_FLAGS_KNOWN`, bits 6 to 15).
const KNOWN_FLAGS: u16 = 0xFFC0;

/// Format: the extra naming a changeset's branch (`changelog.py`).
const BRANCH: &[u8] = b"branch";

/// Format: the branch of a changeset without that extra (`_defaultextra`).
const DEFAULT_BRANCH: &[u8] = b"default";

/// Format: the extra a closing changeset holds.
const CLOSE: &[u8] = b"close";

/// Format: the store's phase roots (`phases.py`).
const PHASEROOTS: &[u8] = b"phaseroots";

/// Format: the bookmarks file, under `.hg` or, with `bookmarksinstore`, the store (`bookmarks.py`).
const BOOKMARKS: &[u8] = b"bookmarks";

/// Format: the store's obsolescence markers (`obsolete.py`).
const OBSSTORE: &[u8] = b"obsstore";

/// Format: the bytes Python's `bytes.split()` and `bytes.strip()` take for whitespace.
const PYTHON_WHITESPACE: &[u8] = b" \t\n\r\x0b\x0c";

/// Format: the end of a line, as a file's lines are read.
const NEWLINE: u8 = b'\n';

/// Format: what parts a bookmark's node from its name (`split(b' ', 1)`).
const SPACE: u8 = b' ';

/// Format: a phase's number is written in decimal.
const DECIMAL_RADIX: u32 = 10;

/// Why the host did not hand a file over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileRefused<R> {
  /// The file is longer than the room the import has left: refused before a byte of it was held.
  PastAdmission(PastAdmission),
  /// The host's own refusal (§4.15's `HostError`: a fetch still pending on an S3 base, the host unavailable).
  Host(R),
}

/// A repository's files as the import reads them (§4.15's read seam): read-only, a file under the repository's `.hg`
/// directory named by its path's names, its length asked, then its bytes read by ranges (A-649 part 3), so no revlog is
/// held whole. The server adapts the base plane's host seam to it, opening each of the path's names in turn, never a
/// path string. Every name is plain: never empty, `.` or `..`, and holding no `/` and no NUL.
pub trait RepositoryFiles {
  /// The host's refusal, carried through untouched.
  type Refused;

  /// The length of the file at `path` under `.hg`, given as its names (`requires`; `store`, `data`, `a.txt.i`); `None`
  /// when no file is there.
  fn len(&mut self, path: &[&[u8]]) -> Result<Option<u64>, FileRefused<Self::Refused>>;

  /// `len` bytes of the file at `path` from `offset`, their length charged to `room` before they are held; fewer only
  /// where the file ends, and `None` when no file is there.
  fn read(
    &mut self,
    path: &[&[u8]],
    offset: u64,
    len: u64,
    room: &mut Admission,
  ) -> Result<Option<Vec<u8>>, FileRefused<Self::Refused>>;
}
/// An imported store: the repository's index, and how much of each kind it holds.
#[derive(Debug, Default)]
pub struct Imported {
  /// The index: every log of the store, its texts in the content store.
  pub index: HgIndex,
  /// The changesets.
  pub changesets: u32,
  /// The manifest revisions.
  pub manifests: u32,
  /// The file logs.
  pub file_logs: u64,
  /// The file revisions, over every file log.
  pub file_revisions: u64,
  /// The roots of the draft, secret, archived and internal phases, each a changeset the changelog holds, sorted by
  /// phase and node: the repository's state, for the register to record.
  pub phase_roots: Vec<PhaseHead>,
  /// Phase roots dropped, as 7.2 drops them: their nodes are not the changelog's.
  pub dropped_phase_roots: u64,
  /// The bookmarks, each its name and a changeset the changelog holds, sorted by name: for the register's greens.
  pub bookmarks: Vec<(Vec<u8>, Node)>,
  /// Bookmarks dropped, as 7.2 drops them: their nodes are not the changelog's.
  pub dropped_bookmarks: u64,
  /// Bookmark lines passed over, as 7.2 passes them over with "malformed line in .hg/bookmarks".
  pub malformed_bookmarks: u64,
  /// The obsolescence markers, kept as the index's marker rows in the obsstore's order.
  pub markers: u64,
  /// Texts rebuilt from the text before them, where their chains passed through it (7.2's revision cache): the count
  /// that shows the cache ran.
  pub rebuilt_from_cache: u64,
}

/// Why an import was refused. Nothing it took is left held: its index is released and its holds given back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportRefused<R> {
  /// The host refused a read, for its own reason; for a fetch still pending the caller runs the import again once
  /// the fetch lands.
  Host(R),
  /// More than the room the caller gave the import: a file, the listing, a log's group or a text's rebuild.
  PastAdmission(PastAdmission),
  /// The requirements refused.
  Requirements(RequiresRefused),
  /// A file the repository names that is not there: the store's requirements under `share-safe`, the index file of a
  /// revlog the listing names, or a file a file index's docket names. Its path under `.hg`, as the host was asked.
  Missing {
    /// The path.
    path: Box<[u8]>,
  },
  /// A path the import would not hand the host: a name `.` or `..`, or holding a NUL. Its path under `.hg`.
  UnsafePath {
    /// The path.
    path: Box<[u8]>,
  },
  /// The fncache refused.
  Fncache(FncacheRefused),
  /// The file index refused.
  FileIndex(file_index::Refused),
  /// An fncache entry naming a revlog's file outside `data/` (a tree manifest's, in a store that requires none).
  Entry {
    /// The entry.
    entry: Box<[u8]>,
  },
  /// A listed path no manifest could hold: empty, or holding a NUL, a line feed or a carriage return.
  UnusablePath {
    /// The path.
    path: Box<[u8]>,
  },
  /// A path long enough to be hashed whose bytes hold a known SHA-1 collision attack.
  PathName(CollisionAttack),
  /// A revlog the reader refused.
  Revlog {
    /// The revlog's radix (`00changelog`, `data/a.txt`).
    revlog: Box<[u8]>,
    /// The reader's refusal.
    refused: revlog::Refused,
  },
  /// A censored revision.
  Censored {
    /// The revlog's radix.
    revlog: Box<[u8]>,
    /// The revision.
    rev: u32,
  },
  /// An ellipsis revision.
  Ellipsis {
    /// The revlog's radix.
    revlog: Box<[u8]>,
    /// The revision.
    rev: u32,
  },
  /// An externally stored revision.
  ExternallyStored {
    /// The revlog's radix.
    revlog: Box<[u8]>,
    /// The revision.
    rev: u32,
  },
  /// Revision flags 7.2 does not know.
  UnknownFlags {
    /// The revlog's radix.
    revlog: Box<[u8]>,
    /// The revision.
    rev: u32,
    /// The flags.
    flags: u16,
  },
  /// A `phaseroots` line 7.2 would not read: not two fields, a phase it does not define, or a node that is not forty
  /// hexadecimal digits (it aborts on each).
  PhaseRoots {
    /// The line's number, from 1.
    line: u64,
  },
  /// An obsstore 7.2 would not read.
  Markers(obsmarkers::Refusal),
  /// A marker that does not write alone in its obsstore's own format.
  MarkerUnencodable {
    /// The marker's place in the obsstore, from 0.
    marker: u64,
    /// The encoder's refusal.
    refusal: obsmarkers::EncodeRefusal,
  },
  /// A changeset 7.2 would not read.
  Changeset {
    /// The changeset.
    rev: u32,
    /// The reader's refusal.
    refusal: changeset::Refusal,
  },
  /// A file revision whose metadata block 7.2 would not read.
  FileRevision {
    /// The revlog's radix.
    revlog: Box<[u8]>,
    /// The revision.
    rev: u32,
    /// The reader's refusal.
    refusal: filerev::Refusal,
  },
  /// A revision whose link names no changeset.
  Link {
    /// The revlog's radix.
    revlog: Box<[u8]>,
    /// The revision.
    rev: u32,
    /// The link.
    link: u32,
  },
  /// A revision whose node the log holds already.
  Duplicate {
    /// The revlog's radix.
    revlog: Box<[u8]>,
    /// The revision.
    rev: u32,
  },
  /// A changeset whose manifest the manifest log does not hold.
  UnknownManifest {
    /// The changeset.
    rev: u32,
    /// The manifest's node.
    node: Node,
  },
  /// A path a changeset names whose file log the listing lacks.
  Unlisted {
    /// The path.
    path: Box<[u8]>,
  },
  /// A path a changeset names whose file log holds no revision.
  EmptyLog {
    /// The path.
    path: Box<[u8]>,
  },
  /// A changeset the import appended that the index does not answer: the index and its store disagree.
  Unindexed {
    /// The changeset.
    rev: u32,
  },
  /// The index refused a group, or the seal after one.
  Index(index::Refused),
  /// The content store refused a text (its budget).
  Content(ContentRefused),
  /// The import was refused before, or advanced after its end: it does nothing more.
  Ended,
  /// Giving back what a refused import held was refused too; `cause` is the import's refusal.
  Unreleased {
    /// The import's refusal.
    cause: Box<ImportRefused<R>>,
    /// The release's.
    release: IndexRefused,
  },
}

impl<R> From<PastAdmission> for ImportRefused<R> {
  fn from(past: PastAdmission) -> Self {
    Self::PastAdmission(past)
  }
}

impl<R> From<FileRefused<R>> for ImportRefused<R> {
  fn from(refused: FileRefused<R>) -> Self {
    match refused {
      FileRefused::PastAdmission(past) => Self::PastAdmission(past),
      FileRefused::Host(host) => Self::Host(host),
    }
  }
}

impl<R> From<RequiresRefused> for ImportRefused<R> {
  fn from(refused: RequiresRefused) -> Self {
    Self::Requirements(refused)
  }
}

impl<R> From<FncacheRefused> for ImportRefused<R> {
  fn from(refused: FncacheRefused) -> Self {
    match refused {
      FncacheRefused::PastAdmission(past) => Self::PastAdmission(past),
      refused => Self::Fncache(refused),
    }
  }
}

impl<R> From<file_index::Refused> for ImportRefused<R> {
  fn from(refused: file_index::Refused) -> Self {
    match refused {
      file_index::Refused::PastAdmission(past) => Self::PastAdmission(past),
      refused => Self::FileIndex(refused),
    }
  }
}

impl<R> From<CollisionAttack> for ImportRefused<R> {
  fn from(attack: CollisionAttack) -> Self {
    Self::PathName(attack)
  }
}

impl<R> From<index::Refused> for ImportRefused<R> {
  fn from(refused: index::Refused) -> Self {
    Self::Index(refused)
  }
}

impl<R> From<ContentRefused> for ImportRefused<R> {
  fn from(refused: ContentRefused) -> Self {
    Self::Content(refused)
  }
}

/// The owner's clock, monotonic nanoseconds: the shard's (§4.3). The import has none of its own.
pub trait Clock {
  /// Now.
  fn now_ns(&mut self) -> u64;
}

/// One turn of an import (A-649 part 3): the owner's clock, and the nanoseconds the turn may spend, §4.3's step budget,
/// the owner's to derive. A turn does one unit of work whatever its quantum.
pub struct Turn<'t> {
  /// The owner's clock.
  pub clock: &'t mut dyn Clock,
  /// The nanoseconds the turn may spend.
  pub quantum_ns: u64,
}

/// What a turn left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
  /// More of the store to import.
  More,
  /// The store is imported; [`Import::into_imported`] hands it over.
  Done,
}

/// Which log a stage reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Which {
  /// The changelog.
  Changelog,
  /// The manifest log.
  Manifest,
  /// The file log of the listed path at this place.
  File(usize),
}

/// Where an import stands.
enum Stage {
  /// A log is to be opened.
  Open(Which),
  /// A log's revisions are being read.
  Log(Box<LogInProgress>),
  /// Each changeset's manifest is being held to the manifest log, from this changeset.
  Check(u32),
  /// The repository's state is to be read: its phase roots, bookmarks and markers.
  State,
  /// The store is imported.
  Done,
  /// The import was refused or its index taken back: it does nothing more.
  Ended,
}

/// One log being read: which, its radix, its revlog, its next revision, the last text rebuilt, and the room its
/// revisions are rebuilt within.
struct LogInProgress {
  /// Which log.
  which: Which,
  /// Its radix.
  radix: Vec<u8>,
  /// What its revlog is.
  outline: Outline,
  /// Its revlog's bytes, whole or by ranges.
  bytes: Bytes,
  /// The next revision to read.
  next: u32,
  /// The last revision rebuilt, and its text.
  cached: Option<(u32, Vec<u8>)>,
  /// The room left after its revlog was opened.
  room: Admission,
}

/// An import under way (A-649 part 3; module docs): what it has taken so far, and where it stands. It is advanced a
/// turn at a time ([`Import::advance`]), each turn doing units of work until the owner's quantum is spent: one
/// revision rebuilt, kept and appended to the index; one changeset's manifest held to the manifest log; a log opened;
/// the repository's state read. Between turns it holds no borrow of the host or the store. An import not taken to its
/// end is given back with [`Import::abandon`]; a refused one has given back everything already.
pub struct Import {
  /// How the store lists its file logs and names its files.
  shape: Shape,
  /// The listed paths, sorted.
  listed: Vec<Vec<u8>>,
  /// Whether a changeset names each listed path.
  named: Vec<bool>,
  /// The room the requirements and the listing left.
  room: Admission,
  /// What the import has taken: the index and its counts.
  imported: Imported,
  /// Where it stands.
  stage: Stage,
  /// Turns taken.
  turns: u64,
  /// Units of work done.
  units: u64,
}

/// A clock that stands still, so a turn of it ends only with its work: [`import`]'s.
struct Still;

impl Clock for Still {
  fn now_ns(&mut self) -> u64 {
    0
  }
}

/// Imports the store of the repository whose `.hg` directory `files` reads into a new index, its texts kept in `store`,
/// every log in 7.2's order (module docs), holding at most `room` at once, in one call: an [`Import`] advanced in one
/// turn that never ends before its work (the thin facade R6 allows; a shard advances an [`Import`] a turn at a time).
/// Refused, it leaves `store` as it found it.
pub fn import<F: RepositoryFiles>(
  files: &mut F,
  store: &mut ContentStore,
  room: Admission,
) -> Result<Imported, ImportRefused<F::Refused>> {
  let mut import = Import::start(files, room)?;
  let turn = Turn {
    clock: &mut Still,
    quantum_ns: u64::MAX,
  };
  import.advance(files, store, turn)?;
  import.into_imported().ok_or(ImportRefused::Ended)
}

/// Releases a refused import's index; the refusal, or the release's beside it.
fn release<R>(
  index: HgIndex,
  store: &mut ContentStore,
  refused: ImportRefused<R>,
) -> ImportRefused<R> {
  match index.release(store) {
    Ok(()) => refused,
    Err(release) => ImportRefused::Unreleased {
      cause: Box::new(refused),
      release,
    },
  }
}

impl Import {
  /// Starts an import: the requirements and the listing read, nothing kept in any store yet, every log still to read.
  pub fn start<F: RepositoryFiles>(
    files: &mut F,
    room: Admission,
  ) -> Result<Import, ImportRefused<F::Refused>> {
    let mut walk = room;
    let shape = read_shape(files, &mut walk)?;
    let listed = list_file_logs(files, shape, &mut walk)?;
    walk.charge_items::<bool>(listed.len())?;
    let named = vec![false; listed.len()];
    Ok(Import {
      shape,
      listed,
      named,
      room: walk,
      imported: Imported::default(),
      stage: Stage::Open(Which::Changelog),
      turns: 0,
      units: 0,
    })
  }

  /// One turn: units of work until `turn`'s quantum is spent on its clock, at least one; `Done` once the store is
  /// imported. Refused, the index is released, and the import ends.
  pub fn advance<F: RepositoryFiles>(
    &mut self,
    files: &mut F,
    store: &mut ContentStore,
    turn: Turn<'_>,
  ) -> Result<Progress, ImportRefused<F::Refused>> {
    let start = turn.clock.now_ns();
    self.turns = self.turns.saturating_add(1);
    loop {
      match self.unit(files, store) {
        Ok(Progress::Done) => return Ok(Progress::Done),
        Ok(Progress::More) => {}
        Err(refused) => return Err(self.refuse(store, refused)),
      }
      if turn.clock.now_ns().saturating_sub(start) >= turn.quantum_ns {
        return Ok(Progress::More);
      }
    }
  }

  /// The turns taken.
  pub fn turns(&self) -> u64 {
    self.turns
  }

  /// The units of work done.
  pub fn units(&self) -> u64 {
    self.units
  }

  /// The imported store, once a turn has answered `Done`.
  pub fn into_imported(self) -> Option<Imported> {
    matches!(self.stage, Stage::Done).then_some(self.imported)
  }

  /// Gives back everything an import not taken to its end holds: its index's rows and contents.
  pub fn abandon(self, store: &mut ContentStore) -> Result<(), IndexRefused> {
    self.imported.index.release(store)
  }

  /// Ends a refused import: its index released, the refusal answered.
  fn refuse<R>(&mut self, store: &mut ContentStore, refused: ImportRefused<R>) -> ImportRefused<R> {
    self.stage = Stage::Ended;
    release(core::mem::take(&mut self.imported.index), store, refused)
  }

  /// One unit of work.
  fn unit<F: RepositoryFiles>(
    &mut self,
    files: &mut F,
    store: &mut ContentStore,
  ) -> Result<Progress, ImportRefused<F::Refused>> {
    self.units = self.units.saturating_add(1);
    self.stage = match core::mem::replace(&mut self.stage, Stage::Ended) {
      Stage::Open(which) => self.open(files, which)?,
      Stage::Log(log) => self.log_unit(files, store, log)?,
      Stage::Check(rev) => self.check(store, rev)?,
      Stage::State => {
        self.state(files, store)?;
        Stage::Done
      }
      Stage::Done => Stage::Done,
      Stage::Ended => return Err(ImportRefused::Ended),
    };
    Ok(match self.stage {
      Stage::Done => Progress::Done,
      _ => Progress::More,
    })
  }

  /// Opens a log's revlog.
  fn open<F: RepositoryFiles>(
    &mut self,
    files: &mut F,
    which: Which,
  ) -> Result<Stage, ImportRefused<F::Refused>> {
    let (radix, listed) = match which {
      Which::Changelog => (CHANGELOG.to_vec(), false),
      Which::Manifest => (MANIFEST.to_vec(), false),
      Which::File(at) => {
        let path = self.listed.get(at).map_or(&[][..], Vec::as_slice);
        ([DATA, path].concat(), true)
      }
    };
    let mut room = self.room;
    let (outline, bytes) = open_revlog(files, self.shape, &radix, listed, &mut room)?;
    Ok(Stage::Log(Box::new(LogInProgress {
      which,
      radix,
      outline,
      bytes,
      next: 0,
      cached: None,
      room,
    })))
  }

  /// One revision of a log read, kept and appended; or the log finished.
  fn log_unit<F: RepositoryFiles>(
    &mut self,
    files: &mut F,
    store: &mut ContentStore,
    mut log: Box<LogInProgress>,
  ) -> Result<Stage, ImportRefused<F::Refused>> {
    if log.next >= log.outline.len() {
      return self.finish_log(&log);
    }
    match log.which {
      Which::Changelog => self.changeset_unit(files, store, &mut log)?,
      Which::Manifest | Which::File(_) => self.revision_unit(files, store, &mut log)?,
    }
    log.next = log.next.saturating_add(1);
    Ok(Stage::Log(log))
  }

  /// A log read whole: its count kept, a named file's log held to hold a revision, and the next stage.
  fn finish_log<R>(&mut self, log: &LogInProgress) -> Result<Stage, ImportRefused<R>> {
    let revisions = log.outline.len();
    Ok(match log.which {
      Which::Changelog => {
        self.imported.changesets = revisions;
        Stage::Open(Which::Manifest)
      }
      Which::Manifest => {
        self.imported.manifests = revisions;
        Stage::Check(0)
      }
      Which::File(at) => {
        if revisions == 0 && self.named.get(at).copied().unwrap_or_default() {
          let path = self.listed.get(at).map_or(&[][..], Vec::as_slice);
          return Err(ImportRefused::EmptyLog {
            path: Box::from(path),
          });
        }
        self.imported.file_logs = self.imported.file_logs.saturating_add(1);
        let total = self
          .imported
          .file_revisions
          .saturating_add(u64::from(revisions));
        self.imported.file_revisions = total;
        self.next_file(at.saturating_add(1))
      }
    })
  }

  /// The stage of the listed file log at `at`, or the state's once every one is read.
  fn next_file(&self, at: usize) -> Stage {
    if at < self.listed.len() {
      Stage::Open(Which::File(at))
    } else {
      Stage::State
    }
  }

  /// Marks `path` named, or refuses it when the listing lacks it.
  fn name<R>(&mut self, path: &[u8]) -> Result<(), ImportRefused<R>> {
    let at = self
      .listed
      .binary_search_by(|known| known.as_slice().cmp(path))
      .map_err(|_| ImportRefused::Unlisted {
        path: Box::from(path),
      })?;
    if let Some(named) = self.named.get_mut(at) {
      *named = true;
    }
    Ok(())
  }

  /// One changeset: its text rebuilt and held to its node, its manifest, branch and closing read as 7.2 reads them,
  /// every path it names listed, kept and appended; its text then cached for the next.
  fn changeset_unit<F: RepositoryFiles>(
    &mut self,
    files: &mut F,
    store: &mut ContentStore,
    log: &mut LogInProgress,
  ) -> Result<(), ImportRefused<F::Refused>> {
    let rev = log.next;
    let mut room = log.room;
    let cached = log.cached.take();
    let reading = Reading {
      log: Log::Changelog,
      radix: &log.radix,
      outline: &log.outline,
      bytes: &log.bytes,
    };
    let view = cached.as_ref().map(|(at, text)| (*at, text.as_slice()));
    let counter = &mut self.imported.rebuilt_from_cache;
    let (entry, text) = rebuild(files, &reading, rev, view, &mut room, counter)?;
    drop(cached);
    let read = changeset::decode(&text).map_err(|refusal| changeset_refused(rev, refusal))?;
    let manifest = read
      .manifest_node()
      .map_err(|refusal| changeset_refused(rev, refusal))?;
    let (branch, closed) =
      branch_of(&read, &mut room).map_err(|refusal| changeset_refused(rev, refusal))?;
    for path in read.file_names() {
      self.name(path)?;
    }
    let (p1, p2) = parents(files, &reading, &entry, &mut room)?;
    let mut held = Held::default();
    let index = &mut self.imported.index;
    let outcome = held.put(store, &text).and_then(|text_id| {
      let planned = PlannedChangeset {
        node: entry.node,
        p1,
        p2,
        flags: entry.flags,
        len: to_u64(text.len()),
        text: text_id,
        manifest,
        closed,
        branch,
      };
      append_changeset(index, store, rev, &planned)
    });
    held.give_back(store, outcome)?;
    log.cached = Some((rev, text));
    Ok(())
  }

  /// One manifest or file revision: its text rebuilt and held to its node, its link a changeset the index holds, a file
  /// revision's block apart from its bytes, both kept and appended; its text then cached for the next.
  fn revision_unit<F: RepositoryFiles>(
    &mut self,
    files: &mut F,
    store: &mut ContentStore,
    log: &mut LogInProgress,
  ) -> Result<(), ImportRefused<F::Refused>> {
    let rev = log.next;
    let mut room = log.room;
    let cached = log.cached.take();
    let path = match log.which {
      Which::File(at) => self.listed.get(at).map(Vec::as_slice),
      Which::Changelog | Which::Manifest => None,
    };
    let reading = Reading {
      log: path.map_or(Log::Manifest, Log::File),
      radix: &log.radix,
      outline: &log.outline,
      bytes: &log.bytes,
    };
    let view = cached.as_ref().map(|(at, text)| (*at, text.as_slice()));
    let counter = &mut self.imported.rebuilt_from_cache;
    let (entry, text) = rebuild(files, &reading, rev, view, &mut room, counter)?;
    drop(cached);
    let link = self
      .imported
      .index
      .changeset(store, entry.link)
      .map(|row| row.node)
      .ok_or_else(|| ImportRefused::Link {
        revlog: Box::from(reading.radix),
        rev,
        link: entry.link,
      })?;
    let (p1, p2) = parents(files, &reading, &entry, &mut room)?;
    let (bytes, block) = match reading.log {
      Log::File(_) => {
        let split = filerev::split(&text).map_err(|refusal| ImportRefused::FileRevision {
          revlog: Box::from(reading.radix),
          rev,
          refusal,
        })?;
        (split.bytes, split.metadata)
      }
      Log::Changelog | Log::Manifest => (text.as_slice(), None),
    };
    let mut held = Held::default();
    let index = &mut self.imported.index;
    let outcome = keep_revision(store, &mut held, bytes, block).and_then(|(bytes, block)| {
      let row = NewRevision {
        node: entry.node,
        p1,
        p2,
        link,
        flags: entry.flags,
        len: to_u64(text.len()),
        bytes,
        block,
      };
      append_revision(index, store, &reading, rev, &row)
    });
    held.give_back(store, outcome)?;
    log.cached = Some((rev, text));
    Ok(())
  }

  /// One changeset's manifest held to the manifest log: the null node, or one it holds.
  fn check<R>(&mut self, store: &ContentStore, rev: u32) -> Result<Stage, ImportRefused<R>> {
    if rev >= self.imported.changesets {
      return Ok(self.next_file(0));
    }
    let index = &self.imported.index;
    let row = index
      .changeset(store, rev)
      .ok_or(ImportRefused::Unindexed { rev })?;
    if row.manifest != NULL_NODE && index.rev(store, Log::Manifest, &row.manifest).is_none() {
      return Err(ImportRefused::UnknownManifest {
        rev,
        node: row.manifest,
      });
    }
    Ok(Stage::Check(rev.saturating_add(1)))
  }

  /// The repository's state beside its history (module docs): its phase roots, bookmarks and markers.
  fn state<F: RepositoryFiles>(
    &mut self,
    files: &mut F,
    store: &mut ContentStore,
  ) -> Result<(), ImportRefused<F::Refused>> {
    self.phase_roots(files, store)?;
    self.bookmarks(files, store)?;
    self.imported.markers = self.markers(files, store)?;
    Ok(())
  }

  /// The store's phase roots into the import (module docs).
  fn phase_roots<F: RepositoryFiles>(
    &mut self,
    files: &mut F,
    store: &ContentStore,
  ) -> Result<(), ImportRefused<F::Refused>> {
    let mut room = self.room;
    let Some(bytes) = read_store_file(files, self.shape, PHASEROOTS, &mut room)? else {
      return Ok(());
    };
    let (roots, dropped) = read_phase_roots(&bytes, &self.imported.index, store)?;
    self.room.charge_items::<PhaseHead>(roots.len())?;
    self.imported.phase_roots = roots;
    self.imported.dropped_phase_roots = dropped;
    Ok(())
  }

  /// The repository's bookmarks into the import: `.hg/bookmarks`, or the store's under `bookmarksinstore` (module
  /// docs).
  fn bookmarks<F: RepositoryFiles>(
    &mut self,
    files: &mut F,
    store: &ContentStore,
  ) -> Result<(), ImportRefused<F::Refused>> {
    let mut room = self.room;
    let read = if self.shape.bookmarks_in_store {
      read_store_file(files, self.shape, BOOKMARKS, &mut room)?
    } else {
      read_whole(files, &[BOOKMARKS], &mut room)?
    };
    let Some(bytes) = read else {
      return Ok(());
    };
    let read = read_bookmarks(&bytes, &self.imported.index, store);
    let names = read
      .found
      .keys()
      .fold(0_usize, |total, name| total.saturating_add(name.len()));
    self.room.charge(to_u64(names))?;
    self
      .room
      .charge_items::<(Vec<u8>, Node)>(read.found.len())?;
    self.imported.bookmarks = read.found.into_iter().collect();
    self.imported.dropped_bookmarks = read.dropped;
    self.imported.malformed_bookmarks = read.malformed;
    Ok(())
  }

  /// The store's obsolescence markers, checked whole, kept as the index's marker rows in the obsstore's order, each
  /// alone in the obsstore's format; how many.
  fn markers<F: RepositoryFiles>(
    &mut self,
    files: &mut F,
    store: &mut ContentStore,
  ) -> Result<u64, ImportRefused<F::Refused>> {
    let mut room = self.room;
    let read = read_store_file(files, self.shape, OBSSTORE, &mut room)?;
    let Some(bytes) = read.filter(|bytes| !bytes.is_empty()) else {
      return Ok(0);
    };
    let markers = obsmarkers::decode(&bytes).map_err(ImportRefused::Markers)?;
    room.charge_items::<ContentId>(markers.len())?;
    let mut held = Held::default();
    let index = &mut self.imported.index;
    let outcome = keep_markers(store, &markers, &mut held, room).and_then(|kept| {
      index
        .append_markers(store, &kept)
        .map_err(ImportRefused::from)
    });
    held
      .give_back(store, outcome)
      .map(|numbers| to_u64(numbers.len()))
  }
}

/// Each marker kept alone in its obsstore's format, its encoding charged to a copy of `room` first.
fn keep_markers<R>(
  store: &mut ContentStore,
  markers: &obsmarkers::Markers<'_>,
  held: &mut Held,
  room: Admission,
) -> Result<Vec<ContentId>, ImportRefused<R>> {
  let format = markers.format();
  let mut kept = Vec::with_capacity(markers.len());
  for (number, marker) in (0_u64..).zip(markers.iter()) {
    let unencodable = |refusal| ImportRefused::MarkerUnencodable {
      marker: number,
      refusal,
    };
    let len = obsmarkers::encoded_len(format, &[marker]).map_err(unencodable)?;
    let mut marker_room = room;
    marker_room.charge(len)?;
    let mut alone = Vec::new();
    obsmarkers::encode(format, &[marker], &mut alone).map_err(unencodable)?;
    kept.push(held.put(store, &alone)?);
  }
  Ok(kept)
}

/// A revision's bytes and block kept as content.
fn keep_revision<R>(
  store: &mut ContentStore,
  held: &mut Held,
  bytes: &[u8],
  block: Option<&[u8]>,
) -> Result<(ContentId, Option<ContentId>), ImportRefused<R>> {
  let bytes = held.put(store, bytes)?;
  let block = block.map(|block| held.put(store, block)).transpose()?;
  Ok((bytes, block))
}

/// The path the store path `path` (unencoded) has under the store's encoding.
fn store_path<R>(shape: Shape, path: &[u8]) -> Result<StorePath, ImportRefused<R>> {
  StorePath::new(&shape.encoding.encode(path)?)
}

/// Reads the whole store file `path` (unencoded), named as the store's encoding names it.
fn read_store_file<F: RepositoryFiles>(
  files: &mut F,
  shape: Shape,
  path: &[u8],
  room: &mut Admission,
) -> Result<Option<Vec<u8>>, ImportRefused<F::Refused>> {
  let path = store_path(shape, path)?;
  read_whole(files, &path.names(), room)
}

/// A revlog's outline and bytes (A-649 part 3): its `.i`'s length asked and its header read; a docket, an inline index
/// (7.2 keeps one under 128 KiB) or an index shorter than a header read whole; a split index and its data file read by
/// ranges. An absent `.i` is an empty revlog, unless the listing named the revlog.
fn open_revlog<F: RepositoryFiles>(
  files: &mut F,
  shape: Shape,
  radix: &[u8],
  listed: bool,
  room: &mut Admission,
) -> Result<(Outline, Bytes), ImportRefused<F::Refused>> {
  let index = store_path(shape, &[radix, INDEX_EXTENSION].concat())?;
  let Some(len) = files.len(&index.names())? else {
    if listed {
      return Err(ImportRefused::Missing {
        path: index.shown(),
      });
    }
    let outline = Outline::of_index(&[], room).map_err(|refused| revlog_refused(radix, refused))?;
    return Ok((outline, Bytes::Whole(Vec::new())));
  };
  let head_len = len.min(to_u64(HEADER_BYTES));
  let head = files.read(&index.names(), 0, head_len, room)?;
  let header = head
    .unwrap_or_default()
    .first_chunk::<HEADER_BYTES>()
    .map(|header| u32::from_be_bytes(*header));
  match header {
    Some(header) if DOCKETED.contains(&(header & VERSION_MASK)) => {
      docketed(files, shape, radix, &index, len, room)
    }
    Some(header) if header & VERSION_MASK != VERSION_1 || header & FLAG_INLINE == 0 => {
      split(files, shape, radix, index, header, len)
    }
    _ => {
      let whole = files
        .read(&index.names(), 0, len, room)?
        .unwrap_or_default();
      let outline =
        Outline::of_index(&whole, room).map_err(|refused| revlog_refused(radix, refused))?;
      Ok((outline, Bytes::Whole(whole)))
    }
  }
}

/// A split version 1 revlog, its index and data files read by ranges (any other version refused by its header).
fn split<F: RepositoryFiles>(
  files: &mut F,
  shape: Shape,
  radix: &[u8],
  index: StorePath,
  header: u32,
  len: u64,
) -> Result<(Outline, Bytes), ImportRefused<F::Refused>> {
  let outline = Outline::of_split(header, len).map_err(|refused| revlog_refused(radix, refused))?;
  let data = store_path(shape, &[radix, DATA_EXTENSION].concat())?;
  let data_len = files.len(&data.names())?.unwrap_or_default();
  let bytes = Bytes::Ranged {
    index: (index, len),
    data: (data, data_len),
  };
  Ok((outline, bytes))
}

/// A docketed revlog: the docket read whole, the index and data files it names read by ranges, each held to hold the
/// bytes the docket says (7.2: "too few index data"; an absent one holds none).
fn docketed<F: RepositoryFiles>(
  files: &mut F,
  shape: Shape,
  radix: &[u8],
  path: &StorePath,
  len: u64,
  room: &mut Admission,
) -> Result<(Outline, Bytes), ImportRefused<F::Refused>> {
  let docket = files.read(&path.names(), 0, len, room)?.unwrap_or_default();
  let read = revlog::read_docket(&docket).map_err(|refused| revlog_refused(radix, refused))?;
  let outline = Outline::of_docket(&read).map_err(|refused| revlog_refused(radix, refused))?;
  let index = store_path(
    shape,
    &[radix, &[ID_SEPARATOR], read.index_id, DOCKET_INDEX].concat(),
  )?;
  let data = store_path(
    shape,
    &[radix, &[ID_SEPARATOR], read.data_id, DOCKET_DATA].concat(),
  )?;
  let (index_end, data_end) = (read.index_end, read.data_end);
  holds(files, radix, &index, index_end, "an index file")?;
  holds(files, radix, &data, data_end, "a data file")?;
  let bytes = Bytes::Ranged {
    index: (index, index_end),
    data: (data, data_end),
  };
  Ok((outline, bytes))
}

/// Refuses a docket's file shorter than the docket says.
fn holds<F: RepositoryFiles>(
  files: &mut F,
  radix: &[u8],
  path: &StorePath,
  end: u64,
  what: &'static str,
) -> Result<(), ImportRefused<F::Refused>> {
  if files.len(&path.names())?.unwrap_or_default() < end {
    return Err(revlog_refused(radix, revlog::Refused::Truncated { what }));
  }
  Ok(())
}

/// Revision `rev`'s entry and text: its flags refused first where 7.2 refuses them, or where its text does not hash to
/// its node; then its text rebuilt and held to its node by the reader, from the text rebuilt before it where its chain
/// passes through that one (7.2's revision cache, counted in `cache_hits`), the cached text charged to `room` beside the
/// rebuild.
fn rebuild<F: RepositoryFiles>(
  files: &mut F,
  reading: &Reading<'_>,
  rev: u32,
  cached: Option<(u32, &[u8])>,
  room: &mut Admission,
  cache_hits: &mut u64,
) -> Result<(Entry, Vec<u8>), ImportRefused<F::Refused>> {
  let radix = reading.radix;
  let mut reader = Reader::new(files, reading.bytes);
  let entry = reading
    .outline
    .entry(&mut reader, rev, room)
    .map_err(|refused| read_refused(radix, refused))?;
  refuse_flags(radix, rev, entry.flags)?;
  if let Some((_, text)) = cached {
    room.charge(to_u64(text.len()))?;
  }
  let (text, from_cache) = reading
    .outline
    .text(&mut reader, rev, cached, room)
    .map_err(|refused| read_refused(radix, refused))?;
  if from_cache {
    *cache_hits = cache_hits.saturating_add(1);
  }
  Ok((entry, text))
}

/// A revision's parents' nodes, from its revlog's entries.
fn parents<F: RepositoryFiles>(
  files: &mut F,
  reading: &Reading<'_>,
  entry: &Entry,
  room: &mut Admission,
) -> Result<(Node, Node), ImportRefused<F::Refused>> {
  let mut reader = Reader::new(files, reading.bytes);
  let mut node = |rev: u32| -> Result<Node, ImportRefused<F::Refused>> {
    if rev == NONE {
      return Ok(NULL_NODE);
    }
    reading
      .outline
      .entry(&mut reader, rev, room)
      .map(|parent| parent.node)
      .map_err(|refused| read_refused(reading.radix, refused))
  };
  let p1 = node(entry.p1)?;
  Ok((p1, node(entry.p2)?))
}

/// The store's shape from the repository's requirements: `requires`, and the store's own under `share-safe`.
fn read_shape<F: RepositoryFiles>(
  files: &mut F,
  room: &mut Admission,
) -> Result<Shape, ImportRefused<F::Refused>> {
  let mut layout = Layout::default();
  layout.read(&read_whole(files, &[REQUIRES], room)?.unwrap_or_default())?;
  if layout.share_safe {
    let in_store =
      read_whole(files, &[STORE, REQUIRES], room)?.ok_or_else(|| ImportRefused::Missing {
        path: in_store(REQUIRES),
      })?;
    layout.read(&in_store)?;
  }
  Ok(layout.shape()?)
}

/// A path under `.hg` from its path under the store.
fn in_store(path: &[u8]) -> Box<[u8]> {
  [STORE, &[SLASH], path].concat().into_boxed_slice()
}

/// A whole file: its length asked, then its bytes read from its start; `None` when no file is there.
fn read_whole<F: RepositoryFiles>(
  files: &mut F,
  names: &[&[u8]],
  room: &mut Admission,
) -> Result<Option<Vec<u8>>, ImportRefused<F::Refused>> {
  let Some(len) = files.len(names)? else {
    return Ok(None);
  };
  Ok(files.read(names, 0, len, room)?)
}

/// A store file's path as the host is asked for it: its names under `.hg`, `store` first, each checked plain, and the
/// path as a refusal names it.
#[derive(Clone, Debug)]
struct StorePath {
  /// The names.
  names: Vec<Vec<u8>>,
  /// The path under `.hg`.
  shown: Vec<u8>,
}

impl StorePath {
  /// The path of `path` under the store: its names split at `/`, an empty one passed over as the host's own walk of a
  /// path passes over it, and a name that is `.` or `..` or holds a NUL refused before the host is asked.
  fn new<R>(path: &[u8]) -> Result<StorePath, ImportRefused<R>> {
    let mut names = vec![STORE.to_vec()];
    for name in path
      .split(|byte| *byte == SLASH)
      .filter(|name| !name.is_empty())
    {
      if DOT_NAMES.contains(&name) || name.contains(&0) {
        return Err(ImportRefused::UnsafePath {
          path: in_store(path),
        });
      }
      names.push(name.to_vec());
    }
    if names.len() < 2 {
      return Err(ImportRefused::UnsafePath {
        path: in_store(path),
      });
    }
    Ok(StorePath {
      names,
      shown: in_store(path).into_vec(),
    })
  }

  /// The names, as the host takes them.
  fn names(&self) -> Vec<&[u8]> {
    self.names.iter().map(Vec::as_slice).collect()
  }

  /// The path as a refusal names it.
  fn shown(&self) -> Box<[u8]> {
    self.shown.clone().into_boxed_slice()
  }
}

/// Reads the whole file at `path` under the store ([`StorePath::new`]'s names).
fn read_in_store<F: RepositoryFiles>(
  files: &mut F,
  path: &[u8],
  room: &mut Admission,
) -> Result<Option<Vec<u8>>, ImportRefused<F::Refused>> {
  read_whole(files, &StorePath::new(path)?.names(), room)
}
/// Every file log's path the store lists, sorted, once; each one a manifest could hold.
fn list_file_logs<F: RepositoryFiles>(
  files: &mut F,
  shape: Shape,
  room: &mut Admission,
) -> Result<Vec<Vec<u8>>, ImportRefused<F::Refused>> {
  let mut paths = match shape.listing {
    Listing::Fncache => fncache_paths(files, room)?,
    Listing::FileIndex => file_index_paths(files, room)?,
  };
  let unusable =
    |path: &&Vec<u8>| path.is_empty() || path.iter().any(|byte| FORBIDDEN.contains(byte));
  if let Some(path) = paths.iter().find(unusable) {
    return Err(ImportRefused::UnusablePath {
      path: Box::from(path.as_slice()),
    });
  }
  paths.sort_unstable();
  paths.dedup();
  Ok(paths)
}

/// The paths of the file logs an fncache lists: each revlog's radix under `data/`, its other files passed over.
fn fncache_paths<F: RepositoryFiles>(
  files: &mut F,
  room: &mut Admission,
) -> Result<Vec<Vec<u8>>, ImportRefused<F::Refused>> {
  let Some(bytes) = read_in_store(files, FNCACHE, room)? else {
    return Ok(Vec::new());
  };
  let entries = store::read_fncache(&bytes, room)?;
  room.charge(to_u64(bytes.len()))?;
  room.charge_items::<Vec<u8>>(entries.len())?;
  let mut paths = Vec::with_capacity(entries.len());
  for entry in &entries {
    let Some(radix) = radix_len(entry) else {
      continue;
    };
    let path = entry
      .strip_prefix(DATA)
      .and_then(|_| entry.get(DATA.len()..radix))
      .ok_or_else(|| ImportRefused::Entry {
        entry: Box::from(entry.as_slice()),
      })?;
    paths.push(path.to_vec());
  }
  Ok(paths)
}

/// The length of the radix of the revlog file `name`, as 7.2's `_split_revlog_ext` cuts it; `None` for a file that is
/// no revlog's, or a long extension with no `-` before it.
fn radix_len(name: &[u8]) -> Option<usize> {
  let extension = REVLOG_EXTENSIONS
    .iter()
    .find(|extension| name.ends_with(extension))?;
  let separator = if LONG_EXTENSIONS.contains(extension) {
    ID_SEPARATOR
  } else {
    PERIOD
  };
  name.iter().rposition(|byte| *byte == separator)
}

/// The paths a file index lists, its files read as its docket names them.
fn file_index_paths<F: RepositoryFiles>(
  files: &mut F,
  room: &mut Admission,
) -> Result<Vec<Vec<u8>>, ImportRefused<F::Refused>> {
  let Some(bytes) = read_in_store(files, file_index::DOCKET, room)? else {
    return Ok(Vec::new());
  };
  let docket = file_index::read_docket(&bytes)?;
  let list = read_named(files, docket.list_file(), room)?;
  let meta = read_named(files, docket.meta_file(), room)?;
  let tree = read_named(files, docket.tree_file(), room)?;
  Ok(file_index::paths(&docket, &list, &meta, &tree, room)?)
}

/// A file a docket names, which must be there; empty when it names none.
fn read_named<F: RepositoryFiles>(
  files: &mut F,
  name: Option<Vec<u8>>,
  room: &mut Admission,
) -> Result<Vec<u8>, ImportRefused<F::Refused>> {
  let Some(name) = name else {
    return Ok(Vec::new());
  };
  read_in_store(files, &name, room)?.ok_or_else(|| ImportRefused::Missing {
    path: in_store(&name),
  })
}

/// A changeset planned for the changelog's group, its branch owned until the group is written.
struct PlannedChangeset {
  /// The node.
  node: Node,
  /// The first parent's node.
  p1: Node,
  /// The second parent's node.
  p2: Node,
  /// 7.2's index flags.
  flags: u16,
  /// The text's length.
  len: u64,
  /// The text, kept.
  text: ContentId,
  /// The manifest's node.
  manifest: Node,
  /// Whether it closes its branch.
  closed: bool,
  /// Its branch.
  branch: Vec<u8>,
}

/// The holds one unit's puts took, given back once its rows hold the contents or the unit is refused.
#[derive(Default)]
struct Held(Vec<ContentId>);

impl Held {
  /// Keeps `bytes` as content (an object of any length, cut into windows past one) and records the hold.
  fn put<R>(
    &mut self,
    store: &mut ContentStore,
    bytes: &[u8],
  ) -> Result<ContentId, ImportRefused<R>> {
    let identity = store.put_object(bytes)?;
    self.0.push(identity);
    Ok(identity)
  }

  /// Gives back every hold; `outcome`, or a give-back's refusal beside it.
  fn give_back<T, R>(
    self,
    store: &mut ContentStore,
    outcome: Result<T, ImportRefused<R>>,
  ) -> Result<T, ImportRefused<R>> {
    let mut first = Ok(());
    for identity in &self.0 {
      first = first.and(store.release(identity).map(drop));
    }
    match (outcome, first) {
      (outcome, Ok(())) => outcome,
      (Ok(_), Err(refused)) => Err(ImportRefused::Content(refused)),
      (Err(cause), Err(refused)) => Err(ImportRefused::Unreleased {
        cause: Box::new(cause),
        release: IndexRefused::Content(refused),
      }),
    }
  }
}

/// The phase roots of a `phaseroots` file (module docs); with how many were dropped.
fn read_phase_roots<R>(
  bytes: &[u8],
  index: &HgIndex,
  store: &ContentStore,
) -> Result<(Vec<PhaseHead>, u64), ImportRefused<R>> {
  let mut roots = Vec::new();
  let mut dropped: u64 = 0;
  for (line, text) in (1_u64..).zip(bytes.split_inclusive(|byte| *byte == NEWLINE)) {
    let root = phase_root(text).ok_or(ImportRefused::PhaseRoots { line })?;
    if root.phase == Phase::Public {
      continue;
    }
    if index.rev(store, Log::Changelog, &root.node).is_none() {
      dropped = dropped.saturating_add(1);
      continue;
    }
    roots.push(root);
  }
  roots.sort_unstable_by_key(|root| (root.phase.number(), root.node));
  roots.dedup();
  Ok((roots, dropped))
}

/// One `phaseroots` line's root: two fields between Python's whitespace, a phase 7.2 defines in decimal digits and a
/// node's forty hexadecimal digits.
fn phase_root(text: &[u8]) -> Option<PhaseHead> {
  let mut fields = text
    .split(|byte| PYTHON_WHITESPACE.contains(byte))
    .filter(|field| !field.is_empty());
  let (Some(number), Some(hex), None) = (fields.next(), fields.next(), fields.next()) else {
    return None;
  };
  Some(PhaseHead {
    phase: Phase::from_number(decimal(number)?)?,
    node: node::from_hex(hex)?,
  })
}

/// Decimal digits' value, `None` for anything else or a value past an `i32`.
fn decimal(field: &[u8]) -> Option<i32> {
  let radix = i32::try_from(DECIMAL_RADIX).ok()?;
  let value = field.iter().try_fold(0_i32, |value, digit| {
    let digit = i32::try_from(char::from(*digit).to_digit(DECIMAL_RADIX)?).ok()?;
    value.checked_mul(radix)?.checked_add(digit)
  })?;
  (!field.is_empty()).then_some(value)
}

/// What a bookmarks file holds.
#[derive(Default)]
struct ReadBookmarks {
  /// Each name's node, the later line's for a name given twice.
  found: BTreeMap<Vec<u8>, Node>,
  /// Lines whose nodes the changelog does not hold.
  dropped: u64,
  /// Lines passed over as malformed.
  malformed: u64,
}

/// The bookmarks a bookmarks file holds (module docs): each line stripped of Python's whitespace, an empty one passed
/// over; a node in hexadecimal, a space and the name.
fn read_bookmarks(bytes: &[u8], index: &HgIndex, store: &ContentStore) -> ReadBookmarks {
  let mut read = ReadBookmarks::default();
  for text in bytes.split_inclusive(|byte| *byte == NEWLINE) {
    let line = strip(text);
    if line.is_empty() {
      continue;
    }
    let parted = line.iter().position(|byte| *byte == SPACE).and_then(|at| {
      let hex = line.get(..at)?;
      let hexadecimal = hex.len().is_multiple_of(2) && hex.iter().all(u8::is_ascii_hexdigit);
      Some((hex, line.get(at.saturating_add(1)..)?)).filter(|_| hexadecimal)
    });
    let Some((hex, name)) = parted else {
      read.malformed = read.malformed.saturating_add(1);
      continue;
    };
    match node::from_hex(hex).filter(|node| index.rev(store, Log::Changelog, node).is_some()) {
      Some(node) => {
        read.found.insert(name.to_vec(), node);
      }
      None => read.dropped = read.dropped.saturating_add(1),
    }
  }
  read
}

/// `text` without Python's whitespace at either end (`bytes.strip()`).
fn strip(text: &[u8]) -> &[u8] {
  let kept = |byte: &u8| !PYTHON_WHITESPACE.contains(byte);
  let start = text.iter().position(kept).unwrap_or(text.len());
  let end = text
    .iter()
    .rposition(kept)
    .map_or(start, |at| at.saturating_add(1));
  text.get(start..end).unwrap_or_default()
}

/// One revlog being read, as a view: where it goes in the index, its radix, its outline and its bytes.
struct Reading<'r> {
  /// Its log in the index.
  log: Log<'r>,
  /// Its radix, for refusals.
  radix: &'r [u8],
  /// What it is.
  outline: &'r Outline,
  /// Its bytes, whole or by ranges.
  bytes: &'r Bytes,
}

/// A revlog's bytes as the import holds them (A-649 part 3): whole (an inline revlog, which 7.2 keeps under 128 KiB, or
/// one too short for a header), or named for ranged reads, each file with the bytes the reader may read of it.
enum Bytes {
  /// The whole index file, its chunks inline.
  Whole(Vec<u8>),
  /// The index and data files, read by ranges: a split index and its data file, or the files a docket names.
  Ranged {
    /// The index file, and its bytes to read: its length, or the docket's index size.
    index: (StorePath, u64),
    /// The data file, and its bytes to read.
    data: (StorePath, u64),
  },
}

/// A revlog's bytes as the reader reads them: whole, or by ranges through the host.
enum Reader<'r, F> {
  /// Whole.
  Whole(Slices<'r>),
  /// By ranges.
  Ranged {
    /// The host.
    files: &'r mut F,
    /// The index file, and its bytes to read.
    index: (&'r StorePath, u64),
    /// The data file, and its bytes to read.
    data: (&'r StorePath, u64),
  },
}

impl<'r, F> Reader<'r, F> {
  /// The reader of `bytes`, through `files` when they are read by ranges.
  fn new(files: &'r mut F, bytes: &'r Bytes) -> Self {
    match bytes {
      Bytes::Whole(index) => Reader::Whole(Slices { index, data: index }),
      Bytes::Ranged { index, data } => Reader::Ranged {
        files,
        index: (&index.0, index.1),
        data: (&data.0, data.1),
      },
    }
  }
}

/// A read's refusal: the reader's, or the host's.
#[derive(Debug)]
enum ReadRefused<R> {
  /// The revlog reader refused.
  Reader(revlog::Refused),
  /// The host refused.
  File(FileRefused<R>),
}

impl<R> From<revlog::Refused> for ReadRefused<R> {
  fn from(refused: revlog::Refused) -> Self {
    Self::Reader(refused)
  }
}

impl<F: RepositoryFiles> Source for Reader<'_, F> {
  type Error = ReadRefused<F::Refused>;

  fn index(
    &mut self,
    at: u64,
    len: usize,
    admission: &mut Admission,
  ) -> Result<Cow<'_, [u8]>, Self::Error> {
    match self {
      Self::Whole(slices) => Ok(slices.index(at, len, admission)?),
      Self::Ranged { files, index, .. } => {
        ranged(&mut **files, *index, at, len, admission, "an entry")
      }
    }
  }

  fn data(
    &mut self,
    at: u64,
    len: usize,
    admission: &mut Admission,
  ) -> Result<Cow<'_, [u8]>, Self::Error> {
    match self {
      Self::Whole(slices) => Ok(slices.data(at, len, admission)?),
      Self::Ranged { files, data, .. } => {
        ranged(&mut **files, *data, at, len, admission, "a chunk")
      }
    }
  }
}

/// `len` bytes of a file from `at`, within the bytes the reader may read of it and read whole; otherwise refused as
/// cut short.
fn ranged<F: RepositoryFiles>(
  files: &mut F,
  (path, end): (&StorePath, u64),
  at: u64,
  len: usize,
  admission: &mut Admission,
  what: &'static str,
) -> Result<Cow<'static, [u8]>, ReadRefused<F::Refused>> {
  let wanted = to_u64(len);
  if at.checked_add(wanted).is_none_or(|stop| stop > end) {
    return Err(revlog::Refused::Truncated { what }.into());
  }
  match files
    .read(&path.names(), at, wanted, admission)
    .map_err(ReadRefused::File)?
  {
    Some(bytes) if bytes.len() == len => Ok(Cow::Owned(bytes)),
    _ => Err(revlog::Refused::Truncated { what }.into()),
  }
}

/// A read's refusal in the import's terms: the reader's naming its revlog, the host's as the host gave it.
fn read_refused<R>(radix: &[u8], refused: ReadRefused<R>) -> ImportRefused<R> {
  match refused {
    ReadRefused::Reader(refused) => revlog_refused(radix, refused),
    ReadRefused::File(refused) => refused.into(),
  }
}
/// Refuses a revision whose flags mark a text that does not hash to its node, or that 7.2 does not know.
fn refuse_flags<R>(radix: &[u8], rev: u32, flags: u16) -> Result<(), ImportRefused<R>> {
  let revlog = || Box::from(radix);
  let refused = if flags & CENSORED != 0 {
    ImportRefused::Censored {
      revlog: revlog(),
      rev,
    }
  } else if flags & ELLIPSIS != 0 {
    ImportRefused::Ellipsis {
      revlog: revlog(),
      rev,
    }
  } else if flags & EXTSTORED != 0 {
    ImportRefused::ExternallyStored {
      revlog: revlog(),
      rev,
    }
  } else if flags & !KNOWN_FLAGS != 0 {
    ImportRefused::UnknownFlags {
      revlog: revlog(),
      rev,
      flags,
    }
  } else {
    return Ok(());
  };
  Err(refused)
}

/// A changeset's branch and whether it closes it, as 7.2 reads its extras: the last `branch`, else `default`; any
/// `close`.
fn branch_of(
  read: &Changeset<'_>,
  room: &mut Admission,
) -> Result<(Vec<u8>, bool), changeset::Refusal> {
  let mut buffer = Vec::new();
  let extras = read.extras(room, &mut buffer)?;
  let branch = extras
    .iter()
    .rev()
    .find(|(key, _)| *key == BRANCH)
    .map_or(DEFAULT_BRANCH, |(_, value)| *value)
    .to_vec();
  Ok((branch, extras.iter().any(|(key, _)| *key == CLOSE)))
}

/// Appends one changeset, which must be numbered `rev`, its revlog's own.
fn append_changeset<R>(
  index: &mut HgIndex,
  store: &mut ContentStore,
  rev: u32,
  changeset: &PlannedChangeset,
) -> Result<(), ImportRefused<R>> {
  let new = NewChangeset {
    node: changeset.node,
    p1: changeset.p1,
    p2: changeset.p2,
    flags: changeset.flags,
    len: changeset.len,
    text: changeset.text,
    manifest: changeset.manifest,
    closed: changeset.closed,
    branch: &changeset.branch,
  };
  let numbers = index.append_changesets(store, &[new])?;
  settle(index, store, CHANGELOG, rev, &numbers)
}

/// Appends one manifest or file revision, which must be numbered `rev`, its revlog's own.
fn append_revision<R>(
  index: &mut HgIndex,
  store: &mut ContentStore,
  reading: &Reading<'_>,
  rev: u32,
  revision: &NewRevision,
) -> Result<(), ImportRefused<R>> {
  let numbers = index.append_revisions(store, reading.log, &[*revision])?;
  settle(index, store, reading.radix, rev, &numbers)
}

/// Holds an append's numbers to its revlog's own from `first` (the index passes over a node a log holds already, and
/// would number what follows otherwise than 7.2), then seals the index's tail when it is due.
fn settle<R>(
  index: &mut HgIndex,
  store: &mut ContentStore,
  radix: &[u8],
  first: u32,
  numbers: &[u32],
) -> Result<(), ImportRefused<R>> {
  if let Some((rev, _)) = (first..).zip(numbers).find(|&(rev, &number)| rev != number) {
    return Err(ImportRefused::Duplicate {
      revlog: Box::from(radix),
      rev,
    });
  }
  if index.rows().seal_due(store) {
    index.rows().seal(store).map_err(index::Refused::from)?;
  }
  Ok(())
}
/// A reader's refusal in the import's terms: its room's by kind, any other naming its revlog.
fn revlog_refused<R>(radix: &[u8], refused: revlog::Refused) -> ImportRefused<R> {
  match refused {
    revlog::Refused::PastAdmission(past) => ImportRefused::PastAdmission(past),
    refused => ImportRefused::Revlog {
      revlog: Box::from(radix),
      refused,
    },
  }
}

/// A changeset reader's refusal in the import's terms: its room's by kind, any other naming its changeset.
fn changeset_refused<R>(rev: u32, refusal: changeset::Refusal) -> ImportRefused<R> {
  match refusal {
    changeset::Refusal::PastAdmission(past) => ImportRefused::PastAdmission(past),
    refusal => ImportRefused::Changeset { rev, refusal },
  }
}
