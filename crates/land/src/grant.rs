//! Grants and the landing lease (§4.15, D-26) as in-process records: a grant is bound to a
//! manifest's hash and consumed by one landing (or good for a session); the lease has one
//! holder per canonical target with a fencing generation. Phase 2 makes them database records
//! through the control shard; the shapes are the design's.

use std::collections::BTreeMap;

/// A grant's id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GrantId(pub u64);

/// Who issued the grant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
  /// The command line.
  Cli,
  /// A confirmation surface of a harness.
  Confirmation,
}

/// What a grant covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantScope {
  /// One landing of this manifest.
  Once,
  /// Every landing of the same volume into the same target for the session.
  Session,
}

/// The grant's state machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantState {
  /// Usable.
  Issued,
  /// Used by its landing.
  Consumed,
  /// Past its term or its session.
  Expired,
  /// Revoked by the human.
  Revoked,
}

/// A grant record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantRecord {
  /// The id.
  pub id: GrantId,
  /// The surface.
  pub surface: Surface,
  /// The manifest hash it binds.
  pub manifest: [u8; 32],
  /// The scope.
  pub scope: GrantScope,
  /// Issued at, monotonic ns.
  pub issued_ns: u64,
  /// Expires at, monotonic ns.
  pub expires_ns: u64,
  /// The state.
  pub state: GrantState,
}

/// Why a grant does not cover a landing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GrantRefusal {
  /// No grant named.
  GrantRequired,
  /// The grant binds another manifest.
  GrantMismatch {
    /// The manifest the grant binds.
    expected: [u8; 32],
    /// The manifest about to be written.
    got: [u8; 32],
  },
  /// Past its term or consumed.
  GrantExpired,
  /// Revoked.
  GrantRefused,
}

/// The grants table.
#[derive(Debug, Default)]
pub struct Grants {
  records: BTreeMap<GrantId, GrantRecord>,
  next: u64,
}

impl Grants {
  /// Issues a grant for a manifest from a human surface; `term_ns` is the grant term derived
  /// from the measured plan-to-grant interval (§4.15's derived constants; the caller's).
  pub fn issue(
    &mut self,
    surface: Surface,
    manifest: [u8; 32],
    scope: GrantScope,
    now_ns: u64,
    term_ns: u64,
  ) -> GrantId {
    self.next += 1;
    let id = GrantId(self.next);
    self.records.insert(
      id,
      GrantRecord {
        id,
        surface,
        manifest,
        scope,
        issued_ns: now_ns,
        expires_ns: now_ns.saturating_add(term_ns),
        state: GrantState::Issued,
      },
    );
    id
  }

  /// Revokes a grant.
  pub fn revoke(&mut self, id: GrantId) {
    if let Some(g) = self.records.get_mut(&id) {
      g.state = GrantState::Revoked;
    }
  }

  /// Checks that `id` covers `manifest` now.
  pub fn check(
    &mut self,
    id: Option<GrantId>,
    manifest: [u8; 32],
    now_ns: u64,
  ) -> Result<GrantRecord, GrantRefusal> {
    let id = id.ok_or(GrantRefusal::GrantRequired)?;
    let g = self
      .records
      .get_mut(&id)
      .ok_or(GrantRefusal::GrantRequired)?;
    if g.state == GrantState::Issued && now_ns > g.expires_ns {
      g.state = GrantState::Expired;
    }
    match g.state {
      GrantState::Revoked => return Err(GrantRefusal::GrantRefused),
      GrantState::Expired | GrantState::Consumed => return Err(GrantRefusal::GrantExpired),
      GrantState::Issued => {}
    }
    // A single-use grant binds exactly the manifest the human saw; a session grant covers the
    // later landings of the same volume into the same target (§4.15 step 3), each of which
    // still presents its manifest and still refuses on a conflict.
    if g.scope == GrantScope::Once && g.manifest != manifest {
      return Err(GrantRefusal::GrantMismatch {
        expected: g.manifest,
        got: manifest,
      });
    }
    Ok(g.clone())
  }

