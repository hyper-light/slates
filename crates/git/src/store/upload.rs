//! upload-pack's pack, streamed from the store (A-639; §4.17.2 "A fetch", item 3, and "Pieces (git)", piece 7's "packs
//! from fragments").
//!
//! **What it sends.** The negotiation (A-615) answers a fetch with runs of a green's versions, `after + 1` to
//! `through`, and with objects one by one; this module takes that answer through a trait ([`Answered`]), so neither
//! module names the other, and sends exactly the objects it names, each once, in three stages:
//! 1. **The runs**, from their versions' fragments in order. The request's filter applies to a run's objects (A-615:
//!    "the request's filter applies to their objects"), so before the header counts them each run's objects are
//!    gathered: every object of its versions' D(k) when the filter sends everything, otherwise each by its row; and
//!    the run's head whenever the run brought it, since it is the want, which git sends whatever the filter. A run is
//!    exact because each of its versions fast-forwarded the one before, which the negotiation checked: D(M+1) to D(N)
//!    is then what N reaches and M does not (§4.17.2 item 3).
//! 2. **The cover**: the objects still to send that kept parts hold. The greens a head of whose versions is still
//!    wanted are taken largest base pack first; for each, its base pack's two parts (commits, trees and tags; then
//!    blobs), then the fragments of each version after the base (of every version, without one), each part sending
//!    the objects of it still wanted: copied whole when they are all of it, read entry by entry otherwise (A-653), its
//!    wanted entries alone judged. So a clone is the base pack and then the fragments after it (§4.17.2: "A clone is
//!    the base pack and then the fragments after version C"), with no clone path of its own; a filter that sends no
//!    blobs takes the parts without blobs whole, the "fragment classes" of §4.17.2, and a `blob:limit` clone takes the
//!    small blobs from the parts of blobs entry by entry, as kept, deltas and all. Then each object still wanted is
//!    offered the parts of the version its row says brought it, once each: a partial clone's later fetch of blobs by
//!    their ids wants no head, and its blobs are found so.
//! 3. **The rest**, which no part covered (a version not yet encoded), in position order. An object pushed in a pack
//!    is first sent on as its entry lies there (A-859, git's pack reuse: `pack-objects` reuses an object's packed
//!    entry before it searches): a whole entry's compressed data copied, once it inflates to the object its name says;
//!    a delta whose base precedes it in this pack (sent already, or placed by this stage before it, its base placed
//!    first where it is still wanted, as git's `write_one` writes a delta's base first), once the delta applied to its
//!    base gives the object its name says, as an offset delta to a base this stage placed where the client said
//!    `ofs-delta` and by its base's name otherwise; a whole entry whenever it has one (main 01a20182's ruling of
//!    2026-11-22 05:38: an object with a stored form is never searched). Every other object is encoded by the fragments'
//!    encoder: a delta against an earlier one of its kind within the learned window and depth, at the learned level,
//!    the rules copied from the repository when the stream begins. An object past a window is searched by the same
//!    rules against the earlier ones past a window of its kind, the nearest first, as many as the learned window
//!    tries, through [`super::Search`] a window at a time (**Slices**), when the request's admission can hold the
//!    search (A-653 continued (4)): the object's bytes, the largest candidate's, the search's index and delta
//!    ([`super::Search::held_most`]), and the best delta kept while another candidate is searched, all charged with a
//!    [`Hold`] before the first byte is read and given back once the entry is sent. Where the admission cannot hold
//!    it, the object is sent whole and counted (`loose_unsearched`). git holds the same three for a trial and counts
//!    them as its window's memory: the target's bytes, the source's and the source's index (`pack-objects.c` at
//!    v2.50.1, `try_delta`, lines 2774, 2803 and 2813). It searches every object of a type it can read but those past
//!    `core.bigFileThreshold` (`get_object_details`, lines 2541-2544, and `prepare_pack`, line 3326), and its
//!    `pack.windowMemory`, unlimited by default (line 253), lets the oldest candidates go past it but never the nearest
//!    (`find_deltas`, lines 2913-2919); slates takes neither constant (R3): the request's own admission decides. git
//!    also skips a trial its sizes rule out: a source shorter than the target by half the target or more (less a
//!    hash's length, the half scaled by the depth the source's chain leaves), or over 32 times the target's length
//!    (`try_delta`, lines 2737-2755); slates' encoder tries every candidate its learned window holds, objects within a
//!    window and past one alike, and the window learns what the trials save.
//!
//! The header counts the wanted objects; every one is sent exactly once, since each stage sends only what is still
//! wanted and takes it out; the trailer is the checksum of every byte before it, made as the bytes pass, and the count
//! sent is checked against the header's before it is written.
//!
//! **What the client can read.** A kept part is copied byte for byte, its offset deltas valid wherever it lands
//! (§4.17.2: "A fragment's offset deltas stay valid when fragments are joined"), when every object it holds is to be
//! sent, the client said `ofs-delta` or the part has no offset delta, and the client reads every base its reference
//! deltas name. A base is read when it is sent already. For a run's part it is also read when it comes later in this
//! pack, as a reference delta may name any object of its pack \[B: gitformat-pack, "ref-delta"\] and no cycle can
//! form, since every version of a run fast-forwarded the one before; and, for a client that asked a thin pack, when the
//! pack does not carry it at all. Such a base lies in the tree the version before had, so outside the pack it is in
//! what the client's version reaches; under a filter it is held only where the filter would have sent it, so a client
//! cloned under the same filter has it. A part the cover took names bases sent before it alone: across a rewrite two
//! versions may name each other's objects. Otherwise the part is read entry by entry: an entry kept whole is copied;
//! an offset delta whose base this pack carries is copied with its distance measured again, or with its base named by
//! id for a client without `ofs-delta`, and named by id where its base was sent before the part; a reference delta
//! whose base the client reads is copied; any other delta is sent whole from its object, so a client that did not ask
//! for `thin-pack` is never sent a base it lacks ("A client MUST NOT request the 'thin-pack' capability if it cannot
//! turn a thin pack into a self-contained pack" \[B: gitprotocol-capabilities\]). Sending the object whole rather than
//! its base ahead of it sends nothing the client has, so the pack holds exactly the answer's objects.
//!
//! **Slices.** [`Upload::advance`] does units of work until the owner's quantum is spent, at least one, and checks the
//! owner's clock after each: a window of a kept part copied; a window of an object deflated (an object sent whole, or
//! one past a window, is deflated a window a unit, and so is a delta found for one); a window of an object past a
//! window, or of a candidate base, read for its search, or a step of the search over the two, a window's bytes at most;
//! one object at most a window long encoded, its delta trials within the learned window; a window's worth of the
//! answer's handles read; one object of a run judged by its row; a part's wanted positions marked among its entries
//! until a window's bytes of the part's entries in position order are read (each read an entry of the order and the
//! entry it names), each found by galloping ahead from where the
//! last was (the positions come ascending), so a part's reading costs what it sends and never the whole part; one wanted entry of a part judged; one object still wanted offered its version's
//! parts. So no unit outgrows a window and the learned window's trials, and nothing is held whole but what a search
//! holds, an object past a window and a candidate, while the request's admission holds them (**What it sends** 3):
//! between turns the stream holds no borrow of the store or the repository, names what it reads by green, version,
//! class and content, and holds the wanted and sent sets as runs of positions (§4.17.2 item 3).
//!
//! **A base pack made again** while a stream reads one of its parts entry by entry ends the stream refused
//! ([`StoreRefused::BaseRebuilt`]), never a pack of two base packs' entries; the owner makes base packs between
//! streams. A part copied whole names its content, which holds the bytes it was made with.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::mem::size_of;

use slates_vcs::{ContentId, ContentStore};

use crate::entry::{self, Header, PACK_HEADER_LEN, Representation};
use crate::object_name::{Hasher, Kind, ObjectFormat, ObjectId};
use crate::out::Out;
use crate::pack;
use crate::{Admission, Hold, PastAdmission};

use super::deltas::{Search, SearchStep, delta_most};
use super::fragment::{self, Encoder, FragmentRefused, Learning, Placed, Stored, ToEncode};
use super::learn::{Clock, Exchange, Levels, Window};
use super::packed::{self, InPack};
use super::positions::Positions;
use super::push::StoredAt;
use super::repository::{Kept, Repository, StoreRefused};
use super::rows::{self, ObjectRow};

/// Format: git's four object types, each asked of a filter to learn whether it sends everything.
const KINDS: [Kind; 4] = [Kind::Commit, Kind::Tree, Kind::Blob, Kind::Tag];

/// A run of a green's versions the pack carries from their fragments (A-615's `Run`): versions `after + 1` to
/// `through` of `green`, the client holding version `after`'s head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Versions {
  /// The green.
  pub green: u64,
  /// The version whose head the client holds.
  pub after: u64,
  /// The version wanted.
  pub through: u64,
}

/// What a fetch's negotiation answered (A-615), and what its client reads: taken through a trait, so the store and the
/// negotiation name neither one another (A-640 implements it over the negotiation's answer).
pub trait Answered {
  /// The runs of versions the pack carries from their fragments, in order.
  fn runs(&self) -> &[Versions];
  /// The objects the pack carries one by one, by position, as runs (A-745).
  fn objects(&self) -> &Positions;
  /// Whether the client said `thin-pack`: a delta may name a base the client holds.
  fn thin(&self) -> bool;
  /// Whether the client said `ofs-delta`: a delta may name its base by its offset back.
  fn ofs_delta(&self) -> bool;
  /// Whether the request's filter sends an object of `kind` and `size` that a run brought (A-615's per-object filter,
  /// which decides by kind and size alone, sending a smaller object wherever it sends a larger one); every one with no
  /// filter.
  fn admits(&self, kind: Kind, size: u64) -> bool;
}

/// One turn of a stream: the owner's clock, its exchange (what a saved byte is worth in time), and the quantum the
/// turn may spend (§4.3's step budget, the owner's to derive).
pub struct Turn<'t> {
  /// The owner's clock.
  pub clock: &'t mut dyn Clock,
  /// The exchange the learned rules decide by.
  pub exchange: Exchange,
  /// The nanoseconds the turn may spend; one unit of work is done whatever it is.
  pub quantum_ns: u64,
}

/// What a turn left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
  /// More of the pack to send.
  More,
  /// The pack is sent, its trailer last.
  Done,
}

/// What a stream did, for a test to see each path taken.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UploadCounters {
  /// Turns taken.
  pub turns: u64,
  /// Kept parts (fragments and base packs' parts) copied whole, byte for byte.
  pub parts_copied: u64,
  /// Kept parts read entry by entry.
  pub parts_by_entry: u64,
  /// Entries of parts read entry by entry that were judged: the wanted ones alone.
  pub entries_judged: u64,
  /// Entries copied with the header they were kept with.
  pub entries_copied: u64,
  /// Entries copied with a header written again: an offset delta's distance in this pack, or its base named by id.
  pub headers_rewritten: u64,
  /// Entries sent whole from their objects, since their base is none the client can read.
  pub made_whole: u64,
  /// Objects no part covered, encoded whole.
  pub loose_whole: u64,
  /// Objects no part covered, encoded as deltas.
  pub loose_deltas: u64,
  /// Objects past a window no part covered that had a candidate base, sent whole because the request's admission
  /// could not hold their search (module docs, **What it sends** 3); each is counted in `loose_whole` too.
  pub loose_unsearched: u64,
  /// Objects no part covered sent on as their pushed entry lies, whole, its compressed data copied (A-859, git's pack
  /// reuse).
  pub reused_whole: u64,
  /// Objects no part covered sent on as their pushed delta (A-859), its base sent before it in this pack.
  pub reused_deltas: u64,
  /// Objects whose pushed entry failed its check (its data not the object its name says), encoded instead (A-859).
  pub reuse_refused: u64,
  /// Objects no part covered that lie in no pack the store holds (kept as content), encoded with no entry to reuse
  /// (A-859).
  pub reuse_no_entry: u64,
  /// Objects no part covered whose pushed entry, or a delta's base, could not be read, encoded instead (A-859).
  pub reuse_unread: u64,
  /// Objects no part covered whose pushed entry's raw bytes did not match the CRC32 its row keeps (A-864), rebuilt to
  /// be checked.
  pub reuse_crc_missed: u64,
  /// The loose stage's reads of a pushed entry that lay before the one read before it in the same pack (A-864's piece 1:
  /// none where the stage reads each pack front to back).
  pub backward_reads: u64,
  /// Turns that stopped at a let-go window, each a window met the first time (A-864 piece 2): with
  /// `vcs_store.git_resident.stopped_brought`, a stall shows as stops without windows brought back.
  pub stops: u64,
  /// Objects no part covered whose pushed entry was rebuilt to be checked (inflated, a delta applied, named): those with
  /// a CRC32 miss or no kept entry (A-864: none, for a pushed pack's objects).
  pub reuse_rebuilt: u64,
  /// The most of the answer's kept units (runs, listed positions or table words) one unit of reading its objects read:
  /// a window's worth at most, however many the answer holds (module docs, **Slices**).
  pub objects_read_most: u64,
  /// The most of a part's entries in position order one unit of marking its wanted entries read: a window's worth of
  /// handles at most, however many entries the part holds (module docs, **Slices**).
  pub marks_read_most: u64,
}

