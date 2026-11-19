//! A version's fragments, or a green's base pack, encoded a unit at a time (A-653 continued (2)). This first state is
//! the failing test's: today's encoding, whole, in one unit.

use slates_vcs::ContentStore;

use super::repository::{Repository, StoreRefused};
use super::upload::{Progress, Turn};

/// What an encoding makes: a version's two fragments, or a green's base pack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
  /// `green`'s version `version`'s fragments.
  Version { green: u64, version: usize },
  /// `green`'s base pack.
  Base { green: u64 },
}

/// What an encoding did, for a test to see its turns sliced.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EncodingCounters {
  /// Turns taken.
  pub turns: u64,
  /// Units of work done.
  pub units: u64,
  /// Pairs of trees walked for the bases a reference delta may name.
  pub trees_walked: u64,
  /// Objects' rows read into the encoding's order.
  pub rows_read: u64,
  /// Objects encoded.
  pub objects: u64,
}

/// A version's fragments, or a green's base pack, being encoded.
pub struct Encoding {
  target: Target,
  done: bool,
  counters: EncodingCounters,
}

impl Encoding {
  /// The encoding of `green`'s version `version` as its two fragments.
  pub fn fragments(
    _repository: &Repository,
    _store: &ContentStore,
    green: u64,
    version: usize,
  ) -> Result<Encoding, StoreRefused> {
    Ok(Encoding {
      target: Target::Version { green, version },
      done: false,
      counters: EncodingCounters::default(),
    })
  }

  /// The making of `green`'s base pack again.
  pub fn base(_repository: &Repository, green: u64) -> Result<Encoding, StoreRefused> {
    Ok(Encoding {
      target: Target::Base { green },
      done: false,
      counters: EncodingCounters::default(),
    })
  }

  /// What the encoding did.
  pub fn counters(&self) -> EncodingCounters {
    self.counters
  }

  /// One turn: today's encoding, whole, as one unit.
  pub fn advance(
    &mut self,
    repository: &mut Repository,
    store: &mut ContentStore,
    turn: Turn<'_>,
  ) -> Result<Progress, StoreRefused> {
    self.counters.turns = self.counters.turns.saturating_add(1);
    if self.done {
      return Ok(Progress::Done);
    }
    self.counters.units = self.counters.units.saturating_add(1);
    let entries = match self.target {
      Target::Version { green, version } => {
        repository.encode_fragments(store, (green, version), turn.clock, turn.exchange)?;
        repository
          .fragments(green, version)
          .map_or(0, |(objects, blobs)| objects.len().saturating_add(blobs.len()))
      }
      Target::Base { green } => {
        repository.rebuild_base(store, green, turn.clock, turn.exchange)?;
        [false, true]
          .iter()
          .filter_map(|blobs| repository.base_part(green, *blobs))
          .map(|(_, kept)| kept.placed.len())
          .sum()
      }
    };
    self.counters.objects = u64::try_from(entries).unwrap_or(u64::MAX);
    self.done = true;
    Ok(Progress::Done)
  }
}
