//! The version-control credentials a partition holds (§4.13 "Principals", §4.17.2 "Access"; A-625 part 3).
//!
//! **What.** An SSH user key or an HTTP token a human enrolled for a principal through the CLI's control verbs (never
//! MCP or an SDK, §4.13), each named by the SHA-256 of what a client presents: a key's blob (OpenSSH's fingerprint) or a
//! token's bytes. The record lives on the partition its id routes to (`slates_server::credentials::owner_of`), so the
//! HTTP edge's authenticator and the SSH edge's key holders find it by one lookup there, with no global index (D-14). A
//! token is held as its hash and its CRAM-MD5 keyed contexts (A-692), never itself, as an enrolled consumer's
//! capability is checked and never handed back.
//!
//! **Guard then apply** (`crate::partition`). [`Credentials::check`] refuses an enrollment whose id is held
//! (`AlreadyExists`) and a revocation of an id not held (`NotFound`); the partition refuses an enrollment past its table
//! byte budget (`Capacity`, naming this table). [`Credentials::apply`] is unconditional, so replay is deterministic. A
//! record never changes once made: a new expiry is a new credential, the old one revoked.
//!
//! **Bounds.** Each record's bytes (the record, its key's blob and algorithm name, a SID principal's text) are charged to
//! the partition's table byte budget (`PartitionCaps::green_chain_bytes`: the table share of the shard's reserve, which
//! the merge chains and the review records draw from too), so the table is bounded by measured memory and refused typed
//! at it (banned item 8). An expired record stays until it is revoked: expiry is decided at authentication against the
//! wall clock ([`CredentialRecord::live_at`]), never in `apply`, where a decision from the clock would replay
//! differently.

use std::collections::BTreeMap;

use crate::catalog::{CredentialKind, CredentialRecord, Principal};
use crate::error::DbError;
use crate::op::Op;

/// The credentials a partition holds, by id, and the bytes they hold.
#[derive(Debug, Default)]
pub struct Credentials {
  /// The records, by id.
  records: BTreeMap<[u8; 32], CredentialRecord>,
  /// The bytes the records hold, charged to the partition's table byte budget.
  bytes: usize,
}

/// The bytes a credential charges (the module doc's bounds): its record (a token's contexts among its own bytes), its
/// key's blob and algorithm name, and a SID principal's text.
fn record_bytes(record: &CredentialRecord) -> usize {
  let kind = match &record.kind {
    CredentialKind::SshKey { algorithm, blob } => algorithm.len().saturating_add(blob.len()),
    CredentialKind::HttpToken { .. } => 0,
  };
  let principal = match &record.principal {
    Principal::Sid { sid, .. } => sid.len(),
    Principal::Uid { .. } | Principal::Certificate { .. } | Principal::Consumer { .. } => 0,
  };
  size_of::<CredentialRecord>()
    .saturating_add(kind)
    .saturating_add(principal)
}

/// The first half of a credential's id, as `DbError::AlreadyExists` names the record it met.
/// Shape: 16 bytes, half of the 32-byte id.
fn named(id: &[u8; 32]) -> [u8; 16] {
  id.first_chunk::<16>().copied().unwrap_or_default()
}

impl Credentials {
  /// No credentials.
  pub fn new() -> Credentials {
    Credentials::default()
  }

  /// The credential named `id`, if this partition holds it.
  pub fn get(&self, id: &[u8; 32]) -> Option<&CredentialRecord> {
    self.records.get(id)
  }

