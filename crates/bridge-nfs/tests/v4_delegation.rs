//! NFSv4.1 read delegations at the file's owner (RFC 8881 §10.4; §4.6 A-78), driven through the owner's file state as
//! the v4 front end drives it: grants against the file's opens, recalls on a conflicting change, returns, revocation
//! after a lease, the quiet period after a recall, two mounts of one file, I/O under a delegation's state id, and the
//! durable records a restart rebuilds from.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_bridge_nfs::handle::FileHandle;
use slates_bridge_nfs::nfs::Nfsfh3;
use slates_bridge_nfs::v4::Nfsstat4;
use slates_bridge_nfs::v4::delegation::{Conflict, DelegationRecord};
use slates_bridge_nfs::v4::files::{
  FileChange, FileState, IoAuthority, IoWant, Share, owner_tag, share,
};
use slates_db::catalog::VolumeId;

/// Shape: the opens and lock ranges the test owner holds.
const BOUND: usize = 64;
/// Shape: the lease and the quiet period the tests run under, in nanoseconds.
const LEASE: u64 = 90_000_000_000;
/// Shape: see [`LEASE`].
const QUIET: u64 = LEASE;
/// Format: the two clients.
const A: u64 = 1;
/// Format: see [`A`].
const B: u64 = 2;

/// The handle of inode `inode` under mount `attachment`.
fn handle(inode: u64, attachment: u64) -> Nfsfh3 {
  FileHandle {
    volume: VolumeId { bytes: [7; 16] },
    inode,
    generation: 1,
    attachment,
    token: [u8::try_from(attachment).unwrap(); 16],
  }
  .to_fh()
}

fn read_only() -> Share {
  Share {
    access: share::READ,
    deny: 0,
  }
}

fn read_write() -> Share {
  Share {
    access: share::BOTH,
    deny: 0,
  }
}

fn owner() -> FileState {
  FileState::restore(
    owner_tag(0, 1),
    BOUND,
    BOUND,
    Vec::new(),
    Vec::new(),
    Vec::new(),
  )
}

/// §10.4: do open a file read-only from A and ask for a delegation; expect one granted, and asking again answered with
/// the same one. With B holding the file open for writing, expect none for a third client.
#[test]
fn a_read_only_open_is_delegated_unless_another_client_writes_the_file() {
  let mut files = owner();
  let fh = handle(10, 1);
  files.open(A, b"a".to_vec(), &fh, read_only()).unwrap();
  let delegation = files
    .delegate_read(A, &fh, 0, QUIET)
    .expect("A's read-only open is delegated");
  assert_eq!(
    files.delegate_read(A, &fh, 1, QUIET),
    Some(delegation),
    "the same delegation again"
  );
  files.open(B, b"b".to_vec(), &fh, read_write()).unwrap();
  assert_eq!(
    files.delegate_read(3, &fh, 2, QUIET),
    None,
    "no delegation while another client writes the file"
  );
}

/// §10.2, §10.4.4: do change a delegated file from another client; expect the operation to wait on A's delegation
/// with one recall sent, a second conflicting change to wait without a second recall, and nothing to wait on once A
/// returns it. A's own change waits on nothing.
#[test]
fn a_conflicting_change_recalls_once_and_waits_until_the_return() {
  let mut files = owner();
  let fh = handle(10, 1);
  files.open(A, b"a".to_vec(), &fh, read_only()).unwrap();
  let delegation = files.delegate_read(A, &fh, 0, QUIET).unwrap();
  assert!(
    !files.recall(&fh, Some(A), Conflict::Change, 1).waiting,
    "the holder's own change waits on nothing"
  );
  let first = files.recall(&fh, Some(B), Conflict::Change, 2);
  assert!(first.waiting);
  assert_eq!(first.send.len(), 1, "one recall");
  assert_eq!(first.send[0].stateid, delegation);
  assert_eq!(first.send[0].clientid, A);
  let again = files.recall(&fh, None, Conflict::Change, 3);
  assert!(
    again.waiting && again.send.is_empty(),
    "still waiting, no second recall"
  );
  files.return_delegation(&delegation, A, &fh).unwrap();
  assert!(
    !files.recall(&fh, Some(B), Conflict::Change, 5).waiting,
    "nothing to wait for once returned"
  );
  assert_eq!(
    files.return_delegation(&delegation, A, &fh),
    Err(Nfsstat4::BadStateid),
    "a delegation is returned once"
  );
}