/// What the loose stage does with an object's pushed entry (A-859).
enum Reuse {
  /// Sent on as it lies: its header written, what copies its data.
  Sent(Pending),
  /// A delta whose base, still wanted, is placed first: the base's position.
  BaseFirst(u64),
  /// Encoded instead.
  Encode,
  /// Its entry's read met a let-go window the stream had not stopped at before: the turn ends, the window to be brought
  /// back, the object taken again (A-864 piece 2).
  Stopped,
}

/// How a pushed delta goes on (A-859).
enum DeltaReuse {
  /// As this delta.
  As(Representation),
  /// After its base, still wanted, at this position.
  BaseFirst(u64),
  /// Not as it lies: encoded instead.
  No,
  /// Its object's read to check it met a let-go window, and the turn stopped (r165): taken again.
  Stopped,
}

/// Where the stream is (module docs).
enum Stage {
  /// Reading the answer's objects into the wanted set, from its kept unit `next` (a run, a listed position or a table
  /// word, as its form keeps them).
  Objects { next: usize },
  /// Gathering run `run`'s version `version`'s objects the filter sends, from position `from` on.
  Gather { run: usize, version: u64, from: u64 },
  /// The header.
  Header,
  /// Sending run `run`'s version `version`'s fragment of blobs or of the other kinds.
  Runs {
    run: usize,
    version: u64,
    blobs: bool,
  },
  /// Choosing the greens whose kept parts may cover what is left.
  CoverFrom,
  /// Offering green `greens[at]`'s `part` to the cover.
  Cover {
    greens: Vec<u64>,
    at: usize,
    part: Part,
  },
  /// Offering each object still wanted, from position `from` on, the parts of the version its row says brought it;
  /// `offered` the parts offered so far.
  Holders {
    from: u64,
    offered: BTreeSet<(u64, u64, bool)>,
  },
  /// Reading where each object no part covered lies, from position `from` on, into the loose stage's order (A-864's
  /// piece 1: a pack's objects in the pack's own order).
  Order { from: u64 },
  /// Sending what no part covered, from the loose stage's order's `from`th object on.
  Loose { from: u64 },
  /// The trailer.
  Trailer,
  /// Sent.
  Done,
  /// Refused: every later turn answers the same.
  Refused(StoreRefused),
}

/// A green's part the cover offers next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Part {
  /// Its base pack's part of blobs, or of the other kinds.
  Base { blobs: bool },
  /// Its version's fragment of blobs, or of the other kinds.
  Version { version: usize, blobs: bool },
}

/// Where a kept part is, named as the stream names it between turns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
  /// A version's fragment.
  Fragment {
    green: u64,
    version: usize,
    blobs: bool,
  },
  /// A base pack's part, with the content it was read from: one made again since is another.
  Base {
    green: u64,
    blobs: bool,
    content: ContentId,
  },
}

/// A part read entry by entry.
struct ByEntry {
  source: Source,
  /// The positions of its objects to send.
  keep: Positions,
  /// Whether it is a run's part, whose bases outside the pack a thin client holds; the cover's parts' never are.
  run: bool,
  /// Which of its entries to send.
  chosen: Chosen,
  /// The entry to look from next.
  next: usize,
  /// Where each of its entries sent begins in the pack, by the entry's index, ascending.
  moved: Vec<(usize, u64)>,
}

/// Which of a part's entries a reading sends (module docs, **Slices**).
enum Chosen {
  /// Every one: the whole part is wanted.
  Every,
  /// Those marked, a bit an entry by its index, marked from the wanted positions first, from `marking` on, a window's
  /// worth of the part's entries in position order read a unit; `marking` is `None` once every one is marked. Each is
  /// found ahead of `cursor`, where the one before it was, in the part's position order: the positions come ascending,
  /// so the search never goes back and all the marking costs what it marks.
  Marked {
    bits: Vec<u64>,
    marking: Option<u64>,
    cursor: usize,
  },
}

impl Chosen {
  /// The first entry at `from` or after it, of `entries`, the reading sends.
  fn next_at(&self, from: usize, entries: usize) -> Option<usize> {
    match self {
      Chosen::Every => (from < entries).then_some(from),
      Chosen::Marked { bits, .. } => next_bit(bits, from).filter(|index| *index < entries),
    }
  }
}

/// An object being deflated whole, or a delta made for one, a window a unit.
struct Deflating {
  from: Deflated,
  len: u64,
  /// Its window to feed next.
  next: u64,
  compress: super::deflaters::Deflater,
}

/// What a [`Deflating`] reads.
enum Deflated {
  /// A kept content.
  Content(ContentId),
  /// An object read whole where it lies in its pack (A-722's record v4): its bytes, kept for its deflate's windows.
  Read(Vec<u8>),
  /// A delta the stream made, and the admission's hold on what its search kept, given back after its last window.
  Made { delta: Vec<u8>, hold: Hold },
}

/// An object past a window that the loose stage sent: what a later one's search reads it by and names it by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Large {
  id: ObjectId,
  kind: Kind,
  len: u64,
  stored: StoredAt,
  /// Where its entry begins in the pack.
  at: u64,
  /// Its delta chain's depth.
  depth: u64,
}

/// An object past a window being searched (module docs, **What it sends** 3): its bytes read a window a unit, then
/// each candidate's, each searched a window's bytes a unit; the smallest delta shorter than the object wins.
struct Searching {
  /// The object.
  target: (ObjectId, ObjectRow, StoredAt),
  /// The admission's hold on everything the search keeps.
  hold: Hold,
  /// The object's bytes read so far.
  target_bytes: Vec<u8>,
  /// The candidates, the nearest first.
  candidates: Vec<Large>,
  /// The candidate being tried, by its index in `candidates`.
  trying: usize,
  /// Its bytes read so far.
  base_bytes: Vec<u8>,
  /// Its search, begun once its bytes are whole.
  search: Search,
  /// The owner's nanoseconds its trial has taken so far.
  trial_ns: u64,
  /// The best delta so far, and the index of the candidate it names.
  best: Option<(usize, Vec<u8>)>,
}

/// Bytes owed to the pack before anything else is judged.
enum Pending {
  /// The bytes `from` to `to` of a kept content `len` bytes long.
  Copy {
    content: ContentId,
    len: u64,
    from: u64,
    to: u64,
  },
  /// An object being deflated.
  Deflate(Deflating),
  /// An object past a window being searched.
  Search(Box<Searching>),
}

/// What a turn's units read: the repository, the store, the owner's clock and exchange, and the request's admission.
struct Context<'c> {
  repository: &'c Repository,
  store: &'c ContentStore,
  clock: &'c mut dyn Clock,
  exchange: Exchange,
  admission: &'c mut Admission,
}

/// upload-pack's pack, streamed (module docs).
pub struct Upload<A> {
  answer: A,
  format: ObjectFormat,
  /// Whether the filter sends every object (no filter, or one that sends the largest of each kind), so a run's objects
  /// need no row read.
  sends_all: bool,
  stage: Stage,
  /// The objects the pack carries and has not sent.
  wanted: Positions,
  /// The objects sent.
  sent: Positions,
  /// The runs' objects the filter sends, gathered before the header counts them.
  gathered: Positions,
  /// D(k) of the version gathered an object at a time (its green and version), read from its value once and kept until
  /// its objects are gathered.
  reading: Option<((u64, u64), Positions)>,
  /// The objects the header counts.
  counted: u64,
  /// The entries sent.
  entries: u64,
  /// The bytes sent so far: where the next entry begins.
  at: u64,
  hasher: Option<Hasher>,
  window: Window,
  levels: Levels,
  depth: u64,
  encoder: Encoder,
  /// For each kind, the objects past a window a later one's search may try, the nearest last.
  large: BTreeMap<Kind, VecDeque<Large>>,
  /// Where each object the loose stage sent begins in the pack, by its position (A-859: a reused delta names a base
  /// the loose stage sent by its offset back), a word an object, charged to the request's admission as it grows.
  loose_at: BTreeMap<u64, u64>,
  /// The positions the loose stage places before it goes on, the last first (A-859, git's `write_one`: a delta's base
  /// written before it): a pushed delta whose base is still wanted and not yet placed, and its base above it; as deep
  /// as the deepest chain any push carried (`Repository::learned_depth`), a word an object charged with the stage's.
  ahead: Vec<u64>,
  /// The objects no part covered, in the order the loose stage sends them ([`Upload::order_unit`]): each by its pack's
  /// ordinal (`packs`), its offset there and its position, those held whole last by position.
  order: Vec<(u64, u64, u64)>,
  /// Each pack the loose stage's objects lie in, by its ordinal in `order`, in the order first met.
  packs: BTreeMap<ContentId, u64>,
  /// The same packs by ordinal, each with its length.
  pack_lens: Vec<(ContentId, u64)>,
  /// The let-go window a unit met and stopped at, its work kept to be taken again once it is brought back (goal 93
  /// item 2a's look-ahead, A-864 piece 2): taken by [`Upload::waiting`].
  waiting: core::cell::Cell<Option<packed::Waited>>,
  /// The let-go window the stream last stopped at, by its pack and index, until a unit is done after it: a stop at the
  /// same window with nothing done between is read and refused rather than waited for again, so a stop always makes
  /// progress or ends (A-864 piece 2's progress); a window let go again behind the stream (piece 3) may be stopped at
  /// again once the stream has gone on.
  last_stop: Option<(ContentId, u64)>,
  /// Whether the stream has stopped at a let-go window at all: its look-ahead begins then ([`Upload::reading_next`]).
  stopped_any: bool,
  /// The loose stage's objects charged their placement and put back before they were placed, by a stop or their base
  /// going first: retaken, none is charged again. At most the ahead stack's and the one stopped.
  prepaid: BTreeSet<u64>,
  /// Where the loose stage last read a pushed entry: its pack and offset (counting [`UploadCounters::backward_reads`]).
  last_read: Option<(ContentId, u64)>,
  by_entry: Option<ByEntry>,
  pending: Option<Pending>,
  pending_crc: Option<PendingCrc>,
  counters: UploadCounters,
}

/// Incremental CRC32 verification state for a multi-window packed entry paused at a let-go window,
/// so [`Upload::reading_next`] advances across the entry's windows and `let_go_behind` keeps resident
/// windows bounded to a turn's look-ahead even for arbitrarily large blobs (A-864 piece 3).
#[derive(Debug)]
struct PendingCrc {
  place: InPack,
  header: Header,
  data: u64,
  hasher: crc32fast::Hasher,
  checked: u64,
}