  /// Every credential, in id order, as a snapshot carries them.
  pub fn records(&self) -> impl Iterator<Item = &CredentialRecord> + '_ {
    self.records.values()
  }

  /// The bytes the records hold, charged to the partition's table byte budget.
  pub fn bytes(&self) -> usize {
    self.bytes
  }

  /// The bytes `op` releases and adds (the module doc's bounds): what the partition's budget must have room for before
  /// it is applied, and what the budget moves by when it is; nothing for another table's operation.
  pub fn charge(&self, op: &Op) -> (usize, usize) {
    match op {
      Op::CredentialEnrolled { record } => (0, record_bytes(record)),
      Op::CredentialRevoked { id } => (self.records.get(id).map_or(0, record_bytes), 0),
      _ => (0, 0),
    }
  }

  /// The guard (the module doc): an enrollment whose id is held is `AlreadyExists`, a revocation of an id not held is
  /// `NotFound`; another table's operation passes.
  pub fn check(&self, op: &Op) -> Result<(), DbError> {
    match op {
      Op::CredentialEnrolled { record } if self.records.contains_key(&record.id) => {
        Err(DbError::AlreadyExists {
          existing: named(&record.id),
        })
      }
      Op::CredentialRevoked { id } if !self.records.contains_key(id) => Err(DbError::NotFound),
      _ => Ok(()),
    }
  }

  /// Applies `op`, checked once before it was recorded: an enrollment holds its record, a revocation lets it go.
  /// `NotFound` for a revocation of an id not held, which only a log the guard never saw could carry.
  pub fn apply(&mut self, op: &Op) -> Result<(), DbError> {
    match op {
      Op::CredentialEnrolled { record } => {
        if let Some(replaced) = self.records.insert(record.id, record.clone()) {
          self.bytes = self.bytes.saturating_sub(record_bytes(&replaced));
        }
        self.bytes = self.bytes.saturating_add(record_bytes(record));
        Ok(())
      }
      Op::CredentialRevoked { id } => {
        let removed = self.records.remove(id).ok_or(DbError::NotFound)?;
        self.bytes = self.bytes.saturating_sub(record_bytes(&removed));
        Ok(())
      }
      _ => Ok(()),
    }
  }

  /// The table a snapshot carries, rebuilt with its bytes counted as when each record was enrolled.
  pub fn restore(records: &[CredentialRecord]) -> Result<Credentials, DbError> {
    let mut credentials = Credentials::new();
    for record in records {
      credentials.apply(&Op::CredentialEnrolled {
        record: record.clone(),
      })?;
    }
    Ok(credentials)
  }
}

#[cfg(test)]
mod tests {
  use std::collections::BTreeMap;

  use proptest::prelude::*;
  use proptest::test_runner::TestCaseError;

  use super::*;
  use crate::HostId;
  use crate::partition::{Partition, PartitionCaps};

  /// Shape: the records of the largest key the tests enroll that the table byte budget holds: small, so histories meet
  /// the bound.
  const BUDGET_RECORDS: usize = 5;
  /// Shape: the blob bytes of the tests' keys: an Ed25519 key's blob is 51 (RFC 8709 §4: the name's 4 + 11 bytes, the
  /// key's 4 + 32).
  const BLOB_BYTES: usize = 51;
  /// Shape: an instant on the wall clock, in nanoseconds since the Unix epoch (2026-11-19).
  const NOW_NS: u64 = 1_795_000_000_000_000_000;

  fn caps() -> PartitionCaps {
    PartitionCaps {
      volumes: 1 << 4,
      snapshots: 1 << 4,
      attachments: 1 << 4,
      segment_slots: 4,
      timers: 1 << 4,
      tick_ns: 1_000,
      green_chain_bytes: BUDGET_RECORDS
        * (size_of::<CredentialRecord>() + BLOB_BYTES + "ssh-ed25519".len()),
      audit_records: 1 << 4,
    }
  }

  fn person(uid: u32) -> Principal {
    Principal::Uid {
      host: HostId(7),
      uid,
    }
  }

  fn key(serial: u8, uid: u32) -> CredentialRecord {
    CredentialRecord {
      id: [serial; 32],
      principal: person(uid),
      kind: CredentialKind::SshKey {
        algorithm: "ssh-ed25519".to_owned(),
        blob: vec![serial; BLOB_BYTES],
      },
      expires_ns: None,
    }
  }

  fn token(serial: u8, uid: u32, expires_ns: Option<u64>) -> CredentialRecord {
    CredentialRecord {
      id: [serial; 32],
      principal: person(uid),
      kind: CredentialKind::HttpToken {
        cram_md5_contexts: [serial; 32],
      },
      expires_ns,
    }
  }

  fn run(partition: &mut Partition, op: &Op) -> Result<(), DbError> {
    partition.check(op, 0)?;
    partition.apply(op)
  }

  fn enrolled(record: CredentialRecord) -> Op {
    Op::CredentialEnrolled { record }
  }

