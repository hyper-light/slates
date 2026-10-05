//! NFSv4.1 open delegations at the file's owner (RFC 8881 §10.4; §4.6 A-78): which clients a file is delegated to,
//! when a recall of each began, and when a file was last recalled. Pure state with the clock passed in; the owner's
//! [`crate::v4::files::FileState`] holds it beside the file's opens, journals it durably (A-37), and decides grants
//! against those opens.
//!
//! - **Grant.** A read delegation (`OPEN_DELEGATE_READ`) lets its holder open and close the file, and trust its cache
//!   of the file's data and attributes, without asking the server (§10.4, §10.4.1). Several clients may hold one; it
//!   is never granted while a write delegation is held by another, nor for a file recalled within the last quiet
//!   period (§10.4: "the probability of future conflicting open requests should be low based on the recent history
//!   of the file"), so a file other clients keep changing does not bounce between grant and recall.
//! - **Recall.** A conflicting operation (another client's open for writing, a write, a SETATTR, a REMOVE, a RENAME,
//!   a LINK, or any slates path that changes the file: NFSv3, FUSE, the SDK, a merge, a landing — §10.2: "operations
//!   done outside the NFSv4.1 protocol ... also need to result in delegation recall") names the delegations it must
//!   wait for ([`Delegations::recall`]); each is marked recalled at that time, once.
//! - **Revoke.** A delegation not returned within the lease of its recall is revoked (§10.4.5: "servers SHOULD
//!   revoke delegations that are not returned in a period of time equal to the lease period").
//! - **Bounds.** The table holds at most its bound (the owner's open bound: a delegation accompanies an open); a grant
//!   past it is simply not made, which a client must always be prepared for (§10.2).
//!
//! Keys are file identities ([`crate::handle::identity`]), never handle bytes, so two mounts of one file share its
//! delegations as they share its locks.

use std::collections::BTreeMap;

use super::types::{OTHER_SIZE, Stateid};
use crate::nfs::Nfsfh3;

/// Format: a state id's `other`.
type Other = [u8; OTHER_SIZE];

/// One delegation: its holder, the file (the holder's own handle for it), whether it is a write delegation, its state
/// id's seqid, and when its recall began.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Delegation {
  clientid: u64,
  fh: Nfsfh3,
  write: bool,
  seqid: u32,
  recalled_ns: Option<u64>,
}

/// A delegation to recall: its state id, its holder, and the holder's handle for the file (`CB_RECALL` names both).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recall {
  /// The delegation's state id.
  pub stateid: Stateid,
  /// The client holding it.
  pub clientid: u64,
  /// The holder's handle for the file.
  pub fh: Nfsfh3,
}

/// A delegation as a durable record carries it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegationRecord {
  /// The state id's `other`.
  pub other: Other,
  /// The client holding it.
  pub clientid: u64,
  /// The holder's handle for the file.
  pub fh: Vec<u8>,
  /// Whether it is a write delegation.
  pub write: bool,
  /// The state id's current seqid.
  pub seqid: u32,
}

/// The delegations of one owner's files.
#[derive(Debug, Default)]
pub struct Delegations {
  max: usize,
  table: BTreeMap<Other, Delegation>,
  /// Every delegation by its file's identity and holder: one per client and file.
  by_file: BTreeMap<(Vec<u8>, u64), Other>,
  /// When each file was last recalled, for the quiet period; pruned as entries age past it.
  recalled: BTreeMap<Vec<u8>, u64>,
}

impl Delegations {
  /// An empty table holding at most `max` delegations.
  pub fn new(max: usize) -> Delegations {
    Delegations {
      max,
      ..Delegations::default()
    }
  }

  /// Puts a kept delegation back (a restore), even past the bound: it was admitted.
  pub fn restore(&mut self, record: DelegationRecord) {
    let fh = Nfsfh3(record.fh);
    self.by_file.insert(
      (crate::handle::identity(&fh), record.clientid),
      record.other,
    );
    self.table.insert(
      record.other,
      Delegation {
        clientid: record.clientid,
        fh,
        write: record.write,
        seqid: record.seqid,
        recalled_ns: None,
      },
    );
  }