impl<A: Answered> Upload<A> {
  /// The stream of the pack that answers `answer` from `repository`, encoding with a copy of the repository's learned
  /// rules.
  pub fn new(answer: A, repository: &Repository) -> Upload<A> {
    let (window, levels, depth) = repository.rules();
    let sends_all = KINDS.iter().all(|kind| answer.admits(*kind, u64::MAX));
    let encoder = Encoder::new(answer.ofs_delta());
    let format = repository.format();
    Upload {
      answer,
      format,
      sends_all,
      stage: Stage::Objects { next: 0 },
      wanted: Positions::new(),
      reading: None,
      sent: Positions::new(),
      gathered: Positions::new(),
      counted: 0,
      entries: 0,
      at: 0,
      hasher: Some(Hasher::pack_checksum(format)),
      window,
      levels,
      depth,
      encoder,
      large: BTreeMap::new(),
      loose_at: BTreeMap::new(),
      ahead: Vec::new(),
      order: Vec::new(),
      packs: BTreeMap::new(),
      pack_lens: Vec::new(),
      waiting: core::cell::Cell::new(None),
      last_stop: None,
      stopped_any: false,
      prepaid: BTreeSet::new(),
      last_read: None,
      by_entry: None,
      pending: None,
      pending_crc: None,
      counters: UploadCounters::default(),
    }
  }

  /// What the stream did.
  pub fn counters(&self) -> UploadCounters {
    self.counters
  }

  /// One turn: units of work until the turn's quantum is spent, at least one, the bytes put to `out` (module docs),
  /// what a search holds for a while charged to the request's `admission` and given back when it ends. Refused, every
  /// later turn is refused the same, and what was put is no pack.
  pub fn advance(
    &mut self,
    repository: &Repository,
    store: &ContentStore,
    turn: Turn<'_>,
    admission: &mut Admission,
    out: &mut impl Out,
  ) -> Result<Progress, StoreRefused> {
    let Turn {
      clock,
      exchange,
      quantum_ns,
    } = turn;
    let start = clock.now_ns();
    self.counters.turns = self.counters.turns.saturating_add(1);
    let mut context = Context {
      repository,
      store,
      clock,
      exchange,
      admission,
    };
    loop {
      match self.step(&mut context, out) {
        Ok(Progress::Done) => return Ok(Progress::Done),
        // A unit met a let-go window and stopped, nothing of it written: the turn ends, the window to be brought back.
        Ok(Progress::More) if self.is_waiting() => return Ok(Progress::More),
        // A unit done: a later stop, at any window, is waited for again.
        Ok(Progress::More) => self.last_stop = None,
        Err(refused) => {
          self.stage = Stage::Refused(refused);
          return Err(refused);
        }
      }
      if context.clock.now_ns().saturating_sub(start) >= quantum_ns {
        return Ok(Progress::More);
      }
    }
  }

  /// One unit of work: what is owed first, then a part's next entry, then the stage's next step.
  fn step(&mut self, cx: &mut Context<'_>, out: &mut impl Out) -> Result<Progress, StoreRefused> {
    if let Some(pending) = self.pending.take() {
      self.pending = self.owed(cx, pending, out)?;
      return Ok(Progress::More);
    }
    if let Some(by_entry) = self.by_entry.take() {
      self.by_entry = self.next_entry(cx, by_entry, out)?;
      return Ok(Progress::More);
    }
    let stage = core::mem::replace(&mut self.stage, Stage::Done);
    self.stage = self.next_stage(cx, stage, out)?;
    if matches!(self.stage, Stage::Done) {
      return Ok(Progress::Done);
    }
    Ok(Progress::More)
  }

  /// The stage after `stage`'s step.
  fn next_stage(
    &mut self,
    cx: &mut Context<'_>,
    stage: Stage,
    out: &mut impl Out,
  ) -> Result<Stage, StoreRefused> {
    match stage {
      Stage::Objects { next } => self.read_objects(cx, next),
      Stage::Gather { run, version, from } => self.gather(cx, (run, version, from)),
      Stage::Header => self.header(out),
      Stage::Runs {
        run,
        version,
        blobs,
      } => self.send_run(cx, (run, version, blobs)),
      Stage::CoverFrom => Ok(self.cover_from(cx)),
      Stage::Cover { greens, at, part } => self.cover(cx, greens, at, part),
      Stage::Holders { from, offered } => self.holders(cx, from, offered),
      Stage::Order { from } => self.order_unit(cx, from),
      Stage::Loose { from } => self.loose(cx, from, out),
      Stage::Trailer => self.trailer(out),
      Stage::Done => Ok(Stage::Done),
      Stage::Refused(refused) => Err(refused),
    }
  }

  /// Reads a window's worth of the answer's objects into the wanted set: its kept units from unit `next` on (its runs,
  /// listed positions or table words, whichever its form keeps), copied as they are kept, so a unit reads its window
  /// and never the set again (A-745 item 4), and the wanted set ends equal to the answer's, in its form. A unit
  /// `size_of::<u64>()` bytes at least, so a clone's few runs are read at once whatever it holds.
  fn read_objects(&mut self, cx: &Context<'_>, next: usize) -> Result<Stage, StoreRefused> {
    let handles = handles(cx.store);
    let (after, read) = self
      .wanted
      .copy_window(self.answer.objects(), next, handles);
    self.counters.objects_read_most = self.counters.objects_read_most.max(to_u64(read));
    match after {
      Some(next) => Ok(Stage::Objects { next }),
      None => self.gather_from(0),
    }
  }

  /// Gathering run `run` from its first version; once every run is gathered, the wanted set made whole and counted.
  fn gather_from(&mut self, run: usize) -> Result<Stage, StoreRefused> {
    if let Some(versions) = self.answer.runs().get(run) {
      return Ok(Stage::Gather {
        run,
        version: versions.after.saturating_add(1),
        from: 0,
      });
    }
    self.wanted = self.wanted.union(&core::mem::take(&mut self.gathered));
    self.counted = self.wanted.len();
    u32::try_from(self.counted).map_err(|_| StoreRefused::PackTooLarge)?;
    Ok(Stage::Header)
  }

  /// D(k) of `green`'s version `version`: the set kept while that version's objects are gathered, or else read from the
  /// version's value (A-647 part 3), once a version.
  fn brought_of(
    &mut self,
    cx: &Context<'_>,
    green: u64,
    version: u64,
  ) -> Result<Positions, StoreRefused> {
    match self.reading.take() {
      Some((at, brought)) if at == (green, version) => Ok(brought),
      _ => cx
        .repository
        .brought(cx.store, green, version_index(green, version)?),
    }
  }

  /// One unit of gathering (module docs, **What it sends** 1): a version's objects joined at once when the filter sends
  /// everything; otherwise one object, judged by its row.
  fn gather(
    &mut self,
    cx: &Context<'_>,
    (run, version, from): (usize, u64, u64),
  ) -> Result<Stage, StoreRefused> {
    let Some(versions) = self.answer.runs().get(run).copied() else {
      return self.gather_from(run);
    };
    if version > versions.through {
      return self.gather_from(run.saturating_add(1));
    }
    let green = versions.green;
    let brought = self.brought_of(cx, green, version)?;
    let next_version = Stage::Gather {
      run,
      version: version.saturating_add(1),
      from: 0,
    };
    if from == 0 {
      let head = run_head(cx.repository, versions)?;
      if brought.contains(head) {
        self.gathered.insert(head);
      }
    }
    if self.sends_all {
      self.gathered = self.gathered.union(&brought);
      return Ok(next_version);
    }
    let Some(position) = brought.next_from(from) else {
      return Ok(next_version);
    };
    let (_, row, _) = cx.repository.stored_at(cx.store, position)?;
    if self.answer.admits(row.kind, row.len) {
      self.gathered.insert(position);
    }
    self.reading = Some(((green, version), brought));
    Ok(Stage::Gather {
      run,
      version,
      from: position.saturating_add(1),
    })
  }

  /// The header, counting the wanted objects; the runs next.
  fn header(&mut self, out: &mut impl Out) -> Result<Stage, StoreRefused> {
    let count = u32::try_from(self.counted).map_err(|_| StoreRefused::PackTooLarge)?;
    let mut header = Vec::new();
    pack::put_header(count, &mut header);
    self.emit(&header, out);
    Ok(self.run_from(0))
  }

  /// Sending run `run` from its first version; the cover once every run is sent.
  fn run_from(&self, run: usize) -> Stage {
    match self.answer.runs().get(run) {
      Some(versions) => Stage::Runs {
        run,
        version: versions.after.saturating_add(1),
        blobs: false,
      },
      None => Stage::CoverFrom,
    }
  }

  /// One fragment of a run (module docs, **What it sends** 1), then the next.
  fn send_run(
    &mut self,
    cx: &Context<'_>,
    (run, version, blobs): (usize, u64, bool),
  ) -> Result<Stage, StoreRefused> {
    let Some(versions) = self.answer.runs().get(run).copied() else {
      return Ok(Stage::CoverFrom);
    };
    if version > versions.through {
      return Ok(self.run_from(run.saturating_add(1)));
    }
    let green = versions.green;
    let at = version_index(green, version)?;
    if let Some(kept) = cx.repository.fragment_part(green, at, blobs) {
      let source = Source::Fragment {
        green,
        version: at,
        blobs,
      };
      self.take_part(source, kept, true);
    }
    Ok(if blobs {
      Stage::Runs {
        run,
        version: version.saturating_add(1),
        blobs: false,
      }
    } else {
      Stage::Runs {
        run,
        version,
        blobs: true,
      }
    })
  }

  /// Sends what of a kept part is still wanted (module docs, **What the client can read**): copied whole, or entry by
  /// entry. `run` says the part is a run's.
  fn take_part(&mut self, source: Source, kept: &Kept, run: bool) {
    let keep = kept.positions.intersection(&self.wanted);
    if keep.is_empty() {
      return;
    }
    let whole = keep.len() == kept.positions.len();
    let readable = (self.answer.ofs_delta() || kept.offsets == 0) && self.bases_read(kept, run);
    self.wanted = self.wanted.difference(&keep);
    self.sent = self.sent.union(&keep);
    if whole && readable {
      self.counters.parts_copied = self.counters.parts_copied.saturating_add(1);
      self.entries = self.entries.saturating_add(to_u64(kept.placed.len()));
      self.pending = Some(Pending::Copy {
        content: kept.content,
        len: kept.len,
        from: 0,
        to: kept.len,
      });
      return;
    }
    self.counters.parts_by_entry = self.counters.parts_by_entry.saturating_add(1);
    let chosen = if whole {
      Chosen::Every
    } else {
      let words = kept.placed.len().div_ceil(word_bits());
      Chosen::Marked {
        bits: vec![0; words],
        marking: Some(0),
        cursor: 0,
      }
    };
    self.by_entry = Some(ByEntry {
      source,
      keep,
      run,
      chosen,
      next: 0,
      moved: Vec::new(),
    });
  }

  /// Whether the client reads every base the part's reference deltas name (module docs): each sent already; for a run's
  /// part, or in the pack still to come, or, with no filter to ask, held by a thin client.
  fn bases_read(&self, kept: &Kept, run: bool) -> bool {
    let missing = kept.references.difference(&self.sent);
    if missing.is_empty() {
      return true;
    }
    run && ((self.answer.thin() && self.sends_all) || missing.is_subset(&self.wanted))
  }

