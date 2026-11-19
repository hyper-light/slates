//! upload-pack's pack source over the store's stream (A-653; A-640's `PackSource`, §4.17.2 "A fetch" item 3): the
//! seam through which `upload::serve` writes a fetch's pack, met by A-639's [`Upload`].
//!
//! **A pack begun.** The plan the negotiation made (its runs, its objects one by one, the request's filter and the
//! client's `thin-pack` and `ofs-delta`) is borrowed for the call alone, so [`StorePacks::begin`] copies it into an
//! answer the stream owns ([`Answered`]), its lists charged to the request's admission first: what a pack holds until
//! its end is admitted before it is held. A run is A-615's `Run`, the stream's [`Versions`]; the filter answers by an
//! object's kind and size at no depth, the only filters the negotiation lets a run be sent under.
//!
//! **A step.** [`StorePacks::write`] runs the stream's units, each at most a window of work, until it holds `room`
//! bytes or the pack has ended, then hands out at most `room`: so a step writes at least one byte while any are left,
//! as the seam asks, and the bytes it holds past a step are at most one unit's. The store is lent again for each
//! step and the stream keeps no borrow of it. A step's work is bounded by the room it is given; the first step also
//! reads the plan and gathers the runs' objects, since the header counts them before any byte can be sent.
//!
//! **Learning.** The stream encodes what no kept part covers with the store's learned rules, timed by the source's
//! clock (the owner's: the shard's, §4.3) at the owner's exchange; a pack learns on a copy of the rules, so packs
//! written at once do not race on them.

use crate::Admission;
use crate::negotiation::{Filter, Run};
use crate::object_name::Kind;
use crate::upload::{PackPlan, PackRefused, PackSource, Served};

use super::learn::{Clock, Exchange};
use super::upload::{Answered, Progress, Turn, Upload, UploadCounters, Versions};

/// The store's packs as upload-pack's source (module docs): the owner's clock and exchange, and what the packs it
/// finished did.
pub struct StorePacks<C> {
  clock: C,
  exchange: Exchange,
  counters: UploadCounters,
}

impl<C: Clock> StorePacks<C> {
  /// A source whose packs learn by `clock` at `exchange`.
  pub fn new(clock: C, exchange: Exchange) -> Self {
    Self {
      clock,
      exchange,
      counters: UploadCounters::default(),
    }
  }

  /// What its finished packs did, added up.
  pub fn counters(&self) -> UploadCounters {
    self.counters
  }
}

/// A plan the stream owns: the negotiation's runs and objects, the filter, the client's capabilities.
struct Plan {
  runs: Vec<Versions>,
  objects: Vec<u64>,
  filter: Option<Filter>,
  thin: bool,
  ofs_delta: bool,
}

impl Answered for Plan {
  fn runs(&self) -> &[Versions] {
    &self.runs
  }

  fn objects(&self) -> &[u64] {
    &self.objects
  }

  fn thin(&self) -> bool {
    self.thin
  }

  fn ofs_delta(&self) -> bool {
    self.ofs_delta
  }

  fn admits(&self, kind: Kind, size: u64) -> bool {
    self
      .filter
      .as_ref()
      .is_none_or(|filter| filter.admits(kind, size, 0))
  }
}

/// One pack the source is writing: its stream, the bytes the stream gave that no step has handed out yet, and whether
/// the stream has ended.
pub struct StorePack {
  upload: Upload<Plan>,
  held: Vec<u8>,
  done: bool,
}

impl StorePack {
  /// What the pack's stream did so far.
  pub fn counters(&self) -> UploadCounters {
    self.upload.counters()
  }
}

/// A-615's run as the stream names it.
fn versions(run: &Run<u64>) -> Versions {
  Versions {
    green: run.green,
    after: run.after,
    through: run.through,
  }
}

impl<C: Clock> PackSource for StorePacks<C> {
  type Pack = StorePack;

  fn begin(
    &mut self,
    served: &Served<'_>,
    plan: &PackPlan<'_>,
    admission: &mut Admission,
  ) -> Result<StorePack, PackRefused> {
    admission
      .charge_items::<Versions>(plan.runs.len())
      .and_then(|()| admission.charge_items::<u64>(plan.objects.len()))
      .map_err(PackRefused::PastAdmission)?;
    let owned = Plan {
      runs: plan.runs.iter().map(versions).collect(),
      objects: plan.objects.to_vec(),
      filter: plan.filter.cloned(),
      thin: plan.thin,
      ofs_delta: plan.ofs_delta,
    };
    Ok(StorePack {
      upload: Upload::new(owned, served.repository),
      held: Vec::new(),
      done: false,
    })
  }

  fn write(
    &mut self,
    served: &Served<'_>,
    pack: &mut StorePack,
    out: &mut Vec<u8>,
    room: usize,
  ) -> Result<bool, PackRefused> {
    while pack.held.len() < room && !pack.done {
      let turn = Turn {
        clock: &mut self.clock,
        exchange: self.exchange,
        quantum_ns: 0,
      };
      let progress = pack
        .upload
        .advance(served.repository, served.store, turn, &mut pack.held)
        .map_err(PackRefused::Store)?;
      if progress == Progress::Done {
        pack.done = true;
        self.counters = added(self.counters, pack.upload.counters());
      }
    }
    let handed = pack.held.len().min(room);
    out.extend(pack.held.drain(..handed));
    Ok(!(pack.done && pack.held.is_empty()))
  }
}

/// Two streams' counters added up.
fn added(a: UploadCounters, b: UploadCounters) -> UploadCounters {
  UploadCounters {
    turns: a.turns.saturating_add(b.turns),
    parts_copied: a.parts_copied.saturating_add(b.parts_copied),
    parts_by_entry: a.parts_by_entry.saturating_add(b.parts_by_entry),
    entries_judged: a.entries_judged.saturating_add(b.entries_judged),
    entries_copied: a.entries_copied.saturating_add(b.entries_copied),
    headers_rewritten: a.headers_rewritten.saturating_add(b.headers_rewritten),
    made_whole: a.made_whole.saturating_add(b.made_whole),
    loose_whole: a.loose_whole.saturating_add(b.loose_whole),
    loose_deltas: a.loose_deltas.saturating_add(b.loose_deltas),
  }
}