/// §10.4: do recall A's delegation, then ask for a new one within the quiet period and after it; expect none within
/// it (the file is being changed by others) and one after it.
#[test]
fn a_recalled_file_is_not_delegated_again_within_the_quiet_period() {
  let mut files = owner();
  let fh = handle(10, 1);
  files.open(A, b"a".to_vec(), &fh, read_only()).unwrap();
  let delegation = files.delegate_read(A, &fh, 0, QUIET).unwrap();
  files.recall(&fh, Some(B), Conflict::Change, 10);
  files.return_delegation(&delegation, A, &fh).unwrap();
  assert_eq!(
    files.delegate_read(A, &fh, 10 + QUIET / 2, QUIET),
    None,
    "within the quiet period"
  );
  assert!(
    files.delegate_read(A, &fh, 10 + QUIET + 1, QUIET).is_some(),
    "after it"
  );
}

/// §10.4.5: do recall a delegation its holder never returns; expect it revoked once a lease has passed since the
/// recall, journaled as cleared, and not before.
#[test]
fn an_unreturned_delegation_is_revoked_after_a_lease() {
  let mut files = owner();
  let fh = handle(10, 1);
  files.open(A, b"a".to_vec(), &fh, read_only()).unwrap();
  let delegation = files.delegate_read(A, &fh, 0, QUIET).unwrap();
  files.take_changes();
  files.recall(&fh, Some(B), Conflict::Change, 100);
  assert!(
    files.revoke_lapsed(100 + LEASE, LEASE, QUIET).is_empty(),
    "not before a lease"
  );
  assert_eq!(
    files.revoke_lapsed(101 + LEASE, LEASE, QUIET),
    vec![(delegation.other, A)]
  );
  assert_eq!(
    files.take_changes(),
    vec![FileChange::DelegationCleared(delegation.other)]
  );
  assert!(
    !files
      .recall(&fh, Some(B), Conflict::Change, 102 + LEASE)
      .waiting,
    "nothing left to wait for"
  );
  assert!(
    files.has_revoked(A),
    "A is told on every SEQUENCE until it frees the delegation"
  );
  assert_eq!(files.test(&delegation.other, A), Nfsstat4::DelegRevoked);
  assert!(
    files.free(&delegation.other, A).is_ok(),
    "A frees what it has seen revoked"
  );
  assert!(!files.has_revoked(A));
  assert_eq!(files.test(&delegation.other, A), Nfsstat4::BadStateid);
}

/// A-36, A-78: do delegate a file opened through one mount, then change it through another mount's handle; expect the
/// recall found all the same (a delegation is the file's, as its locks are).
#[test]
fn a_change_through_another_mount_recalls_the_delegation() {
  let mut files = owner();
  files
    .open(A, b"a".to_vec(), &handle(10, 1), read_only())
    .unwrap();
  files.delegate_read(A, &handle(10, 1), 0, QUIET).unwrap();
  let plan = files.recall(&handle(10, 2), Some(B), Conflict::Change, 1);
  assert!(plan.waiting && plan.send.len() == 1);
}

/// §10.4: do read a file from B while A holds a read delegation of it; expect nothing to wait on (read delegations
/// do not conflict with reads), and no recall sent.
#[test]
fn a_read_by_another_client_does_not_recall_a_read_delegation() {
  let mut files = owner();
  let fh = handle(10, 1);
  files.open(A, b"a".to_vec(), &fh, read_only()).unwrap();
  files.delegate_read(A, &fh, 0, QUIET).unwrap();
  let plan = files.recall(&fh, Some(B), Conflict::Read, 1);
  assert!(!plan.waiting && plan.send.is_empty());
}

/// §10.4.1: do read under A's delegation state id; expect it authorized as an open would be. Expect a write under a
/// read delegation refused `NFS4ERR_OPENMODE`, and B presenting A's delegation refused `NFS4ERR_BAD_STATEID`.
#[test]
fn a_delegations_state_id_serves_its_holders_reads_only() {
  let mut files = owner();
  let fh = handle(10, 1);
  files.open(A, b"a".to_vec(), &fh, read_only()).unwrap();
  let delegation = files.delegate_read(A, &fh, 0, QUIET).unwrap();
  assert_eq!(
    files.check_io(&delegation, &fh, A, IoWant::Read),
    Ok(IoAuthority::Delegation)
  );
  assert_eq!(
    files.check_io(&delegation, &fh, A, IoWant::Write),
    Err(Nfsstat4::Openmode)
  );
  assert_eq!(
    files.check_io(&delegation, &fh, B, IoWant::Read),
    Err(Nfsstat4::BadStateid)
  );
  assert_eq!(files.test(&delegation.other, A), Nfsstat4::Ok);
}