  /// One entry of a part read entry by entry: judged, and sent if wanted; the part's reading, or `None` once its last
  /// entry is judged.
  fn next_entry(
    &mut self,
    cx: &mut Context<'_>,
    mut by_entry: ByEntry,
    out: &mut impl Out,
  ) -> Result<Option<ByEntry>, StoreRefused> {
    let kept = part_of(cx.repository, by_entry.source)?;
    if let Chosen::Marked {
      bits,
      marking,
      cursor,
    } = &mut by_entry.chosen
      && let Some(from) = *marking
    {
      let mut read = 0;
      *marking = mark(
        kept,
        &by_entry.keep,
        bits,
        (from, cursor),
        (marking_reads(cx.store), &mut read),
      );
      self.counters.marks_read_most = self.counters.marks_read_most.max(to_u64(read));
      return Ok(Some(by_entry));
    }
    let Some(index) = by_entry.chosen.next_at(by_entry.next, kept.placed.len()) else {
      return Ok(None);
    };
    let Some(entry) = kept.placed.get(index).copied() else {
      return Ok(None);
    };
    by_entry.next = index.saturating_add(1);
    self.counters.entries_judged = self.counters.entries_judged.saturating_add(1);
    let end = kept
      .placed
      .get(index.saturating_add(1))
      .map_or(kept.len, |after| after.offset);
    if !self.send_entry(cx, kept, &mut by_entry, (index, entry, end), out)? {
      // r165: its object's read met a let-go window and the turn stopped; the entry is judged again next.
      by_entry.next = index;
      self.counters.entries_judged = self.counters.entries_judged.saturating_sub(1);
    }
    Ok(Some(by_entry))
  }

  /// Sends a kept entry that ends at `end` in its part (module docs, **What the client can read**): whether it did; not
  /// where an entry sent whole read its object at a let-go window and the turn stopped, nothing of it written.
  fn send_entry(
    &mut self,
    cx: &mut Context<'_>,
    kept: &Kept,
    by_entry: &mut ByEntry,
    (index, entry, end): (usize, Placed, u64),
    out: &mut impl Out,
  ) -> Result<bool, StoreRefused> {
    let (header, kept_header) = read_header(cx.store, kept, entry.offset, self.format)?;
    let Some(representation) = self.readable(cx, kept, by_entry, &entry, header.representation)?
    else {
      let found = cx.repository.stored_at(cx.store, entry.position)?;
      // Its place noted before it is sent, as for every entry, and given back where it stops.
      let at = self.at;
      let Some(pending) = self.send_whole(cx, found, out)? else {
        return Ok(false);
      };
      by_entry.moved.push((index, at));
      self.entries = self.entries.saturating_add(1);
      self.counters.made_whole = self.counters.made_whole.saturating_add(1);
      self.pending = Some(pending);
      return Ok(true);
    };
    by_entry.moved.push((index, self.at));
    self.entries = self.entries.saturating_add(1);
    let mut written = Vec::new();
    let header = Header {
      representation,
      size: header.size,
    };
    entry::write(header, self.at, &mut written).map_err(entry_refused)?;
    if written == kept_header {
      self.counters.entries_copied = self.counters.entries_copied.saturating_add(1);
    } else {
      self.counters.headers_rewritten = self.counters.headers_rewritten.saturating_add(1);
    }
    self.emit(&written, out);
    self.pending = Some(Pending::Copy {
      content: kept.content,
      len: kept.len,
      from: entry.offset.saturating_add(to_u64(kept_header.len())),
      to: end,
    });
    Ok(true)
  }

  /// How the client reads a kept entry, kept as `kept_as`; `None` when it must be sent whole.
  fn readable(
    &self,
    cx: &Context<'_>,
    kept: &Kept,
    by_entry: &ByEntry,
    entry: &Placed,
    kept_as: Representation,
  ) -> Result<Option<Representation>, StoreRefused> {
    match kept_as {
      Representation::Whole(kind) => Ok(Some(Representation::Whole(kind))),
      Representation::OfsDelta { .. } => Ok(self.offset_base(kept, by_entry, entry)),
      Representation::RefDelta { base } => {
        let Some(position) = entry.base else {
          return Ok(None);
        };
        let reads = self.sent.contains(position)
          || (by_entry.run && (self.wanted.contains(position) || self.held(cx, position)?));
        Ok(reads.then_some(Representation::RefDelta { base }))
      }
    }
  }

  /// An offset delta's base as this pack holds it: at its new place, by offset for a client that reads them and by id
  /// otherwise; by id where it was sent before this part; `None` where the pack does not carry it.
  fn offset_base(&self, kept: &Kept, by_entry: &ByEntry, entry: &Placed) -> Option<Representation> {
    let offset = entry.base?;
    let index = kept
      .placed
      .binary_search_by_key(&offset, |placed| placed.offset)
      .ok()?;
    let base = kept.placed.get(index)?;
    let moved = by_entry
      .moved
      .binary_search_by_key(&index, |(sent, _)| *sent)
      .ok()
      .and_then(|at| by_entry.moved.get(at))
      .map(|(_, at)| *at);
    match moved {
      Some(moved) if self.answer.ofs_delta() => Some(Representation::OfsDelta { base: moved }),
      Some(_) => Some(Representation::RefDelta { base: base.id }),
      None if self.sent.contains(base.position) && !by_entry.keep.contains(base.position) => {
        Some(Representation::RefDelta { base: base.id })
      }
      None => None,
    }
  }

  /// Whether a thin client holds the object at `position` a run's delta names and the pack does not carry (module docs):
  /// it lies in what the client's version reaches, so it holds it where the filter would have sent it.
  fn held(&self, cx: &Context<'_>, position: u64) -> Result<bool, StoreRefused> {
    if !self.answer.thin() {
      return Ok(false);
    }
    if self.sends_all {
      return Ok(true);
    }
    let (_, row, _) = cx.repository.stored_at(cx.store, position)?;
    Ok(self.answer.admits(row.kind, row.len))
  }

  /// An object sent whole: its header now, and what deflates its bytes a window a unit, at the level learned for its
  /// kind or chosen by sampling its first window.
  fn send_whole(
    &mut self,
    cx: &mut Context<'_>,
    (id, row, stored): (ObjectId, ObjectRow, StoredAt),
    out: &mut impl Out,
  ) -> Result<Option<Pending>, StoreRefused> {
    let mut written = Vec::new();
    let header = Header {
      representation: Representation::Whole(row.kind),
      size: row.len,
    };
    entry::write(header, self.at, &mut written).map_err(entry_refused)?;
    let (first, from) = match stored {
      StoredAt::Whole(content) => (
        cx.store
          .window_at(&content, row.len, 0)
          .ok_or(StoreRefused::Unreadable { id })?,
        Deflated::Content(content),
      ),
      StoredAt::InPack(_) => {
        // Nothing of it written yet: a stop takes it again whole.
        let Some(bytes) = self.read_or_stop(cx, &stored, (id, row.len))? else {
          return Ok(None);
        };
        let bytes = bytes.into_owned();
        let first = bytes.get(..cx.store.window()).unwrap_or(&bytes).to_vec();
        (Cow::Owned(first), Deflated::Read(bytes))
      }
    };
    let mut learning = Learning {
      window: &mut self.window,
      levels: &mut self.levels,
      depth: self.depth,
      exchange: cx.exchange,
    };
    let level = fragment::level(row.kind, &first, &mut learning).map_err(StoreRefused::Encoding)?;
    self.emit(&written, out);
    Ok(Some(Pending::Deflate(Deflating {
      from,
      len: row.len,
      next: 0,
      compress: super::deflaters::deflater(level),
    })))
  }

  /// One unit of what is owed: a window of kept bytes copied, a window of an object or a delta deflated, or a unit of
  /// a search; what is owed after it.
  fn owed(
    &mut self,
    cx: &mut Context<'_>,
    pending: Pending,
    out: &mut impl Out,
  ) -> Result<Option<Pending>, StoreRefused> {
    match pending {
      Pending::Copy {
        content,
        len,
        from,
        to,
      } => self.copy_window(cx.store, (content, len), (from, to), out),
      Pending::Deflate(deflating) => self.deflate_window(cx, deflating, out),
      Pending::Search(searching) => self.search_unit(cx, searching, out),
    }
  }

  /// The bytes from `from` to `to` of a kept content, as far as the window `from` lies in holds them.
  fn copy_window(
    &mut self,
    store: &ContentStore,
    (content, len): (ContentId, u64),
    (from, to): (u64, u64),
    out: &mut impl Out,
  ) -> Result<Option<Pending>, StoreRefused> {
    if from >= to {
      return Ok(None);
    }
    let window = to_u64(store.window());
    let index = from.checked_div(window).unwrap_or(0);
    let start = index.saturating_mul(window);
    let Some(bytes) = store.window_at(&content, len, index) else {
      // A-864 piece 2: a window let go stops the copy where it is, nothing written, the first time it is met.
      let waited = packed::Waited {
        pack: content,
        len,
        offset: from,
      };
      if store.away_window(&content, len, from).is_some() && self.stop_at(store, waited) {
        return Ok(Some(Pending::Copy {
          content,
          len,
          from,
          to,
        }));
      }
      return Err(StoreRefused::FragmentUnreadable);
    };
    let upto = to.min(start.saturating_add(to_u64(bytes.len())));
    let piece = within(
      &bytes,
      from.saturating_sub(start),
      upto.saturating_sub(start),
    )?;
    if upto <= from {
      return Err(StoreRefused::FragmentUnreadable);
    }
    self.emit(piece, out);
    if upto >= to {
      return Ok(None);
    }
    Ok(Some(Pending::Copy {
      content,
      len,
      from: upto,
      to,
    }))
  }

  /// The next window of an object or a delta being deflated, fed and what it gave sent; the stream ended after its
  /// last, and a delta's hold given back.
  fn deflate_window(
    &mut self,
    cx: &mut Context<'_>,
    mut deflating: Deflating,
    out: &mut impl Out,
  ) -> Result<Option<Pending>, StoreRefused> {
    let window = to_u64(cx.store.window());
    let windows = deflating.len.div_ceil(window).max(1);
    let bytes = match &deflating.from {
      Deflated::Content(content) => cx
        .store
        .window_at(content, deflating.len, deflating.next)
        .ok_or(StoreRefused::FragmentUnreadable)?,
      Deflated::Made { delta, .. } | Deflated::Read(delta) => {
        Cow::Borrowed(made_window(delta, window, deflating.next)?)
      }
    };
    let last = deflating.next.saturating_add(1) >= windows;
    let given = deflating
      .compress
      .state()
      .and_then(|state| fragment::deflate_more(state, &bytes, last))
      .map_err(StoreRefused::Encoding)?;
    self.emit(&given, out);
    if last {
      if let Deflated::Made { hold, .. } = deflating.from {
        cx.admission.release(hold);
      }
      return Ok(None);
    }
    deflating.next = deflating.next.saturating_add(1);
    Ok(Some(Pending::Deflate(deflating)))
  }

  /// The greens whose kept parts may cover what is left: those a head of whose versions is still wanted, the largest
  /// base pack first (module docs, **What it sends** 2).
  fn cover_from(&self, cx: &Context<'_>) -> Stage {
    let mut wanted: Vec<u64> = cx
      .repository
      .heads()
      .filter(|(_, head)| self.wanted.contains(*head))
      .map(|(green, _)| green)
      .collect();
    wanted.dedup();
    let mut greens: Vec<(u64, u64)> = wanted
      .into_iter()
      .map(|green| (green, base_len(cx.repository, green)))
      .collect();
    greens.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    Stage::Cover {
      greens: greens.into_iter().map(|(green, _)| green).collect(),
      at: 0,
      part: Part::Base { blobs: false },
    }
  }

  /// Offers green `greens[at]`'s `part` to the cover; the next part, the next green's first, or what no part covers.
  fn cover(
    &mut self,
    cx: &Context<'_>,
    greens: Vec<u64>,
    at: usize,
    part: Part,
  ) -> Result<Stage, StoreRefused> {
    let Some(green) = greens.get(at).copied() else {
      return Ok(Stage::Holders {
        from: 0,
        offered: BTreeSet::new(),
      });
    };
    if let Some((source, kept)) = offered(cx.repository, green, part) {
      self.take_part(source, kept, false);
    }
    Ok(match after(cx.repository, green, part) {
      Some(part) => Stage::Cover { greens, at, part },
      None => Stage::Cover {
        greens,
        at: at.saturating_add(1),
        part: Part::Base { blobs: false },
      },
    })
  }

