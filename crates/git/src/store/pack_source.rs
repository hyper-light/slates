//! upload-pack's pack source over the store's stream (A-653; A-640's `PackSource`). Stub for the failing tests.

use crate::Admission;
use crate::upload::{PackPlan, PackRefused, PackSource, Served};

use super::learn::{Clock, Exchange};
use super::upload::UploadCounters;

/// The store's packs as upload-pack's source.
pub struct StorePacks<C> {
  clock: C,
  exchange: Exchange,
  counters: UploadCounters,
}

impl<C: Clock> StorePacks<C> {
  /// A source whose streams learn by `clock` at `exchange`.
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

/// One pack the source is writing.
pub struct StorePack {
  done: bool,
}

impl<C: Clock> PackSource for StorePacks<C> {
  type Pack = StorePack;

  fn begin(
    &mut self,
    served: &Served<'_>,
    plan: &PackPlan<'_>,
    admission: &mut Admission,
  ) -> Result<StorePack, PackRefused> {
    let _ = (served, plan, admission, &mut self.clock, self.exchange);
    Ok(StorePack { done: true })
  }

  fn write(
    &mut self,
    served: &Served<'_>,
    pack: &mut StorePack,
    out: &mut Vec<u8>,
    room: usize,
  ) -> Result<bool, PackRefused> {
    let _ = (served, out, room);
    Ok(!pack.done)
  }
}