/// A-37, A-78: do grant a delegation and rebuild the owner from the records its journal produced; expect the rebuilt
/// owner to hold it (its state id serves A's reads, and a conflicting change waits on it).
#[test]
fn a_delegation_survives_a_restart_through_its_record() {
  let mut files = owner();
  let fh = handle(10, 1);
  files.open(A, b"a".to_vec(), &fh, read_only()).unwrap();
  files.take_changes();
  let delegation = files.delegate_read(A, &fh, 0, QUIET).unwrap();
  let records: Vec<DelegationRecord> = files
    .take_changes()
    .into_iter()
    .filter_map(|change| match change {
      FileChange::DelegationSet(record) => Some(record),
      _ => None,
    })
    .collect();
  assert_eq!(records.len(), 1, "the grant is journaled");
  let mut rebuilt = FileState::restore(
    owner_tag(0, 2),
    BOUND,
    BOUND,
    Vec::new(),
    Vec::new(),
    records,
  );
  assert_eq!(
    rebuilt.check_io(&delegation, &fh, A, IoWant::Read),
    Ok(IoAuthority::Delegation)
  );
  assert!(rebuilt.recall(&fh, Some(B), Conflict::Change, 1).waiting);
}

/// §10.4, §10.4.1 (A-80): do open a file for writing from A and ask for a write delegation; expect one, and the same
/// one again. With B holding the file open, expect none for a third client; with A holding a write delegation, expect
/// no read delegation for B.
#[test]
fn an_open_for_writing_is_write_delegated_only_while_no_other_client_has_the_file() {
  let mut files = owner();
  let fh = handle(10, 1);
  files.open(A, b"a".to_vec(), &fh, read_write()).unwrap();
  let delegation = files
    .delegate_write(A, &fh, 0, QUIET)
    .expect("A's open for writing is delegated");
  assert_eq!(files.delegate_write(A, &fh, 1, QUIET), Some(delegation));
  assert_eq!(
    files.check_io(&delegation, &fh, A, IoWant::Write),
    Ok(IoAuthority::Delegation),
    "the holder writes under it, as the client, not as one checked open"
  );
  assert_eq!(
    files.check_io(&delegation, &fh, A, IoWant::Read),
    Ok(IoAuthority::Delegation),
    "and reads (§9.1.2)"
  );
  assert_eq!(files.delegate_read(B, &fh, QUIET + 1, QUIET), None);
  let other = handle(11, 1);
  files.open(B, b"b".to_vec(), &other, read_only()).unwrap();
  assert_eq!(
    files.delegate_write(3, &other, 2, QUIET),
    None,
    "another client has the file open"
  );
}

/// §10.4.4 (A-80): do open a file A holds a write delegation of, from B, for reading only; expect `NFS4ERR_DELAY`
/// with one recall queued for the daemon, a second attempt to wait without a second recall, and B's open to proceed
/// once A returns the delegation. A's own opens wait on nothing.
#[test]
fn any_open_by_another_client_recalls_a_write_delegation() {
  let mut files = owner();
  let fh = handle(10, 1);
  files.open(A, b"a".to_vec(), &fh, read_write()).unwrap();
  let delegation = files.delegate_write(A, &fh, 0, QUIET).unwrap();
  assert_eq!(files.check_open_conflicts(A, &fh, read_only(), 1), Ok(()));
  assert_eq!(
    files.check_open_conflicts(B, &fh, read_only(), 2),
    Err(Nfsstat4::Delay)
  );
  let recalls = files.take_recalls();
  assert_eq!(recalls.len(), 1);
  assert_eq!(recalls[0].stateid, delegation);
  assert_eq!(
    files.check_open_conflicts(B, &fh, read_only(), 3),
    Err(Nfsstat4::Delay)
  );
  assert!(files.take_recalls().is_empty(), "recalled once");
  files.return_delegation(&delegation, A, &fh).unwrap();
  assert_eq!(files.check_open_conflicts(B, &fh, read_only(), 4), Ok(()));
}