  /// One object still wanted (module docs, **What it sends** 2): the parts of the version its row says brought it
  /// offered, once each.
  fn holders(
    &mut self,
    cx: &Context<'_>,
    from: u64,
    mut offered: BTreeSet<(u64, u64, bool)>,
  ) -> Result<Stage, StoreRefused> {
    let Some(position) = self.wanted.next_from(from) else {
      return Ok(Stage::Order { from: 0 });
    };
    let (_, row, _) = cx.repository.stored_at(cx.store, position)?;
    let blobs = row.kind == Kind::Blob;
    if let Some((green, version)) = cx.repository.bringer_of(position)
      && offered.insert((green, version, blobs))
      && let Ok(version) = usize::try_from(version)
      && let Some(kept) = cx.repository.fragment_part(green, version, blobs)
    {
      let source = Source::Fragment {
        green,
        version,
        blobs,
      };
      self.take_part(source, kept, false);
    }
    Ok(Stage::Holders {
      from: position.saturating_add(1),
      offered,
    })
  }

  /// Whether `refused`, a read's refusal, met a let-go window the stream stops at ([`Upload::stop_at`]).
  fn stopped_by(&mut self, store: &ContentStore, refused: &packed::PackedRefused) -> bool {
    refused
      .waited()
      .is_some_and(|waited| self.stop_at(store, waited))
  }

  /// Stops the turn at `waited`'s let-go window, named for its driver to bring back ([`Upload::waiting`]); whether it
  /// stopped. The same window met again with no unit done since it was to be brought back is not stopped at: the read
  /// refuses typed, as one of a window that cannot be read does, so a stop always makes progress or ends.
  fn stop_at(&mut self, store: &ContentStore, waited: packed::Waited) -> bool {
    let window = to_u64(store.window()).max(1);
    let at = (waited.pack, waited.offset.checked_div(window).unwrap_or(0));
    if self.last_stop == Some(at) {
      return false;
    }
    self.last_stop = Some(at);
    self.stopped_any = true;
    self.counters.stops = self.counters.stops.saturating_add(1);
    self.waiting.set(Some(waited));
    true
  }

  /// A search's next read of the object `(id, len, stored)` into `bytes` ([`read_window`]): one lying in its pack read
  /// whole at once, or, where that read met a let-go window, the turn stopped and nothing read, so the search reads it
  /// again next turn (r165); whether a read is still due.
  fn read_window(
    &mut self,
    cx: &Context<'_>,
    (id, len, stored): (ObjectId, u64, StoredAt),
    bytes: &mut Vec<u8>,
  ) -> Result<bool, StoreRefused> {
    if matches!(stored, StoredAt::InPack(_)) && to_u64(bytes.len()) < len {
      if let Some(read) = self.read_or_stop(cx, &stored, (id, len))? {
        *bytes = read.into_owned();
      }
      return Ok(true);
    }
    read_window(cx, (id, len, stored), bytes)
  }