  fn records(partition: &Partition) -> Vec<CredentialRecord> {
    partition.credentials().records().cloned().collect()
  }

  fn rebuilt(partition: &Partition) -> Partition {
    match Partition::from_snapshot(&partition.to_snapshot(0), caps(), 0) {
      Ok(rebuilt) => rebuilt,
      Err(refusal) => panic!("the snapshot rebuilds: {refusal:?}"),
    }
  }

  /// A-625 part 3. Do: enroll an SSH key and an HTTP token for two principals, look each up by its id, rebuild the
  /// partition from its snapshot, revoke the key, rebuild again. Expect: each id finds its own record (its principal,
  /// kind and expiry), an id never enrolled finds none, the rebuilt partition finds the same, and after the revocation
  /// the key is gone on both while the token stays.
  #[test]
  fn a_credential_is_found_by_its_id_until_revoked_and_a_snapshot_keeps_it() {
    let mut partition = Partition::new(caps(), 0);
    run(&mut partition, &enrolled(key(1, 501))).unwrap();
    run(&mut partition, &enrolled(token(2, 502, Some(NOW_NS)))).unwrap();
    let found =
      |partition: &Partition, serial: u8| partition.credentials().get(&[serial; 32]).cloned();
    assert_eq!(found(&partition, 1), Some(key(1, 501)));
    assert_eq!(found(&partition, 2), Some(token(2, 502, Some(NOW_NS))));
    assert_eq!(found(&partition, 3), None);
    let mut partition = rebuilt(&partition);
    assert_eq!(found(&partition, 1), Some(key(1, 501)));
    assert_eq!(found(&partition, 2), Some(token(2, 502, Some(NOW_NS))));
    run(&mut partition, &Op::CredentialRevoked { id: [1; 32] }).unwrap();
    assert_eq!(found(&partition, 1), None);
    let partition = rebuilt(&partition);
    assert_eq!(found(&partition, 1), None);
    assert_eq!(found(&partition, 2), Some(token(2, 502, Some(NOW_NS))));
  }

  /// A-625 part 3 (guard then apply). Do: enroll a key, then enroll another record under the same id, and revoke an id
  /// never enrolled. Expect: `AlreadyExists` naming the id's first half and `NotFound`, typed, with the first record
  /// unchanged and nothing else held.
  #[test]
  fn an_enrollment_of_a_held_id_and_a_revocation_of_an_unheld_one_are_refused() {
    let mut partition = Partition::new(caps(), 0);
    run(&mut partition, &enrolled(key(4, 501))).unwrap();
    assert_eq!(
      run(&mut partition, &enrolled(token(4, 502, None))),
      Err(DbError::AlreadyExists { existing: [4; 16] })
    );
    assert_eq!(
      run(&mut partition, &Op::CredentialRevoked { id: [5; 32] }),
      Err(DbError::NotFound)
    );
    assert_eq!(records(&partition), vec![key(4, 501)]);
  }

  /// A-625 part 3 (bounded growth, banned item 8). Do: enroll keys until the table byte budget refuses one, rebuild
  /// from the snapshot and try once more, revoke one key, enroll again. Expect: the key past the budget refused
  /// `Capacity` naming the credentials table with nothing recorded, after as many as the budget holds; the rebuilt
  /// partition charges what it holds and refuses at the same key; a revocation gives its bytes back, so one more fits.
  #[test]
  fn credentials_fit_the_table_byte_budget_and_a_revocation_gives_room_back() {
    let mut partition = Partition::new(caps(), 0);
    let mut enrolled_keys = 0u8;
    let refusal = loop {
      match run(&mut partition, &enrolled(key(enrolled_keys, 501))) {
        Ok(()) => enrolled_keys += 1,
        Err(refusal) => break refusal,
      }
    };
    assert_eq!(
      refusal,
      DbError::Capacity {
        table: "credentials"
      }
    );
    assert_eq!(usize::from(enrolled_keys), BUDGET_RECORDS);
    assert_eq!(records(&partition).len(), BUDGET_RECORDS);
    let mut partition = rebuilt(&partition);
    assert_eq!(
      run(&mut partition, &enrolled(key(enrolled_keys, 501))),
      Err(DbError::Capacity {
        table: "credentials"
      })
    );
    run(&mut partition, &Op::CredentialRevoked { id: [0; 32] }).unwrap();
    assert_eq!(
      run(&mut partition, &enrolled(key(enrolled_keys, 501))),
      Ok(())
    );
  }

