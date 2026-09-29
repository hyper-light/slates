//! Grants and the landing lease (§4.15, D-26) as in-process records: a grant is bound to what the human
//! approved — the manifest's hash, and the consumer, volume, snapshot and target identity it was presented
//! for ([`GrantBinding`], §4.13 "Grants": "the exact manifest hash, target identity, intended consumer,
//! scope and validity") — and consumed by one landing (or good for its session); the lease has one holder
//! per canonical target with a fencing generation. Phase 2 makes them database records through the control
//! shard; the shapes are the design's.
//!
//! Until 2026-09-29 a grant carried only its manifest, so a grant approved for one landing could land a
//! same-content plan into another directory, from another volume or for another consumer, and a session
//! grant covered any plan on its shard (AUD-29-01,
//! docs/bugs/2026-09-29-a-grant-did-not-bind-its-target-volume-or-consumer.md).

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

/// A landing target's identity: its canonical key and the target directory's identity on its device
/// (the host's `fingerprint_dir`) when the landing opened it, so a directory replaced at the same path is
/// not the target a grant names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetIdentity {
  /// The canonical target the lease names.
  pub key: Box<str>,
  /// The directory's device.
  pub device: u64,
  /// The directory's inode on that device.
  pub inode: u64,
}

/// What a grant binds besides its manifest (§4.13 "Grants"; §4.15's `GrantRecord`): who lands what,
/// where. A landing presents its own binding and a grant covers it only when they agree
/// ([`Grants::check`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantBinding {
  /// The intended consumer: the principal the landing was presented to, by its exact identity bytes (the
  /// server's principal key), never a number several principals could share.
  pub consumer: Box<[u8]>,
  /// The volume landed.
  pub volume: [u8; 16],
  /// The snapshot landed (a session grant covers later snapshots of the same volume).
  pub snapshot: u64,
  /// The target landed into.
  pub target: TargetIdentity,
}

/// The bound field a landing did not match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindingField {
  /// Another consumer than the grant's.
  Consumer,
  /// Another volume.
  Volume,
  /// Another snapshot, for a single-use grant.
  Snapshot,
  /// Another target: another path, or the same path naming another directory.
  Target,
}

impl BindingField {
  /// The field's name, for the refusal ledger.
  pub const fn name(self) -> &'static str {
    match self {
      BindingField::Consumer => "consumer",
      BindingField::Volume => "volume",
      BindingField::Snapshot => "snapshot",
      BindingField::Target => "target",
    }
  }
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
  /// Who lands what where, as the human approved it.
  pub binding: GrantBinding,
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
  /// The landing is not the one the grant binds: another consumer, volume, snapshot or target.
  Unbound {
    /// The first bound field that differed.
    field: BindingField,
  },
}

/// The grants table.
#[derive(Debug, Default)]
pub struct Grants {
  records: BTreeMap<GrantId, GrantRecord>,
  next: u64,
}

impl Grants {
  /// Issues a grant for a manifest from a human surface; `term_ns` is the grant term derived
  /// from the measured plan-to-grant interval (§4.15's derived constants; the caller's). `None` once the
  /// grant id space is spent: an id is never reused, so a record is never overwritten by a newer grant.
  pub fn issue(
    &mut self,
    surface: Surface,
    manifest: [u8; 32],
    binding: GrantBinding,
    scope: GrantScope,
    now_ns: u64,
    term_ns: u64,
  ) -> Option<GrantId> {
    self.next = self.next.checked_add(1)?;
    let id = GrantId(self.next);
    self.records.insert(
      id,
      GrantRecord {
        id,
        surface,
        manifest,
        binding,
        scope,
        issued_ns: now_ns,
        expires_ns: now_ns.saturating_add(term_ns),
        state: GrantState::Issued,
      },
    );
    Some(id)
  }

  /// Revokes a grant.
  pub fn revoke(&mut self, id: GrantId) {
    if let Some(g) = self.records.get_mut(&id) {
      g.state = GrantState::Revoked;
    }
  }

  /// Checks that `id` covers the landing `binding` describes, writing `manifest`, now.
  pub fn check(
    &mut self,
    id: Option<GrantId>,
    binding: &GrantBinding,
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
    // Who lands what where must be what the human approved, whatever the manifest: a create-only plan has
    // the same manifest in every empty directory and from every volume with the same content.
    if let Some(field) = unbound_field(&g.binding, binding, g.scope) {
      return Err(GrantRefusal::Unbound { field });
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

/// The first field in which a landing's binding differs from the one its grant was approved for: the
/// consumer, the volume and the target always; the snapshot only for a single-use grant, since a session
/// grant covers the later landings (later snapshots) of its volume into its target (§4.15 step 3).
fn unbound_field(
  granted: &GrantBinding,
  landing: &GrantBinding,
  scope: GrantScope,
) -> Option<BindingField> {
  if granted.consumer != landing.consumer {
    Some(BindingField::Consumer)
  } else if granted.volume != landing.volume {
    Some(BindingField::Volume)
  } else if granted.target != landing.target {
    Some(BindingField::Target)
  } else if scope == GrantScope::Once && granted.snapshot != landing.snapshot {
    Some(BindingField::Snapshot)
  } else {
    None
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
    // A generation advances once per lease taken; it saturates rather than wrapping, since 2^64 takes (one
    // per landing) are unreachable, and a saturated generation stays the largest, never an older one.
    self.generation = self.generation.saturating_add(1);
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

  /// A binding for the unit tests: one consumer, volume, snapshot and target.
  fn binding() -> GrantBinding {
    GrantBinding {
      consumer: b"consumer".as_slice().into(),
      volume: [1; 16],
      snapshot: 1,
      target: TargetIdentity {
        key: "/t".into(),
        device: 1,
        inode: 2,
      },
    }
  }

  #[test]
  fn a_grant_binds_one_manifest() {
    let mut grants = Grants::default();
    let bound = binding();
    let id = grants
      .issue(
        Surface::Cli,
        [1; 32],
        bound.clone(),
        GrantScope::Once,
        100,
        50,
      )
      .expect("a grant id");
    assert_eq!(
      grants.check(None, &bound, [1; 32], 110),
      Err(GrantRefusal::GrantRequired)
    );
    assert!(matches!(
      grants.check(Some(id), &bound, [2; 32], 110),
      Err(GrantRefusal::GrantMismatch { .. })
    ));
    assert!(grants.check(Some(id), &bound, [1; 32], 110).is_ok());
    assert_eq!(
      grants.check(Some(id), &bound, [1; 32], 200),
      Err(GrantRefusal::GrantExpired)
    );
    let id2 = grants
      .issue(
        Surface::Confirmation,
        [3; 32],
        bound.clone(),
        GrantScope::Once,
        100,
        50,
      )
      .expect("a grant id");
    grants.consume(id2);
    assert_eq!(
      grants.check(Some(id2), &bound, [3; 32], 110),
      Err(GrantRefusal::GrantExpired)
    );
    let id3 = grants
      .issue(
        Surface::Cli,
        [4; 32],
        bound.clone(),
        GrantScope::Session,
        100,
        50,
      )
      .expect("a grant id");
    grants.consume(id3);
    assert!(
      grants.check(Some(id3), &bound, [4; 32], 110).is_ok(),
      "a session grant survives a landing"
    );
    grants.revoke(id3);
    assert_eq!(
      grants.check(Some(id3), &bound, [4; 32], 110),
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
