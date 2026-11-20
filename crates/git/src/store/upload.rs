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
//! 3. **The rest**, which no part covered (a version not yet encoded), encoded in position order by the fragments'
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
//! answer's handles read; one object of a run judged by its row; a window's worth of a part's wanted positions marked
//! among its entries, each found by halving the part's entries in position order, so a part's reading costs what it
//! sends and never the whole part; one wanted entry of a part judged; one object still wanted offered its version's
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

use flate2::{Compress, Compression};
use slates_vcs::{ContentId, ContentStore};

use crate::entry::{self, Header, PACK_HEADER_LEN, Representation};
use crate::object_name::{Hasher, Kind, ObjectFormat, ObjectId};
use crate::out::Out;
use crate::pack;
use crate::{Admission, Hold, PastAdmission};

use super::deltas::{Search, SearchStep, delta_most};
use super::fragment::{self, Encoder, FragmentRefused, Learning, Placed, Stored, ToEncode};
use super::learn::{Clock, Exchange, Levels, Window};
use super::positions::Positions;
use super::repository::{Kept, Repository, StoreRefused};
use super::rows::ObjectRow;

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
}

/// Where the stream is (module docs).
enum Stage {
  /// Reading the answer's objects into the wanted set, from the one at `next`.
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
  /// Encoding what no part covered, from position `from` on.
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
  /// worth of handles a unit; `marking` is `None` once every one is marked.
  Marked {
    bits: Vec<u64>,
    marking: Option<u64>,
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
  compress: Compress,
}

/// What a [`Deflating`] reads.
enum Deflated {
  /// A kept content.
  Content(ContentId),
  /// A delta the stream made, and the admission's hold on what its search kept, given back after its last window.
  Made { delta: Vec<u8>, hold: Hold },
}

/// An object past a window that the loose stage sent: what a later one's search reads it by and names it by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Large {
  id: ObjectId,
  kind: Kind,
  len: u64,
  content: ContentId,
  /// Where its entry begins in the pack.
  at: u64,
  /// Its delta chain's depth.
  depth: u64,
}

