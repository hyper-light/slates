//! upload-pack's pack, streamed from the store (A-639; §4.17.2 "A fetch", item 3, and "Pieces (git)", piece 7's "packs
//! from fragments"). Stub for the failing tests.

use slates_vcs::ContentStore;

use crate::object_name::Kind;
use crate::out::Out;

use super::learn::{Clock, Exchange};
use super::repository::{Repository, StoreRefused};

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
  /// The objects the pack carries one by one, by position, each once, in ascending order.
  fn objects(&self) -> &[u64];
  /// Whether the client said `thin-pack`: a delta may name a base the client holds.
  fn thin(&self) -> bool;
  /// Whether the client said `ofs-delta`: a delta may name its base by its offset back.
  fn ofs_delta(&self) -> bool;
  /// Whether the request's filter sends an object of `kind` and `size` that a run brought (A-615's per-object filter,
  /// which decides by kind and size alone); every one with no filter.
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
}

/// upload-pack's pack, streamed (module docs).
pub struct Upload<A> {
  answer: A,
  counters: UploadCounters,
}

impl<A: Answered> Upload<A> {
  /// The stream of the pack that answers `answer` from `repository`.
  pub fn new(answer: A, repository: &Repository) -> Upload<A> {
    let _ = repository;
    Upload {
      answer,
      counters: UploadCounters::default(),
    }
  }

  /// What the stream did.
  pub fn counters(&self) -> UploadCounters {
    self.counters
  }

  /// One turn: units of work until the turn's quantum is spent, at least one, the bytes put to `out`.
  pub fn advance(
    &mut self,
    repository: &Repository,
    store: &ContentStore,
    turn: Turn<'_>,
    out: &mut impl Out,
  ) -> Result<Progress, StoreRefused> {
    let _ = (repository, store, turn, out, &self.answer);
    Ok(Progress::Done)
  }
}