  /// A-625 part 3: a credential proves its principal until its expiry and not at it; one with none always does.
  #[test]
  fn a_credential_lives_until_its_expiry_and_one_without_one_always() {
    let expiring = token(6, 501, Some(NOW_NS));
    assert!(expiring.live_at(NOW_NS - 1));
    assert!(!expiring.live_at(NOW_NS));
    assert!(!expiring.live_at(u64::MAX));
    let lasting = token(7, 501, None);
    assert!(lasting.live_at(0));
    assert!(lasting.live_at(u64::MAX));
  }

  /// A step of a generated history: an enrollment of a key or a token by serial, or a revocation by serial.
  #[derive(Clone, Copy, Debug)]
  enum Step {
    /// Enroll serial's key.
    Key(u8),
    /// Enroll serial's token.
    Token(u8),
    /// Revoke serial.
    Revoke(u8),
  }

  /// Format: the kinds of step a history draws, each as likely.
  const STEP_KINDS: u8 = 3;
  /// Shape: the serials a history names: few, so enrollments meet held ids and revocations meet unheld ones.
  const SERIALS: u8 = 8;

  fn step() -> impl Strategy<Value = Step> {
    (0..STEP_KINDS, 0..SERIALS).prop_map(|(kind, serial)| match kind {
      0 => Step::Key(serial),
      1 => Step::Token(serial),
      _ => Step::Revoke(serial),
    })
  }

  /// The design's rules over a plain map: an id is held once; a revocation needs it held; the bytes fit the budget.
  #[derive(Default)]
  struct Model {
    held: BTreeMap<[u8; 32], CredentialRecord>,
  }

  impl Model {
    fn step(&mut self, step: Step) -> Result<(), DbError> {
      let record = match step {
        Step::Key(serial) => key(serial, 501),
        Step::Token(serial) => token(serial, 502, Some(NOW_NS)),
        Step::Revoke(serial) => {
          return self
            .held
            .remove(&[serial; 32])
            .map(|_| ())
            .ok_or(DbError::NotFound);
        }
      };
      if self.held.contains_key(&record.id) {
        return Err(DbError::AlreadyExists {
          existing: named(&record.id),
        });
      }
      let used: usize = self.held.values().map(record_bytes).sum();
      if used + record_bytes(&record) > caps().green_chain_bytes {
        return Err(DbError::Capacity {
          table: "credentials",
        });
      }
      self.held.insert(record.id, record);
      Ok(())
    }
  }

  fn op_of(step: Step) -> Op {
    match step {
      Step::Key(serial) => enrolled(key(serial, 501)),
      Step::Token(serial) => enrolled(token(serial, 502, Some(NOW_NS))),
      Step::Revoke(serial) => Op::CredentialRevoked { id: [serial; 32] },
    }
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig { cases: slates_test_seeds::cases(400), ..ProptestConfig::default() }))]

    /// A-625 part 3 (guard then apply). Do: run a generated history of key and token enrollments and revocations over a
    /// few ids on a partition whose table byte budget holds a few records; after every step rebuild the partition from
    /// its snapshot and go on from the rebuilt one. Expect: every answer (accepted, or which refusal) and the whole
    /// table equal the rules kept over a plain map, and the rebuilt partition holds the same records.
    #[test]
    fn the_credentials_equal_the_rules_over_every_history(
      steps in proptest::collection::vec(step(), 0..80),
    ) {
      let mut partition = Partition::new(caps(), 0);
      let mut model = Model::default();
      for step in steps {
        prop_assert_eq!(run(&mut partition, &op_of(step)), model.step(step), "step {:?}", step);
        prop_assert_eq!(records(&partition), model.held.values().cloned().collect::<Vec<_>>());
        partition = Partition::from_snapshot(&partition.to_snapshot(0), caps(), 0)
          .map_err(|refusal| TestCaseError::fail(format!("the snapshot rebuilds: {refusal:?}")))?;
        prop_assert_eq!(records(&partition), model.held.values().cloned().collect::<Vec<_>>());
      }
    }
  }
}
