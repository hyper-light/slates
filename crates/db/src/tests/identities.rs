//! A principal's tool identities (A-664 part 1; §4.13 "a principal's record"; §4.17.1 "A push is a work" item 4): set
//! whole as the version after the one its setter read, refused typed from a stale read with nothing changed, one record
//! a principal, and carried by the partition's snapshot.

use super::{Partition, PartitionCaps};
use crate::DbError;
use crate::HostId;
use crate::catalog::{GitIdentity, IdentityRecord, Principal};
use crate::op::Op;

/// Shape: a partition with room for the little these tests make (the replay tests' own).
fn caps() -> PartitionCaps {
  PartitionCaps {
    volumes: 1 << 4,
    snapshots: 1 << 4,
    attachments: 1 << 4,
    segment_slots: 4,
    timers: 1 << 4,
    tick_ns: 1_000,
    green_chain_bytes: 1 << 20,
    audit_records: 1 << 4,
  }
}

/// An agent: a consumer enrolled under an account of host 1.
fn agent(consumer: u64) -> Principal {
  Principal::Consumer {
    host: HostId(1),
    account: 501,
    consumer,
  }
}

/// `principal`'s identities as version `version`: git's of `name`, and Mercurial's when `mercurial` is given.
fn record(principal: &Principal, (name, mercurial): (&str, Option<&str>), version: u64) -> Op {
  Op::IdentitiesSet {
    record: IdentityRecord {
      principal: principal.clone(),
      git: Some(GitIdentity {
        name: name.to_owned(),
        email: format!("{}@example.com", name.to_lowercase().replace(' ', ".")),
      }),
      mercurial: mercurial.map(str::to_owned),
      version,
    },
  }
}

/// `op` passes the partition's check and is applied.
fn set(partition: &mut Partition, op: &Op) {
  partition.check(op, 0).unwrap();
  partition.apply(op).unwrap();
}

/// What `principal`'s record holds: its git name, its Mercurial user and its version.
fn held(partition: &Partition, principal: &Principal) -> Option<(String, Option<String>, u64)> {
  partition.identities(principal).map(|record| {
    let name = record
      .git
      .as_ref()
      .map(|git| git.name.clone())
      .unwrap_or_default();
    (name, record.mercurial.clone(), record.version)
  })
}

/// A-664 part 1 (§4.13; A-622's version rule). Do: set an agent's identities as version 1, then set them whole again as
/// version 2 with a Mercurial user; then set them from a stale read (version 2 again), past the next (version 4), and
/// another agent's from a version not its first (2). Expect: version 1 kept, then replaced whole by version 2; each
/// later setting refused `PolicyChanged` naming the version held (2, 2, and 0 for the agent with none), nothing changed;
/// the other agent holds no record.
#[test]
fn t_a664_identities_are_set_whole_from_the_version_read_and_a_stale_setting_changes_nothing() {
  let mut partition = Partition::new(caps(), 0);
  let (first, second) = (agent(7), agent(8));
  set(&mut partition, &record(&first, ("Ada", None), 1));
  assert_eq!(held(&partition, &first), Some(("Ada".to_owned(), None, 1)));
  set(
    &mut partition,
    &record(&first, ("Ada L", Some("Ada L <ada@example.com>")), 2),
  );
  let now = Some((
    "Ada L".to_owned(),
    Some("Ada L <ada@example.com>".to_owned()),
    2,
  ));
  assert_eq!(held(&partition, &first), now);
  for (principal, version, current) in [(&first, 2, 2), (&first, 4, 2), (&second, 2, 0)] {
    let stale = record(principal, ("Eve", None), version);
    assert_eq!(
      partition.check(&stale, 0),
      Err(DbError::PolicyChanged { current })
    );
  }
  assert_eq!(
    (held(&partition, &first), held(&partition, &second)),
    (now, None)
  );
}

/// A-664 part 1 (§4.8: a partition is its snapshot and the log after it). Do: set two principals' identities, an agent's
/// and a host account's, the first twice; take the partition's snapshot and rebuild a partition from it. Expect: the
/// rebuilt partition holds each principal's record as it was, its version included, and the next setting of each is
/// the version after it.
#[test]
fn t_a664_identities_are_carried_by_the_partitions_snapshot() {
  let mut partition = Partition::new(caps(), 0);
  let account = Principal::Uid {
    host: HostId(1),
    uid: 501,
  };
  set(&mut partition, &record(&agent(7), ("Ada", None), 1));
  set(&mut partition, &record(&agent(7), ("Ada L", None), 2));
  set(
    &mut partition,
    &record(&account, ("Ada Lundhe", Some("Ada <a@b>")), 1),
  );
  let snapshot = partition.to_snapshot(3);
  let rebuilt = Partition::from_snapshot(&snapshot, caps(), 0).unwrap();
  for principal in [agent(7), account] {
    assert_eq!(held(&rebuilt, &principal), held(&partition, &principal));
  }
  assert_eq!(snapshot.identities.len(), 2);
  assert!(
    rebuilt
      .check(&record(&agent(7), ("Ada", None), 3), 0)
      .is_ok()
  );
}