  /// The current record of delegation `other`, if it is held.
  pub fn record(&self, other: &Other) -> Option<DelegationRecord> {
    self.table.get(other).map(|delegation| DelegationRecord {
      other: *other,
      clientid: delegation.clientid,
      fh: delegation.fh.0.clone(),
      write: delegation.write,
      seqid: delegation.seqid,
    })
  }

  /// How many delegations are held.
  pub fn len(&self) -> usize {
    self.table.len()
  }

  /// Whether none is held.
  pub fn is_empty(&self) -> bool {
    self.table.is_empty()
  }

  /// Whether `fh`'s file was recalled less than `quiet_ns` before `now_ns`.
  fn recently_recalled(&self, file: &[u8], now_ns: u64, quiet_ns: u64) -> bool {
    self
      .recalled
      .get(file)
      .is_some_and(|at| now_ns.saturating_sub(*at) < quiet_ns)
  }

  /// A read delegation of `fh` for `clientid`, minted as `other` when one is due: the client's existing delegation of
  /// the file is answered again; none is granted while another client holds a write delegation, for a file recalled
  /// within `quiet_ns`, or past the bound. The caller has checked the file's opens (no other client's open for
  /// writing or denying reads).
  pub fn grant_read(
    &mut self,
    (clientid, fh): (u64, &Nfsfh3),
    other: Other,
    now_ns: u64,
    quiet_ns: u64,
  ) -> Option<Stateid> {
    let file = crate::handle::identity(fh);
    if let Some(held) = self.by_file.get(&(file.clone(), clientid))
      && let Some(delegation) = self.table.get(held)
    {
      return (delegation.recalled_ns.is_none()).then_some(Stateid {
        seqid: delegation.seqid,
        other: *held,
      });
    }
    let written_elsewhere = self
      .by_file
      .range((file.clone(), 0)..)
      .take_while(|((held, _), _)| *held == file)
      .filter_map(|(_, other)| self.table.get(other))
      .any(|delegation| delegation.write && delegation.clientid != clientid);
    if written_elsewhere
      || self.recently_recalled(&file, now_ns, quiet_ns)
      || self.table.len() >= self.max
    {
      return None;
    }
    self.table.insert(
      other,
      Delegation {
        clientid,
        fh: fh.clone(),
        write: false,
        seqid: 1,
        recalled_ns: None,
      },
    );
    self.by_file.insert((file, clientid), other);
    Some(Stateid { seqid: 1, other })
  }

  /// `DELEGRETURN` (§18.6): the delegation `stateid` of `clientid` on `fh`, given back. `NFS4ERR_BAD_STATEID` for
  /// one this owner does not hold for that client and file.
  pub fn give_back(
    &mut self,
    stateid: &Stateid,
    clientid: u64,
    fh: &Nfsfh3,
  ) -> Result<(), super::Nfsstat4> {
    let delegation = self
      .table
      .get(&stateid.other)
      .ok_or(super::Nfsstat4::BadStateid)?;
    if delegation.clientid != clientid
      || crate::handle::identity(&delegation.fh) != crate::handle::identity(fh)
    {
      return Err(super::Nfsstat4::BadStateid);
    }
    self.remove(&stateid.other);
    Ok(())
  }