  /// `stored`'s `len` bytes, the object `id`, read where it lies; none where the read met a let-go window and the turn
  /// stopped at it ([`Upload::stop_at`]), so the unit reading it is taken again once it is brought back (main 01a20182's
  /// r165: git/git's mirror clone after the push's let-go refused `Unreadable` 16,270,345 bytes in, an object read to be
  /// encoded afresh where only a reused entry's read stopped). Refused where the read met no let-go window, or the same
  /// one again with nothing done since.
  fn read_or_stop<'s>(
    &mut self,
    cx: &Context<'s>,
    stored: &StoredAt,
    (id, len): (ObjectId, u64),
  ) -> Result<Option<Cow<'s, [u8]>>, StoreRefused> {
    // A note an earlier read left is not this read's: the window stopped at is the one this read met.
    let _ = cx.repository.waiting();
    if let Some(bytes) = cx.repository.read_stored(cx.store, stored, len) {
      return Ok(Some(bytes));
    }
    match cx.repository.waiting() {
      Some(waited) if self.stop_at(cx.store, waited) => Ok(None),
      _ => Err(StoreRefused::Unreadable { id }),
    }
  }

  /// Whether a unit stopped at a let-go window ([`Upload::waiting`]) not yet taken.
  fn is_waiting(&self) -> bool {
    let waiting = self.waiting.take();
    self.waiting.set(waiting);
    waiting.is_some()
  }

  /// The let-go window the stream's last turn stopped at, its unit kept to be taken again once the window is brought
  /// back (goal 93 item 2a; A-864 piece 2): taken, so the next turn reads on. None where the turn met none.
  pub fn waiting(&self) -> Option<packed::Waited> {
    self.waiting.take()
  }

  /// Where the stream's next turn begins to read a pushed pack (A-864 piece 2, a turn's windows brought back ahead of
  /// it): the pack, its length and the offset, from the copy under way or the loose stage's next object in its order;
  /// the stage reads each pack front to back from there. None where it reads no pack next.
  pub fn reading_next(&self) -> Option<packed::Waited> {
    // Only once the stream has met a let-go window: a pack held whole has nothing to bring back.
    if !self.stopped_any {
      return None;
    }
    if let Some(crc) = &self.pending_crc {
      return Some(packed::Waited {
        pack: crc.place.pack,
        len: crc.place.len,
        offset: crc.place.offset.saturating_add(crc.checked),
      });
    }
    if let Some(Pending::Copy {
      content, len, from, ..
    }) = &self.pending
    {
      return Some(packed::Waited {
        pack: *content,
        len: *len,
        offset: *from,
      });
    }
    let Stage::Loose { from } = self.stage else {
      return None;
    };
    let (pack, offset, _) = *self.order.get(usize::try_from(from).ok()?)?;
    let (pack, len) = *self.pack_lens.get(usize::try_from(pack).ok()?)?;
    Some(packed::Waited { pack, len, offset })
  }

  /// One unit of the loose stage's order (A-864's piece 1; main 01a20182's ruling of 2026-11-22): where the next object
  /// no part covered lies read from its row, the object noted by its pack and offset there, or last by its position for
  /// one held whole; with none left, the order sorted, so the loose stage reads each pack's entries in the pack's own
  /// order, front to back (as git writes the entries it reuses), its bases before their offset deltas and each window
  /// wanted once. Each object noted is charged three words to the request's admission (a pack's ordinal, an offset and
  /// a position: 24 bytes an object, 3.6 MB for git/git's 150,000).
  fn order_unit(&mut self, cx: &mut Context<'_>, from: u64) -> Result<Stage, StoreRefused> {
    let Some(position) = self.wanted.next_from(from) else {
      self.order.sort_unstable();
      return Ok(Stage::Loose { from: 0 });
    };
    let (_, _, stored) = cx.repository.stored_at(cx.store, position)?;
    cx.admission
      .charge_items::<(u64, u64, u64)>(1)
      .map_err(|_| StoreRefused::PackTooLarge)?;
    let key = match stored {
      StoredAt::InPack(place) => {
        let packs = to_u64(self.packs.len());
        let pack = *self.packs.entry(place.pack).or_insert_with(|| {
          self.pack_lens.push((place.pack, place.len));
          packs
        });
        (pack, place.offset, position)
      }
      StoredAt::Whole(_) => (u64::MAX, position, position),
    };
    self.order.push(key);
    Ok(Stage::Order {
      from: position.saturating_add(1),
    })
  }

  /// The loose stage after the object at `position`, taken at `from` with `next` after it, stopped at a let-go window
  /// (A-864 piece 2; r165): nothing of it written or kept, it is taken again next, its charge kept for it.
  fn retaken(&mut self, position: u64, (from, next): (u64, Stage)) -> Stage {
    self.loose_at.remove(&position);
    self.prepaid.insert(position);
    if matches!(next, Stage::Loose { from: after } if after == from) {
      self.ahead.push(position);
    }
    Stage::Loose { from }
  }

  /// The object [`Self::loose`] takes this turn, and the stage after it: the one a delta's base put ahead, else the
  /// order's object at `from`. `Err` with the stage to go on to when the turn takes none: the order is done (the
  /// trailer), or its object was placed already, ahead of its turn as a delta's base.
  fn loose_next(&mut self, from: u64) -> Result<(u64, Stage), Stage> {
    if let Some(position) = self.ahead.pop() {
      return Ok((position, Stage::Loose { from }));
    }
    let Some(&(_, _, position)) = usize::try_from(from).ok().and_then(|at| self.order.get(at))
    else {
      return Err(Stage::Trailer);
    };
    let next = Stage::Loose {
      from: from.saturating_add(1),
    };
    if self.loose_at.contains_key(&position) {
      return Err(next);
    }
    Ok((position, next))
  }

  /// One object no part covered (module docs, **What it sends** 3): encoded with the fragments' encoder when it is a
  /// window long at most; past a window, searched against the earlier ones past a window when the request's admission
  /// holds the search, and sent whole otherwise (`loose_large`).
  fn loose(
    &mut self,
    cx: &mut Context<'_>,
    from: u64,
    out: &mut impl Out,
  ) -> Result<Stage, StoreRefused> {
    let (position, next) = match self.loose_next(from) {
      Ok(taken) => taken,
      Err(stage) => return Ok(stage),
    };
    let (id, row, stored) = cx.repository.stored_at(cx.store, position)?;
    self.charge_placement_once(cx, position)?;
    self.loose_at.insert(position, self.at);
    match self.reused(cx, (id, &row, stored, position), out)? {
      Reuse::Sent(pending) => {
        self.entries = self.entries.saturating_add(1);
        self.pending = Some(pending);
        return Ok(next);
      }
      Reuse::BaseFirst(base) => {
        // Placed after its base, which goes first (its charge kept for when it is placed).
        self.loose_at.remove(&position);
        self.prepaid.insert(position);
        self.ahead.push(position);
        self.ahead.push(base);
        return Ok(Stage::Loose { from });
      }
      // A-864 piece 2: its entry's read met a let-go window.
      Reuse::Stopped => return Ok(self.retaken(position, (from, next))),
      Reuse::Encode => {}
    }
    if row.len > to_u64(cx.store.window()) {
      let Some(pending) = self.loose_large(cx, (id, row, stored), out)? else {
        return Ok(self.retaken(position, (from, next)));
      };
      self.entries = self.entries.saturating_add(1);
      self.pending = Some(pending);
      return Ok(next);
    }
    // r165: an object encoded afresh is read where it lies; a let-go window stops the turn, as a reused entry's does.
    let Some(bytes) = self.read_or_stop(cx, &stored, (id, row.len))? else {
      return Ok(self.retaken(position, (from, next)));
    };
    self.entries = self.entries.saturating_add(1);
    let bytes = bytes.into_owned();
    let object = ToEncode {
      id,
      position,
      kind: row.kind,
      bytes,
      named: None,
    };
    self.encode_loose(cx, object, out)?;
    Ok(next)
  }

  /// Charges the placement of the object at `position` to the request's admission once (codex-review, 2026-11-22: a
  /// retaken object charged again made the count of a released pack's stops decide whether a request fit): one charged
  /// before a stop or its base's turn keeps its charge.
  fn charge_placement_once(
    &mut self,
    cx: &mut Context<'_>,
    position: u64,
  ) -> Result<(), StoreRefused> {
    if !self.prepaid.remove(&position) {
      cx.admission
        .charge_items::<(u64, u64)>(1)
        .map_err(|_| StoreRefused::PackTooLarge)?;
    }
    Ok(())
  }

  /// `object`, read where it lies, encoded with the fragments' encoder and emitted, counted as sent whole or as a delta.
  fn encode_loose(
    &mut self,
    cx: &Context<'_>,
    object: ToEncode,
    out: &mut impl Out,
  ) -> Result<(), StoreRefused> {
    let mut learning = Learning {
      window: &mut self.window,
      levels: &mut self.levels,
      depth: self.depth,
      exchange: cx.exchange,
    };
    let (entry, placed) = self
      .encoder
      .put(object, self.at, &mut learning)
      .map_err(StoreRefused::Encoding)?;
    if placed.stored == Stored::Whole {
      self.counters.loose_whole = self.counters.loose_whole.saturating_add(1);
    } else {
      self.counters.loose_deltas = self.counters.loose_deltas.saturating_add(1);
    }
    self.emit(&entry, out);
    Ok(())
  }

  /// `id`'s pushed entry sent on as it lies (A-859, git's pack reuse), once its check holds (a whole entry's data
  /// inflates to the object its name says; a delta's, applied to its base, to the object, which its name says) and its
  /// base precedes it in this pack: sent already, or placed by this loose stage before it, its base placed first where it
  /// is still wanted (git's `write_one`). The delta goes as an offset delta to a base the loose stage placed where the
  /// client said `ofs-delta`, otherwise by its base's name. Its header written, what copies its data a window a unit;
  /// its base's position to place first; or none, to encode it instead.
  fn reused(
    &mut self,
    cx: &mut Context<'_>,
    (id, row, stored, position): (ObjectId, &ObjectRow, StoredAt, u64),
    out: &mut impl Out,
  ) -> Result<Reuse, StoreRefused> {
    let StoredAt::InPack(place) = stored else {
      self.counters.reuse_no_entry = self.counters.reuse_no_entry.saturating_add(1);
      return Ok(Reuse::Encode);
    };
    #[cfg(test)]
    if REUSE_OFF.with(core::cell::Cell::get) {
      return Ok(Reuse::Encode);
    }
    if self
      .last_read
      .is_some_and(|(pack, offset)| pack == place.pack && place.offset < offset)
    {
      self.counters.backward_reads = self.counters.backward_reads.saturating_add(1);
    }
    self.last_read = Some((place.pack, place.offset));
    #[cfg(test)]
    let row_pack = row.pack.filter(|_| !KEPT_OFF.with(core::cell::Cell::get));
    #[cfg(not(test))]
    let row_pack = row.pack;
    if let Some(packed) = row_pack
      && let Some(reuse) = self.reused_as_kept(cx, (row, place, packed), out)?
    {
      return Ok(reuse);
    }
    // A-864: rebuilt to be checked only where the kept CRC32 missed.
    self.counters.reuse_rebuilt = self.counters.reuse_rebuilt.saturating_add(1);
    let mut scratch = Vec::new();
    let room = cx.repository.largest_object().max(row.len);
    let raw = match packed::raw_entry(cx.store, (place, self.format), (&mut scratch, room)) {
      Ok(raw) => raw,
      // r165: a let-go window stops the turn rather than sending the object encoded afresh.
      Err(refused) if self.stopped_by(cx.store, &refused) => return Ok(Reuse::Stopped),
      Err(_) => {
        self.counters.reuse_unread = self.counters.reuse_unread.saturating_add(1);
        return Ok(Reuse::Encode);
      }
    };
    let representation = match raw.representation {
      Representation::Whole(kind) => {
        let checked = kind == row.kind
          && crate::object_name::name_of(self.format, kind, &scratch)
            .is_ok_and(|named| named == id);
        if !checked {
          self.counters.reuse_refused = self.counters.reuse_refused.saturating_add(1);
          return Ok(Reuse::Encode);
        }
        Representation::Whole(kind)
      }
      delta => {
        // Its base's read's note this read's own: a let-go window stops the turn (r165).
        let _ = cx.repository.waiting();
        let Some((base, base_bytes)) = self.base_of(cx, (place, delta)) else {
          if let Some(waited) = cx.repository.waiting()
            && self.stop_at(cx.store, waited)
          {
            return Ok(Reuse::Stopped);
          }
          self.counters.reuse_unread = self.counters.reuse_unread.saturating_add(1);
          return Ok(Reuse::Encode);
        };
        match self.delta_reused(cx, (id, row, stored), (base, &base_bytes, &scratch))? {
          DeltaReuse::As(representation) => representation,
          DeltaReuse::BaseFirst(base) => return Ok(Reuse::BaseFirst(base)),
          DeltaReuse::No => return Ok(Reuse::Encode),
          DeltaReuse::Stopped => return Ok(Reuse::Stopped),
        }
      }
    };
    let mut header = Vec::new();
    entry::write(
      Header {
        representation,
        size: raw.size,
      },
      self.at,
      &mut header,
    )
    .map_err(entry_refused)?;
    if matches!(representation, Representation::Whole(_)) {
      self.counters.reused_whole = self.counters.reused_whole.saturating_add(1);
      // Kept as a later object's candidate base, as an object this stage deflates whole is.
      if row.len > to_u64(cx.store.window()) {
        let large = Large {
          id,
          kind: row.kind,
          len: row.len,
          stored,
          at: self.at,
          depth: 0,
        };
        self.remember_large(large, cx.exchange);
      } else {
        let learning = Learning {
          window: &mut self.window,
          levels: &mut self.levels,
          depth: self.depth,
          exchange: cx.exchange,
        };
        let object = ToEncode {
          id,
          position,
          kind: row.kind,
          bytes: scratch,
          named: None,
        };
        self.encoder.remember(object, (self.at, 0), &learning);
      }
    } else {
      self.counters.reused_deltas = self.counters.reused_deltas.saturating_add(1);
    }
    self.emit(&header, out);
    Ok(Reuse::Sent(Pending::Copy {
      content: place.pack,
      len: place.len,
      from: raw.data,
      to: raw.end,
    }))
  }

  /// `row`'s pushed entry sent on as it lies by what its row keeps of it (A-864, `rows::PackedRow`; git's
  /// `write_reuse_object`): its header read, the CRC32 of its raw bytes checked against the kept one, a delta's base by
  /// the position its row names, placed first where it is still wanted (git's `write_one`); then its data copied, no
  /// object read. None where its CRC32 misses (counted `reuse_crc_missed`) or its header cannot be read: the caller
  /// rebuilds the object to check it, as before.
  fn reused_as_kept(
    &mut self,
    cx: &mut Context<'_>,
    (row, place, packed): (&ObjectRow, InPack, rows::PackedRow),
    out: &mut impl Out,
  ) -> Result<Option<Reuse>, StoreRefused> {
    let (header, data, mut hasher, mut checked) = match self.pending_crc.take() {
      Some(pending) if pending.place == place => (
        pending.header,
        pending.data,
        pending.hasher,
        pending.checked,
      ),
      _ => {
        let room = cx.repository.largest_object().max(row.len);
        let (header, data) = match packed::entry_header(cx.store, (place, self.format), room) {
          Ok(found) => found,
          Err(refused) => {
            return Ok(
              self
                .stopped_by(cx.store, &refused)
                .then_some(Reuse::Stopped),
            );
          }
        };
        (header, data, crc32fast::Hasher::new(), 0_u64)
      }
    };
    let crc = match packed::raw_crc_resume(cx.store, place, packed.len, &mut hasher, &mut checked) {
      Ok(crc) => Some(crc),
      Err(refused) if self.stopped_by(cx.store, &refused) => {
        self.pending_crc = Some(PendingCrc {
          place,
          header,
          data,
          hasher,
          checked,
        });
        return Ok(Some(Reuse::Stopped));
      }
      Err(_) => None,
    };
    if crc != Some(packed.crc) {
      self.counters.reuse_crc_missed = self.counters.reuse_crc_missed.saturating_add(1);
      return Ok(None);
    }
    let representation = match (header.representation, packed.base) {
      (Representation::Whole(kind), None) if kind == row.kind => Representation::Whole(kind),
      (Representation::OfsDelta { .. } | Representation::RefDelta { .. }, Some(base)) => {
        let placed = self.sent.contains(base) || self.loose_at.contains_key(&base);
        if !placed {
          if self.wanted.contains(base) && to_u64(self.ahead.len()) < cx.repository.learned_depth()
          {
            return Ok(Some(Reuse::BaseFirst(base)));
          }
          return Ok(Some(Reuse::Encode));
        }
        match self.loose_at.get(&base) {
          Some(base_at) if self.answer.ofs_delta() => Representation::OfsDelta { base: *base_at },
          _ => Representation::RefDelta {
            base: cx
              .repository
              .id_at(cx.store, base)
              .ok_or(StoreRefused::NoPosition { position: base })?,
          },
        }
      }
      _ => return Ok(None),
    };
    let mut written = Vec::new();
    entry::write(
      Header {
        representation,
        size: header.size,
      },
      self.at,
      &mut written,
    )
    .map_err(entry_refused)?;
    if matches!(representation, Representation::Whole(_)) {
      self.counters.reused_whole = self.counters.reused_whole.saturating_add(1);
    } else {
      self.counters.reused_deltas = self.counters.reused_deltas.saturating_add(1);
    }
    self.emit(&written, out);
    Ok(Some(Reuse::Sent(Pending::Copy {
      content: place.pack,
      len: place.len,
      from: data,
      to: place.offset.saturating_add(packed.len),
    })))
  }

  /// A pushed delta's base (A-859): an OFS_DELTA's the entry at its offset of the same pack, read and named; a
  /// REF_DELTA's the object it names. Its name and its bytes; none where it is not read.
  fn base_of(
    &self,
    cx: &Context<'_>,
    (place, delta): (InPack, Representation),
  ) -> Option<(ObjectId, Vec<u8>)> {
    match delta {
      Representation::OfsDelta { base } => {
        let (kind, bytes) = cx.repository.read_entry(
          cx.store,
          InPack {
            offset: base,
            ..place
          },
        )?;
        let base = crate::object_name::name_of(self.format, kind, &bytes).ok()?;
        Some((base, bytes))
      }
      Representation::RefDelta { base } => {
        let (row, content) = cx.repository.indexed(cx.store, &base)?;
        let bytes = cx
          .repository
          .row_bytes(cx.store, &row, content)?
          .into_owned();
        Some((base, bytes))
      }
      Representation::Whole(_) => None,
    }
  }

  /// How a pushed delta of `id` against `base` goes on (A-859), once its base precedes it in this pack (sent already, or
  /// by this loose stage before it) and its data (`delta`, inflated) applied to the base's bytes gives the object, which
  /// its name says: an offset delta to a base this stage placed where the client said `ofs-delta`, by its base's name
  /// otherwise. Its base's position to place first where the base is still wanted and not yet placed, within the
  /// deepest chain any push carried ([`Upload`]'s `ahead`); none where its base does not precede it otherwise;
  /// refused-and-counted where its check fails.
  fn delta_reused(
    &mut self,
    cx: &Context<'_>,
    (id, row, stored): (ObjectId, &ObjectRow, StoredAt),
    (base, base_bytes, delta): (ObjectId, &[u8], &[u8]),
  ) -> Result<DeltaReuse, StoreRefused> {
    let Some((base_row, _)) = cx.repository.indexed(cx.store, &base) else {
      return Ok(DeltaReuse::No);
    };
    let before =
      self.sent.contains(base_row.position) || self.loose_at.contains_key(&base_row.position);
    if !before {
      let first = self.wanted.contains(base_row.position)
        && to_u64(self.ahead.len()) < cx.repository.learned_depth();
      return Ok(if first {
        DeltaReuse::BaseFirst(base_row.position)
      } else {
        DeltaReuse::No
      });
    }
    let Some(object) = self.read_or_stop(cx, &stored, (id, row.len))? else {
      return Ok(DeltaReuse::Stopped);
    };
    let mut built = Vec::new();
    let checked = crate::object_name::name_of(self.format, row.kind, &object)
      .is_ok_and(|named| named == id)
      && crate::delta::apply_to(base_bytes, delta, &mut built).is_ok()
      && built.as_slice() == object.as_ref();
    if !checked {
      self.counters.reuse_refused = self.counters.reuse_refused.saturating_add(1);
      return Ok(DeltaReuse::No);
    }
    Ok(DeltaReuse::As(
      match self.loose_at.get(&base_row.position) {
        Some(base_at) if self.answer.ofs_delta() => Representation::OfsDelta { base: *base_at },
        _ => Representation::RefDelta { base },
      },
    ))
  }

  /// An object past a window (module docs, **What it sends** 3): searched when it has candidates and the request's
  /// admission can hold the search, sent whole otherwise, and counted when the admission was what refused. What is
  /// owed next: the search, or the object's deflating.
  fn loose_large(
    &mut self,
    cx: &mut Context<'_>,
    target: (ObjectId, ObjectRow, StoredAt),
    out: &mut impl Out,
  ) -> Result<Option<Pending>, StoreRefused> {
    let candidates = self.candidates(target.1.kind, cx.exchange);
    if !candidates.is_empty() {
      match cx.admission.hold(search_charge(target.1.len, &candidates)) {
        Ok(hold) => {
          return Ok(Some(Pending::Search(Box::new(searching(
            target, candidates, hold,
          )))));
        }
        // The admission cannot hold the search: the object goes whole, and the fall-back is counted.
        Err(PastAdmission { .. }) => {
          self.counters.loose_unsearched = self.counters.loose_unsearched.saturating_add(1);
        }
      }
    }
    self.large_whole(cx, target, out)
  }

  /// An object past a window sent whole and kept as a later one's candidate: what deflates it.
  fn large_whole(
    &mut self,
    cx: &mut Context<'_>,
    target: (ObjectId, ObjectRow, StoredAt),
    out: &mut impl Out,
  ) -> Result<Option<Pending>, StoreRefused> {
    let large = Large {
      id: target.0,
      kind: target.1.kind,
      len: target.1.len,
      stored: target.2,
      at: self.at,
      depth: 0,
    };
    let Some(pending) = self.send_whole(cx, target, out)? else {
      return Ok(None);
    };
    self.counters.loose_whole = self.counters.loose_whole.saturating_add(1);
    self.remember_large(large, cx.exchange);
    Ok(Some(pending))
  }

  /// The objects past a window of `kind` a search tries (module docs): none where the learned depth makes no delta,
  /// otherwise the nearest first, as many as the learned window tries.
  fn candidates(&self, kind: Kind, exchange: Exchange) -> Vec<Large> {
    let Some(recent) = self.large.get(&kind).filter(|_| self.depth > 0) else {
      return Vec::new();
    };
    let window = self.window.size(exchange, recent.len());
    recent.iter().rev().take(window).copied().collect()
  }

  /// Keeps `large` as a later one's candidate while its chain has room, and lets go of those of its kind past what the
  /// window tries now, as the fragments' encoder keeps its own.
  fn remember_large(&mut self, large: Large, exchange: Exchange) {
    if large.depth >= self.depth {
      return;
    }
    let keep = self.window.size(exchange, usize::MAX);
    let recent = self.large.entry(large.kind).or_default();
    recent.push_back(large);
    while recent.len() > keep {
      recent.pop_front();
    }
  }

  /// One unit of a search (module docs, **Slices**): a window of the object's bytes read, or of the candidate's, or a
  /// step of the search over the two, a window's bytes at most; after the last candidate's trial, the entry.
  fn search_unit(
    &mut self,
    cx: &mut Context<'_>,
    mut searching: Box<Searching>,
    out: &mut impl Out,
  ) -> Result<Option<Pending>, StoreRefused> {
    let (id, row, stored) = &searching.target;
    let target = (*id, row.len, *stored);
    if self.read_window(cx, target, &mut searching.target_bytes)? {
      return Ok(Some(Pending::Search(searching)));
    }
    let Some(candidate) = searching.candidates.get(searching.trying).copied() else {
      return self.searched(cx, *searching, out).map(Some);
    };
    let base = (candidate.id, candidate.len, candidate.stored);
    let step = if self.read_window(cx, base, &mut searching.base_bytes)? {
      SearchStep::More
    } else {
      let Searching {
        search,
        base_bytes,
        target_bytes,
        ..
      } = &mut *searching;
      search.step(base_bytes, target_bytes, cx.store.window())
    };
    if let SearchStep::Done(delta) = step {
      // The trial's time is the machine's measured rate over the bytes it searched (`Rates`), never the clock's.
      searching.trial_ns = cx
        .exchange
        .rates
        .search_ns(candidate.len, searching.target.1.len);
      self.tried(&mut searching, delta);
    }
    Ok(Some(Pending::Search(searching)))
  }

  /// A trial's end, as the fragments' encoder ends one: taught to the window at its rank, its delta kept when it is
  /// shorter than the best so far, the object itself the first best; then the next candidate's trial begun.
  fn tried(&mut self, searching: &mut Searching, delta: Option<Vec<u8>>) {
    let trying = searching.trying;
    let rank = trying.saturating_add(1);
    let best = searching
      .best
      .as_ref()
      .map_or(searching.target_bytes.len(), |(_, kept)| kept.len());
    let len = delta.as_ref().map_or(usize::MAX, Vec::len);
    let saved = to_u64(best.saturating_sub(len));
    self.window.record(rank, saved, searching.trial_ns);
    if len < best {
      searching.best = delta.map(|kept| (trying, kept));
    }
    searching.trying = rank;
    searching.base_bytes = Vec::new();
    searching.search = Search::new();
    searching.trial_ns = 0;
  }

  /// The entry of a searched object: a delta against the candidate whose delta was shortest, named by its offset back
  /// or, for a client without `ofs-delta`, by its id, deflated a window a unit and its hold given back after it; or
  /// whole, its hold given back now, when no delta was shorter than the object.
  fn searched(
    &mut self,
    cx: &mut Context<'_>,
    searching: Searching,
    out: &mut impl Out,
  ) -> Result<Pending, StoreRefused> {
    let Searching {
      target,
      hold,
      candidates,
      best,
      ..
    } = searching;
    let found = best.and_then(|(index, delta)| Some((candidates.get(index).copied()?, delta)));
    let Some((base, delta)) = found else {
      cx.admission.release(hold);
      let again = target.clone();
      if let Some(pending) = self.large_whole(cx, target, out)? {
        return Ok(pending);
      }
      // r165: its read to be sent whole met a let-go window and the turn stopped; taken again as a search of no
      // candidates, which reads it and sends it whole.
      let hold = cx
        .admission
        .hold(search_charge(again.1.len, &[]))
        .map_err(|_| StoreRefused::PackTooLarge)?;
      return Ok(Pending::Search(Box::new(self::searching(
        again,
        Vec::new(),
        hold,
      ))));
    };
    let (id, row, stored) = target;
    let representation = if self.answer.ofs_delta() {
      Representation::OfsDelta { base: base.at }
    } else {
      Representation::RefDelta { base: base.id }
    };
    let header = Header {
      representation,
      size: to_u64(delta.len()),
    };
    let mut written = Vec::new();
    entry::write(header, self.at, &mut written).map_err(entry_refused)?;
    let first = delta.get(..cx.store.window()).unwrap_or(delta.as_slice());
    let mut learning = Learning {
      window: &mut self.window,
      levels: &mut self.levels,
      depth: self.depth,
      exchange: cx.exchange,
    };
    let level = fragment::level(row.kind, first, &mut learning).map_err(StoreRefused::Encoding)?;
    let large = Large {
      id,
      kind: row.kind,
      len: row.len,
      stored,
      at: self.at,
      depth: base.depth.saturating_add(1),
    };
    self.emit(&written, out);
    self.counters.loose_deltas = self.counters.loose_deltas.saturating_add(1);
    self.remember_large(large, cx.exchange);
    Ok(Pending::Deflate(Deflating {
      len: to_u64(delta.len()),
      from: Deflated::Made { delta, hold },
      next: 0,
      compress: super::deflaters::deflater(level),
    }))
  }

  /// The trailer, once the entries sent are the ones the header counted.
  fn trailer(&mut self, out: &mut impl Out) -> Result<Stage, StoreRefused> {
    if self.entries != self.counted {
      return Err(StoreRefused::CountChanged {
        counted: self.counted,
        sent: self.entries,
      });
    }
    let hasher = self.hasher.take().ok_or(StoreRefused::TrailerRefused)?;
    let checksum = hasher.finish().map_err(|_| StoreRefused::TrailerRefused)?;
    out.put(checksum.as_bytes());
    self.at = self.at.saturating_add(to_u64(checksum.as_bytes().len()));
    Ok(Stage::Done)
  }

  /// Puts `bytes` into the pack and its checksum.
  fn emit(&mut self, bytes: &[u8], out: &mut impl Out) {
    if let Some(hasher) = self.hasher.as_mut() {
      hasher.update(bytes);
    }
    out.put(bytes);
    self.at = self.at.saturating_add(to_u64(bytes.len()));
  }
}