/// An object past a window being searched (module docs, **What it sends** 3): its bytes read a window a unit, then
/// each candidate's, each searched a window's bytes a unit; the smallest delta shorter than the object wins.
struct Searching {
  /// The object.
  target: (ObjectId, ObjectRow, ContentId),
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
  by_entry: Option<ByEntry>,
  pending: Option<Pending>,
  counters: UploadCounters,
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
      hasher: Some(Hasher::raw(format)),
      window,
      levels,
      depth,
      encoder,
      large: BTreeMap::new(),
      by_entry: None,
      pending: None,
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
        Ok(Progress::More) => {}
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
      Stage::Loose { from } => self.loose(cx, from, out),
      Stage::Trailer => self.trailer(out),
      Stage::Done => Ok(Stage::Done),
      Stage::Refused(refused) => Err(refused),
    }
  }

  /// Reads a window's worth of the answer's runs of objects into the wanted set, a run `size_of::<(u64, u64)>()` bytes
  /// (A-745: the answer carries its objects as runs, so a clone's few runs are read at once whatever it holds).
  fn read_objects(&mut self, cx: &Context<'_>, next: usize) -> Result<Stage, StoreRefused> {
    let handles = handles(cx.store);
    let runs = self.answer.objects().run_pairs();
    let end = next.saturating_add(handles).min(runs.len());
    let window =
      Positions::from_runs(runs.get(next..end).unwrap_or_default().to_vec()).unwrap_or_default();
    self.wanted = self.wanted.union(&window);
    if end < runs.len() {
      return Ok(Stage::Objects { next: end });
    }
    self.gather_from(0)
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
    if let Chosen::Marked { bits, marking } = &mut by_entry.chosen
      && let Some(from) = *marking
    {
      *marking = mark(kept, &by_entry.keep, bits, from, handles(cx.store));
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
    self.send_entry(cx, kept, &mut by_entry, (index, entry, end), out)?;
    Ok(Some(by_entry))
  }

  /// Sends a kept entry that ends at `end` in its part (module docs, **What the client can read**).
  fn send_entry(
    &mut self,
    cx: &mut Context<'_>,
    kept: &Kept,
    by_entry: &mut ByEntry,
    (index, entry, end): (usize, Placed, u64),
    out: &mut impl Out,
  ) -> Result<(), StoreRefused> {
    let (header, kept_header) = read_header(cx.store, kept, entry.offset, self.format)?;
    by_entry.moved.push((index, self.at));
    self.entries = self.entries.saturating_add(1);
    let Some(representation) = self.readable(cx, kept, by_entry, &entry, header.representation)?
    else {
      self.counters.made_whole = self.counters.made_whole.saturating_add(1);
      let found = cx.repository.stored_at(cx.store, entry.position)?;
      self.pending = Some(self.send_whole(cx, found, out)?);
      return Ok(());
    };
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
    Ok(())
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
    (id, row, content): (ObjectId, ObjectRow, ContentId),
    out: &mut impl Out,
  ) -> Result<Pending, StoreRefused> {
    let mut written = Vec::new();
    let header = Header {
      representation: Representation::Whole(row.kind),
      size: row.len,
    };
    entry::write(header, self.at, &mut written).map_err(entry_refused)?;
    let first = cx
      .store
      .window_at(&content, row.len, 0)
      .ok_or(StoreRefused::Unreadable { id })?;
    let mut learning = Learning {
      window: &mut self.window,
      levels: &mut self.levels,
      depth: self.depth,
      exchange: cx.exchange,
    };
    let level = fragment::level(row.kind, &first, &mut learning).map_err(StoreRefused::Encoding)?;
    self.emit(&written, out);
    Ok(Pending::Deflate(Deflating {
      from: Deflated::Content(content),
      len: row.len,
      next: 0,
      compress: Compress::new(Compression::new(level), true),
    }))
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
    let bytes = store
      .window_at(&content, len, index)
      .ok_or(StoreRefused::FragmentUnreadable)?;
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
      Deflated::Made { delta, .. } => Cow::Borrowed(made_window(delta, window, deflating.next)?),
    };
    let last = deflating.next.saturating_add(1) >= windows;
    let given = fragment::deflate_more(&mut deflating.compress, &bytes, last)
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
      return Ok(Stage::Loose { from: 0 });
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

  /// One object no part covered (module docs, **What it sends** 3): encoded with the fragments' encoder when it is a
  /// window long at most; past a window, searched against the earlier ones past a window when the request's admission
  /// holds the search, and sent whole otherwise (`loose_large`).
  fn loose(
    &mut self,
    cx: &mut Context<'_>,
    from: u64,
    out: &mut impl Out,
  ) -> Result<Stage, StoreRefused> {
    let Some(position) = self.wanted.next_from(from) else {
      return Ok(Stage::Trailer);
    };
    self.entries = self.entries.saturating_add(1);
    let (id, row, content) = cx.repository.stored_at(cx.store, position)?;
    let next = Stage::Loose {
      from: position.saturating_add(1),
    };
    if row.len > to_u64(cx.store.window()) {
      self.pending = Some(self.loose_large(cx, (id, row, content), out)?);
      return Ok(next);
    }
    let bytes = cx
      .store
      .object_bytes(&content, row.len)
      .ok_or(StoreRefused::Unreadable { id })?;
    let object = ToEncode {
      id,
      position,
      kind: row.kind,
      bytes,
      named: None,
    };
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
    Ok(next)
  }

  /// An object past a window (module docs, **What it sends** 3): searched when it has candidates and the request's
  /// admission can hold the search, sent whole otherwise, and counted when the admission was what refused. What is
  /// owed next: the search, or the object's deflating.
  fn loose_large(
    &mut self,
    cx: &mut Context<'_>,
    target: (ObjectId, ObjectRow, ContentId),
    out: &mut impl Out,
  ) -> Result<Pending, StoreRefused> {
    let candidates = self.candidates(target.1.kind, cx.exchange);
    if !candidates.is_empty() {
      match cx.admission.hold(search_charge(target.1.len, &candidates)) {
        Ok(hold) => {
          return Ok(Pending::Search(Box::new(searching(
            target, candidates, hold,
          ))));
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
    target: (ObjectId, ObjectRow, ContentId),
    out: &mut impl Out,
  ) -> Result<Pending, StoreRefused> {
    self.counters.loose_whole = self.counters.loose_whole.saturating_add(1);
    let large = Large {
      id: target.0,
      kind: target.1.kind,
      len: target.1.len,
      content: target.2,
      at: self.at,
      depth: 0,
    };
    let pending = self.send_whole(cx, target, out)?;
    self.remember_large(large, cx.exchange);
    Ok(pending)
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
    let (id, row, content) = &searching.target;
    let target = (*id, row.len, *content);
    if read_window(cx.store, target, &mut searching.target_bytes)? {
      return Ok(Some(Pending::Search(searching)));
    }
    let Some(candidate) = searching.candidates.get(searching.trying).copied() else {
      return self.searched(cx, *searching, out).map(Some);
    };
    let base = (candidate.id, candidate.len, candidate.content);
    let step = if read_window(cx.store, base, &mut searching.base_bytes)? {
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
      return self.large_whole(cx, target, out);
    };
    let (id, row, content) = target;
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
      content,
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
      compress: Compress::new(Compression::new(level), true),
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
  target: (ObjectId, ObjectRow, ContentId),
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
  store: &ContentStore,
  (id, len, content): (ObjectId, u64, ContentId),
  bytes: &mut Vec<u8>,
) -> Result<bool, StoreRefused> {
  let read = to_u64(bytes.len());
  if read >= len {
    return Ok(false);
  }
  let unreadable = StoreRefused::Unreadable { id };
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

/// The index of `kept`'s entry whose object is at `position`, found by halving its entries in position order.
fn entry_of(kept: &Kept, position: u64) -> Option<usize> {
  let position_of = |index: &u32| {
    usize::try_from(*index)
      .ok()
      .and_then(|index| kept.placed.get(index))
      .map_or(u64::MAX, |entry| entry.position)
  };
  let at = kept
    .order
    .binary_search_by_key(&position, position_of)
    .ok()?;
  usize::try_from(*kept.order.get(at)?).ok()
}

/// Marks, among `kept`'s entries, those of `handles` of `keep`'s positions from `from` on: the position to go on from,
/// or `None` once every one is marked.
fn mark(kept: &Kept, keep: &Positions, bits: &mut [u64], from: u64, handles: usize) -> Option<u64> {
  let mut at = from;
  for _ in 0..handles {
    let position = keep.next_from(at)?;
    if let Some(index) = entry_of(kept, position) {
      set_bit(bits, index);
    }
    at = position.checked_add(1)?;
  }
  Some(at)
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