  /// The delegations of `fh`'s file an operation by `actor` (a client, or `None` for a path outside NFSv4) must wait
  /// for: every delegation another holder has when the operation `changes` the file, and every other holder's write
  /// delegation when it only reads it. Each is marked recalled at `now_ns` the first time it is named, and the file's
  /// recall time is kept for the quiet period; the ones whose recall begins now are returned to send.
  pub fn recall(
    &mut self,
    fh: &Nfsfh3,
    actor: Option<u64>,
    changes: bool,
    now_ns: u64,
  ) -> RecallPlan {
    let file = crate::handle::identity(fh);
    let held: Vec<Other> = self
      .by_file
      .range((file.clone(), 0)..)
      .take_while(|((held, _), _)| *held == file)
      .filter(|((_, holder), _)| Some(*holder) != actor)
      .map(|(_, other)| *other)
      .collect();
    let mut plan = RecallPlan::default();
    for other in held {
      let Some(delegation) = self.table.get_mut(&other) else {
        continue;
      };
      if !changes && !delegation.write {
        continue;
      }
      plan.waiting = true;
      if delegation.recalled_ns.is_none() {
        delegation.recalled_ns = Some(now_ns);
        plan.send.push(Recall {
          stateid: Stateid {
            seqid: delegation.seqid,
            other,
          },
          clientid: delegation.clientid,
          fh: delegation.fh.clone(),
        });
      }
    }
    if plan.waiting {
      self.recalled.insert(file, now_ns);
    }
    plan
  }

  /// Revokes every delegation whose recall began more than `lease_ns` before `now_ns` (§10.4.5); their `other`s,
  /// for the caller to journal and tell the holders (`SEQ4_STATUS_RECALLABLE_STATE_REVOKED`). Prunes file recall
  /// times older than the quiet period `quiet_ns` too.
  pub fn revoke_lapsed(&mut self, now_ns: u64, lease_ns: u64, quiet_ns: u64) -> Vec<(Other, u64)> {
    let lapsed: Vec<(Other, u64)> = self
      .table
      .iter()
      .filter(|(_, delegation)| {
        delegation
          .recalled_ns
          .is_some_and(|at| now_ns.saturating_sub(at) > lease_ns)
      })
      .map(|(other, delegation)| (*other, delegation.clientid))
      .collect();
    for (other, _) in &lapsed {
      self.remove(other);
    }
    self
      .recalled
      .retain(|_, at| now_ns.saturating_sub(*at) < quiet_ns);
    lapsed
  }

  /// Whether `other` names a delegation `clientid` holds of `fh`, at a current seqid (0 is "the current one").
  pub fn check(
    &self,
    stateid: &Stateid,
    clientid: u64,
    fh: &Nfsfh3,
  ) -> Result<bool, super::Nfsstat4> {
    let delegation = self
      .table
      .get(&stateid.other)
      .ok_or(super::Nfsstat4::BadStateid)?;
    if delegation.clientid != clientid
      || crate::handle::identity(&delegation.fh) != crate::handle::identity(fh)
    {
      return Err(super::Nfsstat4::BadStateid);
    }
    match stateid.seqid {
      0 => Ok(delegation.write),
      seqid if seqid == delegation.seqid => Ok(delegation.write),
      seqid if seqid < delegation.seqid => Err(super::Nfsstat4::OldStateid),
      _ => Err(super::Nfsstat4::BadStateid),
    }
  }

  /// The holder of delegation `other`, if held.
  pub fn holder(&self, other: &Other) -> Option<u64> {
    self.table.get(other).map(|delegation| delegation.clientid)
  }

  /// Every delegation of `clientid`, dropped (its lease lapsed, it rebooted, or it was destroyed); their `other`s.
  pub fn purge(&mut self, clientid: u64) -> Vec<Other> {
    let held: Vec<Other> = self
      .table
      .iter()
      .filter(|(_, delegation)| delegation.clientid == clientid)
      .map(|(other, _)| *other)
      .collect();
    for other in &held {
      self.remove(other);
    }
    held
  }

  fn remove(&mut self, other: &Other) {
    if let Some(delegation) = self.table.remove(other) {
      self
        .by_file
        .remove(&(crate::handle::identity(&delegation.fh), delegation.clientid));
    }
  }
}

/// What a conflicting operation must do about a file's delegations.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecallPlan {
  /// Whether any delegation must be returned or revoked before the operation proceeds.
  pub waiting: bool,
  /// The recalls to send now (each delegation's first).
  pub send: Vec<Recall>,
}