/// A search of `target` against `candidates`, with the admission's `hold` on what it keeps, nothing yet read.
fn searching(
  target: (ObjectId, ObjectRow, StoredAt),
  candidates: Vec<Large>,
  hold: Hold,
) -> Searching {
  Searching {
    target,
    hold,
    target_bytes: Vec::new(),
    candidates,
    trying: 0,
    base_bytes: Vec::new(),
    search: Search::new(),
    trial_ns: 0,
    best: None,
  }
}

/// What a search of an object `target_len` bytes long against `candidates` holds at most (module docs, **What it
/// sends** 3): the object's bytes, the largest candidate's, a search over the two, and the best delta kept while
/// another candidate is searched. Every byte where a length does not fit in memory, which no admission holds.
fn search_charge(target_len: u64, candidates: &[Large]) -> u64 {
  let largest = candidates
    .iter()
    .map(|candidate| candidate.len)
    .max()
    .unwrap_or(0);
  let (Ok(target), Ok(source)) = (usize::try_from(target_len), usize::try_from(largest)) else {
    return u64::MAX;
  };
  let kept = if candidates.len() > 1 {
    to_u64(delta_most(source, target))
  } else {
    0
  };
  target_len
    .saturating_add(largest)
    .saturating_add(Search::held_most(source, target))
    .saturating_add(kept)
}

/// Reads the next window of the object `(id, len, content)` names into `bytes`, made with room for the whole object
/// when the first is read: whether one was read, none once the object is whole.
fn read_window(
  cx: &Context<'_>,
  (id, len, stored): (ObjectId, u64, StoredAt),
  bytes: &mut Vec<u8>,
) -> Result<bool, StoreRefused> {
  let store = cx.store;
  let read = to_u64(bytes.len());
  if read >= len {
    return Ok(false);
  }
  let unreadable = StoreRefused::Unreadable { id };
  // An object read where it lies in its pack (A-722's record v4) is read whole at once, its chain resolved.
  let content = match stored {
    StoredAt::Whole(content) => content,
    StoredAt::InPack(_) => {
      *bytes = cx
        .repository
        .read_stored(store, &stored, len)
        .ok_or(unreadable)?
        .into_owned();
      return Ok(true);
    }
  };
  if bytes.is_empty() {
    *bytes = Vec::with_capacity(usize::try_from(len).map_err(|_| unreadable)?);
  }
  let window = to_u64(store.window());
  let index = read.checked_div(window).ok_or(unreadable)?;
  let next = store.window_at(&content, len, index).ok_or(unreadable)?;
  let whole_windows = read.checked_rem(window) == Some(0);
  if !whole_windows || to_u64(next.len()) != window.min(len.saturating_sub(read)) {
    return Err(unreadable);
  }
  bytes.extend_from_slice(&next);
  Ok(true)
}

/// Window `index` of a delta the stream made, cut as the store cuts a content into windows of `window` bytes.
fn made_window(delta: &[u8], window: u64, index: u64) -> Result<&[u8], StoreRefused> {
  let from = index.saturating_mul(window);
  let to = from.saturating_add(window).min(to_u64(delta.len()));
  let (Ok(from), Ok(to)) = (usize::try_from(from), usize::try_from(to)) else {
    return Err(StoreRefused::FragmentUnreadable);
  };
  delta.get(from..to).ok_or(StoreRefused::FragmentUnreadable)
}