  /// Consumes a single-use grant after its landing finished.
  pub fn consume(&mut self, id: GrantId) {
    if let Some(g) = self.records.get_mut(&id)
      && g.scope == GrantScope::Once
      && g.state == GrantState::Issued
    {
      g.state = GrantState::Consumed;
    }
  }

  /// A grant record.
  pub fn get(&self, id: GrantId) -> Option<&GrantRecord> {
    self.records.get(&id)
  }
}

/// The landing lease on a canonical target: one holder, a fencing generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandingLease {
  /// The canonical target.
  pub target: Box<str>,
  /// The holder.
  pub holder: u64,
  /// The fencing generation.
  pub generation: u64,
  /// Expires at, monotonic ns.
  pub expires_ns: u64,
}

/// Why a lease was not taken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandingLeaseHeld {
  /// The holder.
  pub holder: u64,
  /// Its generation.
  pub generation: u64,
}

/// The leases table.
#[derive(Debug, Default)]
pub struct Leases {
  held: BTreeMap<Box<str>, LandingLease>,
  generation: u64,
}

impl Leases {
  /// Takes the lease on `target` for `holder`, for `term_ns` (the derived lease term).
  pub fn take(
    &mut self,
    target: &str,
    holder: u64,
    now_ns: u64,
    term_ns: u64,
  ) -> Result<LandingLease, LandingLeaseHeld> {
    if let Some(l) = self.held.get(target)
      && l.holder != holder
      && l.expires_ns > now_ns
    {
      return Err(LandingLeaseHeld {
        holder: l.holder,
        generation: l.generation,
      });
    }
    self.generation += 1;
    let lease = LandingLease {
      target: target.into(),
      holder,
      generation: self.generation,
      expires_ns: now_ns.saturating_add(term_ns),
    };
    self.held.insert(target.into(), lease.clone());
    Ok(lease)
  }

  /// Releases the lease if `lease` still holds it.
  pub fn release(&mut self, lease: &LandingLease) {
    if self
      .held
      .get(&lease.target)
      .is_some_and(|l| l.generation == lease.generation)
    {
      self.held.remove(&lease.target);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_grant_binds_one_manifest() {
    let mut grants = Grants::default();
    let id = grants.issue(Surface::Cli, [1; 32], GrantScope::Once, 100, 50);
    assert_eq!(
      grants.check(None, [1; 32], 110),
      Err(GrantRefusal::GrantRequired)
    );
    assert!(matches!(
      grants.check(Some(id), [2; 32], 110),
      Err(GrantRefusal::GrantMismatch { .. })
    ));
    assert!(grants.check(Some(id), [1; 32], 110).is_ok());
    assert_eq!(
      grants.check(Some(id), [1; 32], 200),
      Err(GrantRefusal::GrantExpired)
    );
    let id2 = grants.issue(Surface::Confirmation, [3; 32], GrantScope::Once, 100, 50);
    grants.consume(id2);
    assert_eq!(
      grants.check(Some(id2), [3; 32], 110),
      Err(GrantRefusal::GrantExpired)
    );
    let id3 = grants.issue(Surface::Cli, [4; 32], GrantScope::Session, 100, 50);
    grants.consume(id3);
    assert!(
      grants.check(Some(id3), [4; 32], 110).is_ok(),
      "a session grant survives a landing"
    );
    grants.revoke(id3);
    assert_eq!(
      grants.check(Some(id3), [4; 32], 110),
      Err(GrantRefusal::GrantRefused)
    );
  }

  #[test]
  fn a_lease_has_one_holder() {
    let mut leases = Leases::default();
    let a = leases.take("/t", 1, 0, 100).unwrap();
    assert!(matches!(
      leases.take("/t", 2, 10, 100),
      Err(LandingLeaseHeld { holder: 1, .. })
    ));
    assert!(leases.take("/t", 1, 10, 100).is_ok(), "the holder renews");
    assert!(
      leases.take("/t", 2, 500, 100).is_ok(),
      "expired: another holder takes it"
    );
    leases.release(&a);
  }
}
