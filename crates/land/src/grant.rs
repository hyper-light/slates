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

use std::collections::{BTreeMap, BTreeSet};

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
  /// The volume's write policy (A-285): issued by the daemon itself under a bypass policy.
  Policy,
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

/// The key a target's landing lease is held under (§4.15 step 4; AUD-29-03): its canonical identity, the
/// opened directory's device and inode, so every path that names the same directory — an alias through
/// `..`, a second mount of it, a relative spelling — holds one lease, and a directory replaced at the same
/// path is another target.
pub fn lease_key(target: &TargetIdentity) -> String {
  format!("{:016x}:{:016x}", target.device, target.inode)
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

/// The grants table, its issued grants indexed by the end of their term (A-554), so the grants past their term and the
/// next one due are read from the index alone, never a walk of the table.
#[derive(Debug, Default)]
pub struct Grants {
  records: BTreeMap<GrantId, GrantRecord>,
  /// The issued grants by the end of their term, each `(expires_ns, id)` (A-554): one entry an issued grant, entered at
  /// its issue or restore and taken out when it leaves the table or leaves `Issued` (consumed, revoked), so it holds
  /// exactly the grants a sweep may end.
  by_term: BTreeSet<(u64, GrantId)>,
  /// The earliest instant a grant entered in [`Grants::by_term`] falls due, since the caller last asked
  /// ([`Grants::take_noted`]): what tells the shard's deadline keeper a grant was entered to lapse before the deadline
  /// it sleeps for (A-554), whichever path entered it.
  noted: Option<u64>,
  next: u64,
}

/// The first instant a grant whose term ends at `expires_ns` is past it: a grant is usable through its term's last
/// nanosecond and refused after it ([`Grants::check`]).
fn due_after(expires_ns: u64) -> u64 {
  expires_ns.saturating_add(1)
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
    let record = GrantRecord {
      id,
      surface,
      manifest,
      binding,
      scope,
      issued_ns: now_ns,
      expires_ns: now_ns.saturating_add(term_ns),
      state: GrantState::Issued,
    };
    self.index(&record);
    self.records.insert(id, record);
    Some(id)
  }

  /// Restores a grant a durable record holds, under its own id and in its recorded state (a restarted
  /// shard's rebuild, AUD-29-06); the next grant issued takes an id past every restored one, so an id is
  /// never issued twice across a restart. A record whose id is already held is ignored.
  pub fn restore(&mut self, record: GrantRecord) {
    self.next = self.next.max(record.id.0);
    if self.records.contains_key(&record.id) {
      return;
    }
    self.index(&record);
    self.records.insert(record.id, record);
  }

  /// Enters `record`'s term in [`Grants::by_term`] when it is issued, and notes when it falls due (A-554).
  fn index(&mut self, record: &GrantRecord) {
    if record.state != GrantState::Issued {
      return;
    }
    self.by_term.insert((record.expires_ns, record.id));
    let due = due_after(record.expires_ns);
    self.noted = Some(self.noted.map_or(due, |noted| noted.min(due)));
  }

  /// Restores the highest grant id the durable records ever issued (A-266): the table holds live grants only, so an
  /// ended grant's id is known by this mark alone, and the next grant takes an id past it.
  pub fn restore_issued_through(&mut self, id: u64) {
    self.next = self.next.max(id);
  }

  /// Ends `id`'s grant: it leaves the table at its terminal step (A-266; consumed, expired or revoked), its history
  /// kept by the audit log. An id it held stays refused as ended ([`Grants::check`]).
  pub fn end(&mut self, id: GrantId) {
    if let Some(record) = self.records.remove(&id) {
      self.by_term.remove(&(record.expires_ns, id));
    }
  }

  /// Keeps only the grants `held` says the durable table holds (A-266; AC-2.3): after an atom that issued a grant here
  /// was rolled back, the grant leaves this table too, so nothing lands under a grant whose issue was refused. An id it
  /// held stays refused as ended, as one [`Grants::end`] removed is.
  pub fn retain(&mut self, mut held: impl FnMut(GrantId) -> bool) {
    let by_term = &mut self.by_term;
    self.records.retain(|id, record| {
      let keep = held(*id);
      if !keep {
        by_term.remove(&(record.expires_ns, *id));
      }
      keep
    });
  }

  /// The grants past their term at `now_ns` that are still usable, for the sweep that ends them, in the order their
  /// terms ended: read from the index of terms alone (A-554).
  pub fn expired_at(&self, now_ns: u64) -> Vec<GrantId> {
    self
      .by_term
      .iter()
      .take_while(|(expires_ns, _)| now_ns > *expires_ns)
      .map(|(_, id)| *id)
      .collect()
  }

  /// The first instant a grant of this table is past its term (A-554): the earliest issued grant's term end and a
  /// nanosecond, the deadline the shard's keeper sleeps until for grants. `None` while no issued grant is held.
  pub fn next_due_ns(&self) -> Option<u64> {
    self
      .by_term
      .first()
      .map(|(expires_ns, _)| due_after(*expires_ns))
  }

  /// The earliest instant a grant entered since the last call falls due, taken (A-554): the shard's keeper wakes for it
  /// when it sleeps for a later deadline.
  pub fn take_noted(&mut self) -> Option<u64> {
    self.noted.take()
  }

  /// Revokes a grant: it leaves the index of terms, since no sweep ends a revoked grant.
  pub fn revoke(&mut self, id: GrantId) {
    if let Some(g) = self.records.get_mut(&id) {
      g.state = GrantState::Revoked;
      self.by_term.remove(&(g.expires_ns, id));
    }
  }

  /// Checks that `id` covers the landing `binding` describes, writing `manifest`, now. A check only reads: a grant past
  /// its term is refused as expired and left as it was, so the sweep ([`Grants::expired_at`]) still finds it and its
  /// expiry is recorded and audited before it leaves the table (A-266). Until 2026-11-18 the check marked such a grant
  /// expired in this table alone, which the sweep then passed over: it was never recorded, stayed listed as issued and
  /// stayed in this table until a restart.
  pub fn check(
    &self,
    id: Option<GrantId>,
    binding: &GrantBinding,
    manifest: [u8; 32],
    now_ns: u64,
  ) -> Result<GrantRecord, GrantRefusal> {
    let id = id.ok_or(GrantRefusal::GrantRequired)?;
    // An id this table issued that is no longer held has ended (A-266): refused as ended, never taken for a grant the
    // caller still needs (which would present the landing again).
    let issued_here = id.0 >= 1 && id.0 <= self.next;
    let g = self.records.get(&id).ok_or(if issued_here {
      GrantRefusal::GrantExpired
    } else {
      GrantRefusal::GrantRequired
    })?;
    match g.state {
      GrantState::Revoked => return Err(GrantRefusal::GrantRefused),
      GrantState::Expired | GrantState::Consumed => return Err(GrantRefusal::GrantExpired),
      GrantState::Issued if now_ns > g.expires_ns => return Err(GrantRefusal::GrantExpired),
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

  /// Consumes a single-use grant after its landing finished: it leaves the index of terms, since no sweep ends a
  /// consumed grant.
  pub fn consume(&mut self, id: GrantId) {
    if let Some(g) = self.records.get_mut(&id)
      && g.scope == GrantScope::Once
      && g.state == GrantState::Issued
    {
      g.state = GrantState::Consumed;
      self.by_term.remove(&(g.expires_ns, id));
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

  /// A-266: do issue a grant with a term of 50 at 100, check a landing against it at 200 (past its term), then ask the
  /// sweep for the grants due at 200; expect the check refused as expired and the grant among those due, so its
  /// expiry is recorded and audited. Before 2026-11-18 the check marked the grant expired in the table alone and the
  /// sweep, which takes issued grants past their term, never returned it: it was never recorded.
  #[test]
  fn a_grant_a_landing_found_past_its_term_is_still_due_for_the_sweep() {
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
      grants.check(Some(id), &bound, [1; 32], 200),
      Err(GrantRefusal::GrantExpired)
    );
    assert_eq!(
      grants.expired_at(200),
      vec![id],
      "the grant the check refused is still due for the sweep"
    );
    grants.end(id);
    assert_eq!(
      grants.check(Some(id), &bound, [1; 32], 200),
      Err(GrantRefusal::GrantExpired),
      "an ended grant stays refused as ended"
    );
    assert!(grants.expired_at(200).is_empty(), "and is due no more");
  }

  /// AUD-29-06: a restored grant keeps its id and state, the next grant issued takes an id past it, and a
  /// record whose id is already held does not replace it.
  #[test]
  fn restored_grants_keep_their_state_and_new_ids_pass_them() {
    let mut grants = Grants::default();
    let bound = binding();
    let restored = GrantRecord {
      id: GrantId(5),
      surface: Surface::Cli,
      manifest: [9; 32],
      binding: bound.clone(),
      scope: GrantScope::Once,
      issued_ns: 100,
      expires_ns: 150,
      state: GrantState::Consumed,
    };
    grants.restore(restored.clone());
    assert_eq!(
      grants.check(Some(GrantId(5)), &bound, [9; 32], 110),
      Err(GrantRefusal::GrantExpired),
      "a spent grant stays spent"
    );
    grants.restore(GrantRecord {
      state: GrantState::Issued,
      ..restored
    });
    assert_eq!(
      grants.get(GrantId(5)).map(|g| g.state),
      Some(GrantState::Consumed)
    );
    let next = grants
      .issue(Surface::Cli, [1; 32], bound, GrantScope::Once, 100, 50)
      .expect("a grant id");
    assert_eq!(
      next,
      GrantId(6),
      "a new grant's id passes every restored one"
    );
  }

  /// The issued grants of `grants` past their term at `now`, by a walk of the table, in the order their terms ended:
  /// the rule the index of terms must answer by (A-266's sweep, which walked the table until A-554).
  fn walked_due(grants: &Grants, now: u64) -> Vec<GrantId> {
    let mut due: Vec<(u64, GrantId)> = grants
      .records
      .values()
      .filter(|g| g.state == GrantState::Issued && now > g.expires_ns)
      .map(|g| (g.expires_ns, g.id))
      .collect();
    due.sort_unstable();
    due.into_iter().map(|(_, id)| id).collect()
  }

  /// The first instant an issued grant of `grants` is past its term, by a walk of the table.
  fn walked_next(grants: &Grants) -> Option<u64> {
    grants
      .records
      .values()
      .filter(|g| g.state == GrantState::Issued)
      .map(|g| g.expires_ns.saturating_add(1))
      .min()
  }

  /// Shape: the instants every step of the history is asked about: before, at and after each term it sets, and the
  /// clock's two ends.
  const INSTANTS: [u64; 16] = [
    0,
    119,
    120,
    121,
    130,
    131,
    140,
    150,
    151,
    190,
    191,
    200,
    201,
    1_000,
    1_001,
    u64::MAX,
  ];

  /// Asks `grants` for the grants due and the next due instant at each of [`INSTANTS`], and for what it noted since the
  /// last ask: each answer must be a walk's of the table, and the noted instant `noted`.
  fn agrees_with_a_walk(grants: &mut Grants, noted: Option<u64>, step: &str) {
    for now in INSTANTS {
      assert_eq!(
        grants.expired_at(now),
        walked_due(grants, now),
        "{step}: due at {now}"
      );
    }
    assert_eq!(
      grants.next_due_ns(),
      walked_next(grants),
      "{step}: next due"
    );
    assert_eq!(grants.take_noted(), noted, "{step}: noted");
  }

  /// A grant issued from the command line binding [`binding`], `term` from `now`.
  fn issued(grants: &mut Grants, scope: GrantScope, now: u64, term: u64) -> GrantId {
    grants
      .issue(Surface::Cli, [1; 32], binding(), scope, now, term)
      .expect("a grant id")
  }

  /// A-554 (the index of terms). Do: run one history over a grants table: issue grants of several terms and scopes (two
  /// ending at one instant), consume one, revoke one and consume it, end one, restore an issued grant, restore it again
  /// under another term, restore a consumed one, keep all but one, issue one with no term and one whose term reaches
  /// the clock's end, then end every grant due. After each step ask for the grants due and the next due instant around
  /// every term's end, and for what was noted. Expect: every answer a walk's of the table, in the order the terms ended
  /// (ties by id), and the noted instant the earliest due among the grants entered since the last ask, none after a
  /// step that entered none.
  #[test]
  fn the_index_of_terms_answers_as_a_walk_of_the_table() {
    let mut grants = Grants::default();
    let a = issued(&mut grants, GrantScope::Once, 100, 50);
    agrees_with_a_walk(&mut grants, Some(151), "issue a, ending at 150");
    let b = issued(&mut grants, GrantScope::Once, 100, 20);
    let c = issued(&mut grants, GrantScope::Session, 110, 80);
    let d = issued(&mut grants, GrantScope::Once, 100, 20);
    agrees_with_a_walk(
      &mut grants,
      Some(121),
      "issue b and d, ending at 120, and c at 190",
    );
    grants.consume(b);
    agrees_with_a_walk(&mut grants, None, "consume b");
    grants.revoke(c);
    grants.consume(c);
    agrees_with_a_walk(&mut grants, None, "revoke c, then consume it");
    grants.end(a);
    agrees_with_a_walk(&mut grants, None, "end a");
    let restored = GrantRecord {
      id: GrantId(9),
      surface: Surface::Cli,
      manifest: [9; 32],
      binding: binding(),
      scope: GrantScope::Once,
      issued_ns: 100,
      expires_ns: 130,
      state: GrantState::Issued,
    };
    grants.restore(restored.clone());
    agrees_with_a_walk(&mut grants, Some(131), "restore an issued grant");
    grants.restore(GrantRecord {
      expires_ns: 1_000,
      ..restored.clone()
    });
    agrees_with_a_walk(&mut grants, None, "restore it again under another term");
    grants.restore(GrantRecord {
      id: GrantId(10),
      expires_ns: 140,
      state: GrantState::Consumed,
      ..restored
    });
    agrees_with_a_walk(&mut grants, None, "restore a consumed grant");
    grants.retain(|id| id != d);
    agrees_with_a_walk(&mut grants, None, "keep all but d");
    issued(&mut grants, GrantScope::Once, 200, 0);
    issued(&mut grants, GrantScope::Once, 1, u64::MAX);
    agrees_with_a_walk(
      &mut grants,
      Some(201),
      "issue one with no term and one to the clock's end",
    );
    for id in grants.expired_at(u64::MAX) {
      grants.end(id);
    }
    agrees_with_a_walk(&mut grants, None, "end every grant due");
    assert_eq!(
      grants.next_due_ns(),
      Some(u64::MAX),
      "the grant whose term reaches the clock's end is the one left"
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
