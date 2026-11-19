//! The pack from the store's fragments, as the service's pack source (A-640 part 3, joining A-639): the negotiation's
//! answer handed to A-639's stream through its [`Answered`], and the stream stepped a turn at a time on the owner's
//! clock, its bytes handed to the answer as it frames them.
//!
//! **One turn a call.** [`Fragments::write`] hands over what the last turn wrote, at most the room it is given; when
//! none is left it takes one turn of A-639's stream ([`Upload::advance`]): units of work until the owner's quantum is
//! spent, at least one. A turn may write nothing yet, while the stream reads the answer's handles and gathers what the
//! runs carry before the header counts them; the answer then ends its step and asks again at the next. So a step does
//! at most one turn of the stream's work and frames at most what that turn wrote, and the stream holds no borrow of the
//! store between turns, as A-639 keeps it.
//!
//! **Owned.** The plan is copied when the pack begins (its runs, its objects, its filter), each charged to the request's
//! admission first, since the stream outlives every lending of the store; what a turn wrote is kept until the answer
//! has taken it, one turn's bytes at most.

use super::{PackPlan, PackRefused, PackSource, Served};
use crate::Admission;
use crate::negotiation::Filter;
use crate::object_name::Kind;
use crate::store::learn::{Clock, Exchange};
use crate::store::{Answered, Progress, Turn, Upload, Versions};

/// The negotiation's answer as A-639's stream reads it, owned: its runs, its single objects, its filter, and what the
/// client reads.
#[derive(Clone, Debug)]
pub struct Planned {
  /// The runs of versions, in the negotiation's order.
  runs: Vec<Versions>,
  /// The single objects, by position, in ascending order.
  objects: Vec<u64>,
  /// The request's filter, one that decides by an object's own row.
  filter: Option<Filter>,
  /// `thin-pack`.
  thin: bool,
  /// `ofs-delta`.
  ofs_delta: bool,
}

impl Planned {
  /// The answer `plan` names, its lists charged to `admission` before they are copied.
  fn of(plan: &PackPlan<'_>, admission: &mut Admission) -> Result<Planned, PackRefused> {
    admission
      .charge_items::<Versions>(plan.runs.len())
      .and_then(|()| admission.charge_items::<u64>(plan.objects.len()))
      .map_err(PackRefused::PastAdmission)?;
    Ok(Planned {
      runs: plan
        .runs
        .iter()
        .map(|run| Versions {
          green: run.green,
          after: run.after,
          through: run.through,
        })
        .collect(),
      objects: plan.objects.to_vec(),
      filter: plan.filter.cloned(),
      thin: plan.thin,
      ofs_delta: plan.ofs_delta,
    })
  }
}

impl Answered for Planned {
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

/// A pack A-639's stream is writing: the stream, and what its last turn wrote that the answer has not taken.
pub struct Streamed {
  /// The stream.
  upload: Upload<Planned>,
  /// What the last turn wrote.
  written: Vec<u8>,
  /// How much of it the answer has taken.
  taken: usize,
  /// Whether the stream wrote the pack's last bytes, its trailer.
  done: bool,
}

/// A-639's stream as the service's [`PackSource`]: each turn on the owner's clock, at its exchange (what a saved byte
/// is worth in time, §4.11), within its quantum (§4.3's step budget); all three the owner's to derive.
pub struct Fragments<'c> {
  /// The owner's clock.
  pub clock: &'c mut dyn Clock,
  /// What a saved byte is worth in time.
  pub exchange: Exchange,
  /// The nanoseconds a turn may spend.
  pub quantum_ns: u64,
}

impl PackSource for Fragments<'_> {
  type Pack = Streamed;

  fn begin(
    &mut self,
    served: &Served<'_>,
    plan: &PackPlan<'_>,
    admission: &mut Admission,
  ) -> Result<Streamed, PackRefused> {
    Ok(Streamed {
      upload: Upload::new(Planned::of(plan, admission)?, served.repository),
      written: Vec::new(),
      taken: 0,
      done: false,
    })
  }

  fn write(
    &mut self,
    served: &Served<'_>,
    pack: &mut Streamed,
    out: &mut Vec<u8>,
    room: usize,
  ) -> Result<bool, PackRefused> {
    if pack.taken >= pack.written.len() && !pack.done {
      pack.written.clear();
      pack.taken = 0;
      let turn = Turn {
        clock: &mut *self.clock,
        exchange: self.exchange,
        quantum_ns: self.quantum_ns,
      };
      let progress = pack
        .upload
        .advance(served.repository, served.store, turn, &mut pack.written)
        .map_err(PackRefused::Store)?;
      pack.done = progress == Progress::Done;
    }
    let rest = pack.written.get(pack.taken..).unwrap_or_default();
    let chunk = rest.get(..room).unwrap_or(rest);
    out.extend_from_slice(chunk);
    pack.taken = pack.taken.saturating_add(chunk.len());
    Ok(!pack.done || pack.taken < pack.written.len())
  }
}
