//! Quotas and accounting (D-13): a bounded volume reserves its quota and refuses the byte that
//! would exceed it before anything is copied; a dynamic volume grows by increments the pressure
//! source allows; `referenced_bytes` charges every chunk the head reaches in full and
//! `unique_bytes` what the head alone holds since its last snapshot.

/// Where a dynamic volume asks before growing: the daemon's pressure source in Phase 2, a fixed
/// answer in tests.
pub trait PressureSource {
  /// Whether `bytes` more may be taken now.
  fn may_grow(&mut self, bytes: u64) -> bool;
}

/// A source that always agrees up to a ceiling.
#[derive(Debug, Clone)]
pub struct Ceiling {
  /// Bytes granted so far.
  pub granted: u64,
  /// The ceiling.
  pub limit: u64,
}

impl PressureSource for Ceiling {
  fn may_grow(&mut self, bytes: u64) -> bool {
    match self.granted.checked_add(bytes) {
      Some(total) if total <= self.limit => {
        self.granted = total;
        true
      }
      _ => false,
    }
  }
}

/// The volume's size policy.
pub enum Quota {
  /// A fixed quota reserved at creation.
  Bounded {
    /// Bytes.
    limit: u64,
  },
  /// Grows while the pressure source allows, never past `max`.
  Dynamic {
    /// The ceiling.
    max: u64,
    /// The source asked before each increment.
    source: Box<dyn PressureSource>,
    /// Bytes the source has granted so far.
    granted: u64,
    /// Growth requests the source refused: each is one pressure event (T-1.5).
    denied: u64,
  },
}

impl std::fmt::Debug for Quota {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Bounded { limit } => f.debug_struct("Bounded").field("limit", limit).finish(),
      Self::Dynamic {
        max,
        granted,
        denied,
        ..
      } => f
        .debug_struct("Dynamic")
        .field("max", max)
        .field("granted", granted)
        .field("denied", denied)
        .finish(),
    }
  }
}

/// The exact counters of D-13.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Accounting {
  /// Bytes of every chunk the head reaches, charged in full.
  pub referenced_bytes: u64,
  /// Bytes the head alone holds since its last snapshot (freed if the head were dropped).
  pub unique_bytes: u64,
}

impl Quota {
  /// Whether `referenced + more` fits; for a dynamic volume this may ask the source.
  pub fn admit(&mut self, referenced: u64, more: u64) -> bool {
    let Some(total) = referenced.checked_add(more) else {
      return false;
    };
    match self {
      Self::Bounded { limit } => total <= *limit,
      Self::Dynamic {
        max,
        source,
        granted,
        denied,
      } => {
        if total > *max {
          return false;
        }
        if total <= *granted {
          return true;
        }
        let needed = total - *granted;
        if source.may_grow(needed) {
          *granted = total;
          true
        } else {
          *denied += 1;
          false
        }
      }
    }
  }

  /// Growth requests the pressure source refused so far (zero for a bounded quota).
  pub fn denials(&self) -> u64 {
    match self {
      Self::Bounded { .. } => 0,
      Self::Dynamic { denied, .. } => *denied,
    }
  }

  /// Resizes (§4.4 `resize`): a bounded quota takes the new limit unless `referenced` already
  /// exceeds it; a dynamic quota takes the new maximum.
  pub fn resize(&mut self, referenced: u64, new_limit: u64) -> Result<(), crate::error::VfsError> {
    if referenced > new_limit {
      return Err(crate::error::VfsError::NoSpace);
    }
    match self {
      Self::Bounded { limit } => *limit = new_limit,
      Self::Dynamic { max, .. } => *max = new_limit,
    }
    Ok(())
  }

  /// The quota's ceiling.
  pub fn limit(&self) -> u64 {
    match self {
      Self::Bounded { limit } => *limit,
      Self::Dynamic { max, .. } => *max,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn bounded_refuses_the_byte_past_its_limit_and_dynamic_asks_its_source() {
    let mut b = Quota::Bounded { limit: 100 };
    assert!(b.admit(90, 10));
    assert!(!b.admit(90, 11));
    let mut d = Quota::Dynamic {
      max: 1000,
      source: Box::new(Ceiling {
        granted: 0,
        limit: 150,
      }),
      granted: 0,
      denied: 0,
    };
    assert!(d.admit(0, 100));
    assert!(d.admit(100, 50));
    assert!(!d.admit(150, 1), "the source's ceiling");
    assert_eq!(d.denials(), 1, "one pressure event");
    assert!(d.admit(100, 50), "within what was already granted");
    assert!(!d.admit(999, 2), "the max");
  }
}