/// §10.4.4 (A-80): do open a file A holds a read delegation of, from B: for reading, expect it to proceed; for
/// writing, or reading while denying reads, expect `NFS4ERR_DELAY` and A's delegation recalled.
#[test]
fn an_open_that_writes_or_denies_reads_recalls_a_read_delegation() {
  let mut files = owner();
  let fh = handle(10, 1);
  files.open(A, b"a".to_vec(), &fh, read_only()).unwrap();
  files.delegate_read(A, &fh, 0, QUIET).unwrap();
  assert_eq!(files.check_open_conflicts(B, &fh, read_only(), 1), Ok(()));
  let denying = Share {
    access: share::READ,
    deny: share::READ,
  };
  assert_eq!(
    files.check_open_conflicts(B, &fh, denying, 2),
    Err(Nfsstat4::Delay)
  );
  assert_eq!(files.take_recalls().len(), 1);
  assert_eq!(
    files.check_open_conflicts(B, &fh, read_write(), 3),
    Err(Nfsstat4::Delay),
    "still recalled"
  );
}

/// §10.4.3 (A-80): do read a file's attributes from B while A holds a write delegation of it; expect a wait and one
/// recall. A's own reads wait on nothing, nor do B's of a read-delegated file.
#[test]
fn another_clients_getattr_recalls_a_write_delegation() {
  let mut files = owner();
  let fh = handle(10, 1);
  files.open(A, b"a".to_vec(), &fh, read_write()).unwrap();
  files.delegate_write(A, &fh, 0, QUIET).unwrap();
  assert!(!files.check_read_conflicts(&fh, A, 1));
  assert!(files.check_read_conflicts(&fh, B, 2));
  assert_eq!(files.take_recalls().len(), 1);
  let read = handle(12, 1);
  files.open(A, b"a".to_vec(), &read, read_only()).unwrap();
  files.delegate_read(A, &read, 0, QUIET).unwrap();
  assert!(!files.check_read_conflicts(&read, B, 3));
}

/// A-80: do open a file A holds a write delegation of, from A, read-only; expect no read delegation (A's write one
/// already covers its opens), never A's write delegation handed back as a read one.
#[test]
fn a_write_delegations_holder_is_not_given_it_again_as_a_read_one() {
  let mut files = owner();
  let fh = handle(10, 1);
  files.open(A, b"a".to_vec(), &fh, read_write()).unwrap();
  files.delegate_write(A, &fh, 0, QUIET).unwrap();
  files.open(A, b"a2".to_vec(), &fh, read_only()).unwrap();
  assert_eq!(files.delegate_read(A, &fh, QUIET + 1, QUIET), None);
}

/// Applies the inodes whose holders changed to `gate`, as the daemon updates its recall gate (A-80).
fn follow(
  files: &mut FileState,
  gate: &mut std::collections::BTreeMap<u64, std::collections::BTreeSet<u64>>,
) {
  for inode in files.take_changed_inodes() {
    let holders = files.holders_of_inode(inode);
    if holders.is_empty() {
      gate.remove(&inode);
    } else {
      gate.insert(inode, holders);
    }
  }
}

/// A-80: do grant, return, recall, revoke and purge delegations across several files and clients, updating a gate
/// only from the inodes whose holders changed after each step; expect it equal to the holders rebuilt whole from the
/// table every time.
#[test]
fn the_gate_updated_by_changed_inodes_equals_a_whole_rebuild() {
  let mut files = owner();
  let mut gate = std::collections::BTreeMap::new();
  let fhs: Vec<Nfsfh3> = (10..14).map(|inode| handle(inode, 1)).collect();
  let mut check = |files: &mut FileState, step: &str| {
    follow(files, &mut gate);
    assert_eq!(gate, files.delegated_holders(), "after {step}");
  };
  let mut held = Vec::new();
  for (index, fh) in fhs.iter().enumerate() {
    let client = if index % 2 == 0 { A } else { B };
    files.open(client, b"o".to_vec(), fh, read_only()).unwrap();
    held.push((client, files.delegate_read(client, fh, 0, QUIET).unwrap()));
    check(&mut files, "a read grant");
  }
  let shared = &fhs[0];
  files.open(B, b"o".to_vec(), shared, read_only()).unwrap();
  let second = files.delegate_read(B, shared, 0, QUIET).unwrap();
  check(&mut files, "a second holder");
  files.return_delegation(&second, B, shared).unwrap();
  check(&mut files, "a return");
  files.recall(&fhs[1], Some(A), Conflict::Change, 1);
  files.revoke_lapsed(2 + LEASE, LEASE, QUIET);
  check(&mut files, "a lapsed revocation");
  files.revoke_delegation(&held[2].1.other);
  check(&mut files, "a revocation");
  files.purge(A);
  check(&mut files, "a purge");
  let write = handle(20, 1);
  files.open(B, b"w".to_vec(), &write, read_write()).unwrap();
  files.delegate_write(B, &write, 0, QUIET).unwrap();
  check(&mut files, "a write grant");
}