/// The part `part` names of `green`, with its source, when it is kept.
fn offered(repository: &Repository, green: u64, part: Part) -> Option<(Source, &Kept)> {
  match part {
    Part::Base { blobs } => {
      let (_, kept) = repository.base_part(green, blobs)?;
      let source = Source::Base {
        green,
        blobs,
        content: kept.content,
      };
      Some((source, kept))
    }
    Part::Version { version, blobs } => {
      let kept = repository.fragment_part(green, version, blobs)?;
      let source = Source::Fragment {
        green,
        version,
        blobs,
      };
      Some((source, kept))
    }
  }
}

/// The part of `green` the cover offers after `part`: a base pack's blobs after its other kinds, then the versions after
/// it (every version, without one), each version's blobs after its other kinds; `None` after the last version.
fn after(repository: &Repository, green: u64, part: Part) -> Option<Part> {
  let next = match part {
    Part::Base { blobs: false } => Part::Base { blobs: true },
    Part::Base { blobs: true } => Part::Version {
      version: repository
        .base_part(green, false)
        .map_or(0, |(version, _)| version.saturating_add(1)),
      blobs: false,
    },
    Part::Version {
      version,
      blobs: false,
    } => Part::Version {
      version,
      blobs: true,
    },
    Part::Version {
      version,
      blobs: true,
    } => Part::Version {
      version: version.saturating_add(1),
      blobs: false,
    },
  };
  match next {
    Part::Version { version, .. } if version >= repository.versions(green) => None,
    next => Some(next),
  }
}

/// The objects `green`'s base pack holds; none without one.
fn base_len(repository: &Repository, green: u64) -> u64 {
  [false, true]
    .into_iter()
    .filter_map(|blobs| repository.base_part(green, blobs))
    .fold(0_u64, |total, (_, kept)| {
      total.saturating_add(kept.positions.len())
    })
}

/// The entries one word of a reading's marks holds: its bits.
fn word_bits() -> usize {
  usize::try_from(u64::BITS).unwrap_or(1)
}

/// The first bit set in `bits` at `from` or after it.
fn next_bit(bits: &[u64], from: usize) -> Option<usize> {
  let size = word_bits();
  let mut word = from.checked_div(size)?;
  let skipped = u32::try_from(from.checked_rem(size)?).ok()?;
  let mut mask = u64::MAX.checked_shl(skipped).unwrap_or(0);
  loop {
    let set = bits.get(word)? & mask;
    if set != 0 {
      let bit = usize::try_from(set.trailing_zeros()).ok()?;
      return word.checked_mul(size)?.checked_add(bit);
    }
    word = word.checked_add(1)?;
    mask = u64::MAX;
  }
}

/// Marks `index`'s bit in `bits`.
fn set_bit(bits: &mut [u64], index: usize) {
  let size = word_bits();
  let (Some(word), Some(bit)) = (index.checked_div(size), index.checked_rem(size)) else {
    return;
  };
  let one = u32::try_from(bit)
    .ok()
    .and_then(|bit| 1_u64.checked_shl(bit));
  if let (Some(slot), Some(one)) = (bits.get_mut(word), one) {
    *slot |= one;
  }
}

/// The first index of `kept`'s position order at `cursor` or after it whose entry's position is `position` or past it,
/// found by galloping from `cursor` and then halving the stretch it ends in (Bentley and Yao's unbounded search [A:
/// "An almost optimal algorithm for unbounded searching", Information Processing Letters 5(3), 1976]): about twice the
/// logarithm of how far it lies, never the whole order. It reads at most `budget` entries of the order, each counted in
/// `read`; past them, `Err` with how far it got, every entry before which lies before `position`.
fn seek(
  kept: &Kept,
  cursor: usize,
  position: u64,
  (budget, read): (usize, &mut usize),
) -> Result<usize, usize> {
  let mut left = budget;
  let mut before = |at: usize| -> Option<bool> {
    left = left.checked_sub(1)?;
    *read = read.saturating_add(1);
    Some(
      kept
        .order
        .get(at)
        .and_then(|index| usize::try_from(*index).ok())
        .and_then(|index| kept.placed.get(index))
        .is_some_and(|entry| entry.position < position),
    )
  };
  let len = kept.order.len();
  let (mut low, mut high, mut step) = (cursor, cursor, 1_usize);
  while high < len {
    match before(high) {
      None => return Err(low),
      Some(false) => break,
      Some(true) => {
        low = high.saturating_add(1);
        high = cursor.saturating_add(step);
        step = step.saturating_mul(2);
      }
    }
  }
  let mut high = high.min(len);
  while low < high {
    let middle = low.saturating_add(high.saturating_sub(low) / 2);
    match before(middle) {
      None => return Err(low),
      Some(true) => low = middle.saturating_add(1),
      Some(false) => high = middle,
    }
  }
  Ok(low)
}

/// Marks, among `kept`'s entries, those of `keep`'s positions from `from` on, each found ahead of `cursor` in the
/// part's position order ([`seek`]), reading `handles` entries of the order at most ([`marking_reads`]) (a unit's first search may read
/// what one whole search reads): the position to go on from (the
/// one a search stopped short of, when the reading ran out inside it), or `None` once every one is marked.
fn mark(
  kept: &Kept,
  keep: &Positions,
  bits: &mut [u64],
  (from, cursor): (u64, &mut usize),
  (handles, read): (usize, &mut usize),
) -> Option<u64> {
  let mut at = from;
  loop {
    let position = keep.next_from(at)?;
    // A unit's first search may read what a whole search can (twice the order's bits, and two): every unit marks one.
    let whole = bits_of_len(kept.order.len())
      .saturating_mul(2)
      .saturating_add(2);
    let budget = if *read == 0 {
      handles.max(whole)
    } else {
      handles.saturating_sub(*read)
    };
    let found = match seek(kept, *cursor, position, (budget, read)) {
      Ok(found) => found,
      Err(reached) => {
        *cursor = reached;
        return Some(position);
      }
    };
    *cursor = found;
    let index = kept
      .order
      .get(found)
      .and_then(|index| usize::try_from(*index).ok())
      .filter(|index| {
        kept
          .placed
          .get(*index)
          .is_some_and(|entry| entry.position == position)
      });
    if let Some(index) = index {
      set_bit(bits, index);
      *cursor = found.saturating_add(1);
    }
    at = position.checked_add(1)?;
    if *read >= handles {
      return Some(at);
    }
  }
}

/// The bits `len` takes: the most halvings a search of `len` entries makes.
fn bits_of_len(len: usize) -> usize {
  usize::try_from(usize::BITS.saturating_sub(len.leading_zeros())).unwrap_or(usize::MAX)
}

/// The entries of a part's position order one unit of marking reads: a window's bytes, each read an entry of the order
/// and the entry it names (found by use: counted in handles, eight bytes each, a unit read sixteen times its window and
/// held its poll 928 ms in a deepen of git v2.0.0).
pub(super) fn marking_reads(store: &ContentStore) -> usize {
  store
    .window()
    .checked_div(size_of::<u32>().saturating_add(size_of::<Placed>()))
    .unwrap_or(0)
    .max(1)
}

/// The handles a window holds, `size_of::<u64>()` bytes each: what one unit reads of a list of positions.
pub(super) fn handles(store: &ContentStore) -> usize {
  store
    .window()
    .checked_div(size_of::<u64>())
    .unwrap_or(0)
    .max(1)
}

/// The kept part `source` names: refused where a base pack was made again since, or a version's fragment is gone.
fn part_of(repository: &Repository, source: Source) -> Result<&Kept, StoreRefused> {
  match source {
    Source::Fragment {
      green,
      version,
      blobs,
    } => repository
      .fragment_part(green, version, blobs)
      .ok_or(StoreRefused::NotEncoded {
        version: to_u64(version),
      }),
    Source::Base {
      green,
      blobs,
      content,
    } => repository
      .base_part(green, blobs)
      .map(|(_, kept)| kept)
      .filter(|kept| kept.content == content)
      .ok_or(StoreRefused::BaseRebuilt { green }),
  }
}

/// The header of the entry `offset` bytes into a kept part, and its bytes as kept. The admission is unbounded since
/// nothing is made from the size it names: the entry's data is copied, never inflated.
fn read_header(
  store: &ContentStore,
  kept: &Kept,
  offset: u64,
  format: ObjectFormat,
) -> Result<(Header, Vec<u8>), StoreRefused> {
  let longest = to_u64(entry::longest_header(format));
  let to = offset.saturating_add(longest).min(kept.len);
  let bytes = read_range(store, (kept.content, kept.len), offset, to)?;
  let at = PACK_HEADER_LEN.saturating_add(offset);
  let (header, len) = entry::read(&bytes, at, format, &mut Admission::new(u64::MAX))
    .ok()
    .flatten()
    .ok_or(StoreRefused::FragmentUnreadable)?;
  let kept_header = bytes
    .get(..len)
    .ok_or(StoreRefused::FragmentUnreadable)?
    .to_vec();
  Ok((header, kept_header))
}

/// The bytes `from` to `to` of a kept content `len` bytes long, read from the windows they lie in.
fn read_range(
  store: &ContentStore,
  (content, len): (ContentId, u64),
  from: u64,
  to: u64,
) -> Result<Vec<u8>, StoreRefused> {
  let window = to_u64(store.window());
  let mut bytes = Vec::new();
  let mut at = from;
  while at < to {
    let index = at.checked_div(window).unwrap_or(0);
    let start = index.saturating_mul(window);
    let held = store
      .window_at(&content, len, index)
      .ok_or(StoreRefused::FragmentUnreadable)?;
    let upto = to.min(start.saturating_add(to_u64(held.len())));
    if upto <= at {
      return Err(StoreRefused::FragmentUnreadable);
    }
    bytes.extend_from_slice(within(
      &held,
      at.saturating_sub(start),
      upto.saturating_sub(start),
    )?);
    at = upto;
  }
  Ok(bytes)
}

/// `bytes` from `from` to `to`, refused past their end.
fn within(bytes: &[u8], from: u64, to: u64) -> Result<&[u8], StoreRefused> {
  let from = usize::try_from(from).map_err(|_| StoreRefused::FragmentUnreadable)?;
  let to = usize::try_from(to).map_err(|_| StoreRefused::FragmentUnreadable)?;
  bytes.get(from..to).ok_or(StoreRefused::FragmentUnreadable)
}

/// The position of the head a run wants: its last version's.
fn run_head(repository: &Repository, versions: Versions) -> Result<u64, StoreRefused> {
  let (green, through) = (versions.green, versions.through);
  repository
    .head_position(green, version_index(green, through)?)
    .ok_or(StoreRefused::NoVersion {
      green,
      version: through,
    })
}

/// A version's index in its green's chain, refused where it cannot be one.
fn version_index(green: u64, version: u64) -> Result<usize, StoreRefused> {
  usize::try_from(version).map_err(|_| StoreRefused::NoVersion { green, version })
}

/// An entry's header the writer refused, as the store says it.
fn entry_refused(refusal: entry::Refusal) -> StoreRefused {
  StoreRefused::Encoding(FragmentRefused::Entry(refusal))
}

fn to_u64(len: usize) -> u64 {
  u64::try_from(len).unwrap_or(u64::MAX)
}

// The tests' switches, kept on the thread that streams.
#[cfg(test)]
thread_local! {
  /// Whether no pushed entry is sent on as it lies (true: every object encoded afresh): a test's way to stand a stream at
  /// an object's encode, search and whole send reads of its pack (r165's reds).
  pub(crate) static REUSE_OFF: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
  /// Whether no pushed entry is sent on by its kept CRC32 (true: each rebuilt to be checked, its raw entry, base and
  /// object read): a test's way to stand a stream at the check's reads (r165's reds).
  pub(crate) static KEPT_OFF: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}
