//! Call records (A-127; RFC 8881 §2.10.6.5): a compound the daemon applied before it died is answered, when its
//! client retries it after the restart, with what it did — never run a second time.
//!
//! **Why.** A session's slot cache (`super::session`) answers a retry from memory, so it ends with the daemon. Until
//! 2026-10-09 the sessions ended with it too: a client whose reply the dead daemon never sent reconnected, was told
//! `NFS4ERR_BADSESSION`, made a new session and sent the request again, and the restarted daemon ran it a second
//! time. A REMOVE or RENAME it had already applied was answered `NFS4ERR_NOENT`, which a Linux client's `unlink` and
//! `rename` returned to the application (the mixed kill bench, two seeds of six under thirty kills;
//! docs/bugs/2026-10-09-a-compound-the-daemon-applied-before-it-died-ran-again-on-the-clients-retry.md). Sessions
//! are now durable, so the retry arrives on its own slot; this module is what answers it.
//!
//! **What is recorded.** For a compound whose SEQUENCE asked for its reply to be kept (`sa_cachethis`, the client's
//! statement that the compound is not idempotent, RFC 8881 §2.10.6.1.3; Linux sets it on OPEN, CLOSE, REMOVE,
//! RENAME, LINK, CREATE, SETATTR, LOCK, LOCKU and DELEGRETURN, never on READ or WRITE), each call the compound makes
//! is recorded at the shard that serves it: the call's key and its result. A compound that did not ask is served
//! again after a restart, which is what a server may do with a reply it did not keep (a client resends such a retry
//! as new on `NFS4ERR_RETRY_UNCACHED_REP`).
//!
//! **Keys, not positions.** A call is keyed by its procedure, a digest of its arguments, and how many earlier calls
//! of the same compound had both ([`CallKey`]). A retry runs the compound's own logic again, and that logic is
//! deterministic in its calls' results, so each call it makes meets the same key and is answered from the record. A
//! call the original never made (the logic took another path on state no record holds) meets no key and is served.
//!
//! **Durable with the effect.** A record rides the commit that makes its call's effect durable: the barrier's
//! publication of the shard image (A-61's rule for a held reply), or the transaction of a file-state change (A-37).
//! So no restart can keep an effect and lose its record. The calls a compound made before its first change (the
//! LOOKUP an OPEN makes before it creates, the REMOVE a directory refuses before the RMDIR) ride the same commit, so
//! the retry's path through them is the original's.
//!
//! **Bulk reads are not kept.** A READ, READDIR, READLINK or extended-attribute read is recorded by its key alone and
//! served again on a retry: keeping its bytes would cost a copy on the read path, and nothing a later call does
//! depends on them. The stated limit: a compound that reads bytes after changing them, cut by a restart after the
//! change and retried, reads them as they are at the retry.
//!
//! **Bounds.** One record per slot; a record for a newer sequence id replaces the slot's last. A shard's records are
//! bounded in bytes ([`CallRecords::new`]), and a call that would record past the bound is refused before it runs
//! (the front end answers `NFS4ERR_DELAY`). A dropped client's and a destroyed session's records are purged at every
//! owner.

use std::collections::{BTreeMap, BTreeSet};

use super::types::{SESSIONID_SIZE, SessionId};
use crate::procedures::{
  NFSPROC3_READ, NFSPROC3_READDIR, NFSPROC3_READDIRPLUS, NFSPROC3_READLINK, NFSPROC3_WRITE,
  extension,
};
use crate::xdr::{XdrReader, XdrWriter};

/// Format: the bytes of the key a record is published and committed under: the session id, then the slot as a
/// big-endian word.
pub const RECORD_KEY_BYTES: usize = SESSIONID_SIZE + size_of::<u32>();

/// The key a record is published and committed under ([`RECORD_KEY_BYTES`]).
pub type RecordKey = [u8; RECORD_KEY_BYTES];

/// Format: half the sequence id space. RFC 1982 serial number arithmetic: `a` follows `b` when `a − b`, wrapping, is
/// nonzero and less than this, the comparison a slot's sequence ids wrap under (RFC 8881 §2.10.6.1).
const HALF_SEQUENCE_SPACE: u32 = 1 << 31;

/// Format: the bytes a record's encoding spends besides its entries: the client id, the sequence id and the entry
/// count.
const RECORD_OVERHEAD_BYTES: usize = size_of::<u64>() + 2 * size_of::<u32>();

/// Format: the bytes an entry's encoding spends on its key and its result's presence: the procedure, the digest, the
/// occurrence and the presence word. A present result adds its length word and its bytes, padded to a word.
const ENTRY_OVERHEAD_BYTES: usize = size_of::<u32>() + size_of::<u64>() + 2 * size_of::<u32>();

/// The compound a call belongs to: the session, slot and sequence id its SEQUENCE named, and the session's client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CallIdentity {
  /// The session.
  pub session: SessionId,
  /// The slot.
  pub slot: u32,
  /// The slot's sequence id for this compound.
  pub sequence: u32,
  /// The session's client.
  pub clientid: u64,
}

/// One call of a compound: its procedure, the digest of its arguments ([`digest`]), and how many earlier calls of
/// the same compound had both.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CallKey {
  /// The procedure.
  pub procedure: u32,
  /// The arguments' digest.
  pub digest: u64,
  /// The earlier calls of the compound with this procedure and digest.
  pub occurrence: u32,
}

/// One recorded call: its key, and its result unless it is a bulk read ([`is_bulk`]).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
  key: CallKey,
  result: Option<Vec<u8>>,
}

/// One slot's record: the compound its calls belong to and the calls this shard served for it, in order. `published`
/// once one of them changed what a shard publication carries: only then does the record ride publications (a record
/// whose changes were all file state rides their transactions instead). `restored` when it came back from a previous
/// daemon life: only a retry then meets it. `bytes` is its encoding's size, kept as entries are added.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Record {
  clientid: u64,
  sequence: u32,
  entries: Vec<Entry>,
  published: bool,
  restored: bool,
  bytes: usize,
}

impl Record {
  /// A record for the compound `identity`, holding nothing yet.
  fn new(identity: &CallIdentity) -> Record {
    Record {
      clientid: identity.clientid,
      sequence: identity.sequence,
      entries: Vec::new(),
      published: false,
      restored: false,
      bytes: RECORD_OVERHEAD_BYTES,
    }
  }

  /// The bytes an entry adds to its record's encoding: its key and presence word, and a present result's length word
  /// and bytes padded to a word, as XDR writes them.
  fn entry_bytes(result: Option<&[u8]>) -> usize {
    ENTRY_OVERHEAD_BYTES.saturating_add(result.map_or(0, |result| {
      result
        .len()
        .checked_next_multiple_of(size_of::<u32>())
        .unwrap_or(usize::MAX)
        .saturating_add(size_of::<u32>())
    }))
  }

  /// This record's encoding: the client id, the sequence id, then each entry's key and its result (absent for a bulk
  /// read). Written into a buffer of its known size, so the encoding never grows it.
  fn encode(&self) -> Vec<u8> {
    let mut out = XdrWriter::with_capacity(self.bytes);
    out.u64(self.clientid);
    out.u32(self.sequence);
    out.u32(u32::try_from(self.entries.len()).unwrap_or(u32::MAX));
    for entry in &self.entries {
      out.u32(entry.key.procedure);
      out.u64(entry.key.digest);
      out.u32(entry.key.occurrence);
      match &entry.result {
        Some(result) => {
          out.bool(true);
          out.opaque(result);
        }
        None => out.bool(false),
      }
    }
    out.into_bytes()
  }

  /// A record from its encoding; `None` for bytes that are not one whole record. The entry count is checked against
  /// what the bytes can hold before anything is allocated for it.
  fn decode(bytes: &[u8]) -> Option<Record> {
    let mut reader = XdrReader::new(bytes);
    let clientid = reader.u64().ok()?;
    let sequence = reader.u32().ok()?;
    let count = usize::try_from(reader.u32().ok()?).ok()?;
    let fits = reader
      .remaining()
      .checked_div(ENTRY_OVERHEAD_BYTES)
      .unwrap_or(0);
    if count > fits {
      return None;
    }
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
      let key = CallKey {
        procedure: reader.u32().ok()?,
        digest: reader.u64().ok()?,
        occurrence: reader.u32().ok()?,
      };
      let result = if reader.bool().ok()? {
        let limit = reader.remaining();
        Some(reader.opaque(limit).ok()?.to_vec())
      } else {
        None
      };
      entries.push(Entry { key, result });
    }
    if reader.remaining() != 0 {
      return None;
    }
    let bytes = entries.iter().fold(RECORD_OVERHEAD_BYTES, |sum, entry| {
      sum.saturating_add(Record::entry_bytes(entry.result.as_deref()))
    });
    Some(Record {
      clientid,
      sequence,
      entries,
      published: true,
      restored: true,
      bytes,
    })
  }

  /// Whether this record holds more of the same compound than `other`, or a later compound of the same slot: the one
  /// a restore keeps of two copies (a commit's copy is never shorter than an earlier commit's of the same compound,
  /// since a compound's entries are only ever appended).
  fn supersedes(&self, other: &Record) -> bool {
    let ahead = self.sequence.wrapping_sub(other.sequence);
    if ahead == 0 {
      self.entries.len() > other.entries.len()
    } else {
      ahead < HALF_SEQUENCE_SPACE
    }
  }
}

/// The key a slot's record is published and committed under.
pub fn record_key(session: &SessionId, slot: u32) -> RecordKey {
  let mut key = [0u8; RECORD_KEY_BYTES];
  let (session_part, slot_part) = key.split_at_mut(SESSIONID_SIZE);
  session_part.copy_from_slice(session);
  slot_part.copy_from_slice(&slot.to_be_bytes());
  key
}

/// The session and slot a record key names.
fn slot_of(key: &RecordKey) -> (SessionId, u32) {
  let mut session = [0u8; SESSIONID_SIZE];
  let (session_part, slot_part) = key.split_at(SESSIONID_SIZE);
  session.copy_from_slice(session_part);
  let mut slot = [0u8; size_of::<u32>()];
  slot.copy_from_slice(slot_part);
  (session, u32::from_be_bytes(slot))
}

/// A shard's call records (the module doc), bounded in bytes. The records that changed since the shard's last
/// publication, and those purged since, are what its next delta carries.
#[derive(Debug)]
pub struct CallRecords {
  records: BTreeMap<(SessionId, u32), Record>,
  dirty: BTreeSet<(SessionId, u32)>,
  removed: BTreeSet<(SessionId, u32)>,
  bytes: usize,
  limit: usize,
  /// How many records came back from a previous daemon life and are still held: none in the steady state, when no
  /// call can be a retry the records answer, so [`CallRecords::answer`] costs nothing.
  restored: usize,
}

impl CallRecords {
  /// No records, bounded at `limit` bytes of their encodings.
  pub fn new(limit: usize) -> CallRecords {
    CallRecords {
      records: BTreeMap::new(),
      dirty: BTreeSet::new(),
      removed: BTreeSet::new(),
      bytes: 0,
      limit,
      restored: 0,
    }
  }

  /// The recorded result of the call `key` of the compound `identity`, when this shard served it for that same
  /// compound and kept its result: what a retry is answered with instead of the call being served again.
  pub fn answer(&self, identity: &CallIdentity, key: &CallKey) -> Option<&[u8]> {
    if self.restored == 0 {
      return None;
    }
    let record = self.records.get(&(identity.session, identity.slot))?;
    if record.sequence != identity.sequence {
      return None;
    }
    record
      .entries
      .iter()
      .find(|entry| entry.key == *key)
      .and_then(|entry| entry.result.as_deref())
  }

  /// Whether the call `key` of a retried compound `identity` met no entry of the record a previous daemon life left
  /// for that compound while one with the same procedure and occurrence is there: the retry's call differs from the
  /// original's in its digest, so it is served again instead of answered (counted by the caller, so a call whose
  /// arguments change across a retry is seen, never silent). A record of this life is never asked: two calls of one
  /// compound with the same procedure and different arguments each have occurrence zero.
  pub fn diverged(&self, identity: &CallIdentity, key: &CallKey) -> bool {
    if self.restored == 0 {
      return false;
    }
    self
      .records
      .get(&(identity.session, identity.slot))
      .filter(|record| record.restored && record.sequence == identity.sequence)
      .is_some_and(|record| {
        let same_call = |entry: &&Entry| {
          entry.key.procedure == key.procedure && entry.key.occurrence == key.occurrence
        };
        !record.entries.iter().any(|entry| entry.key == *key)
          && record.entries.iter().any(|entry| same_call(&entry))
      })
  }

  /// Whether recording a call of the compound `identity` would pass the bound: the records already fill it, counting
  /// as freed the slot's record of an earlier compound, which this call's record replaces.
  pub fn refuses(&self, identity: &CallIdentity) -> bool {
    if self.bytes < self.limit {
      return false;
    }
    let replaced = self
      .records
      .get(&(identity.session, identity.slot))
      .filter(|record| record.sequence != identity.sequence)
      .map_or(0, |record| record.bytes);
    self.bytes.saturating_sub(replaced) >= self.limit
  }

  /// Records the call `key` of the compound `identity` with its `result` (`None` for a bulk read), replacing the
  /// slot's record of an earlier compound and the result of the same key. `published` when the call changed what the
  /// shard's next publication carries: the record, as it stands now, then rides that publication. A call recorded
  /// after the compound's last change rides nothing new: a retry serves it again, past every recorded change. A
  /// change to file state rides its own transaction instead ([`CallRecords::committed`]).
  pub fn record(
    &mut self,
    identity: &CallIdentity,
    key: CallKey,
    result: Option<&[u8]>,
    published: bool,
  ) {
    let slot = (identity.session, identity.slot);
    let record = self
      .records
      .entry(slot)
      .or_insert_with(|| Record::new(identity));
    let before = record.bytes;
    let mut retired = false;
    if record.sequence != identity.sequence {
      retired = record.restored;
      *record = Record::new(identity);
    }
    match record.entries.iter_mut().find(|entry| entry.key == key) {
      Some(entry) => {
        record.bytes = record
          .bytes
          .saturating_sub(Record::entry_bytes(entry.result.as_deref()))
          .saturating_add(Record::entry_bytes(result));
        entry.result = result.map(<[u8]>::to_vec);
      }
      None => {
        record.bytes = record.bytes.saturating_add(Record::entry_bytes(result));
        record.entries.push(Entry {
          key,
          result: result.map(<[u8]>::to_vec),
        });
      }
    }
    record.published |= published;
    let after = record.bytes;
    self.bytes = self.bytes.saturating_sub(before).saturating_add(after);
    if retired {
      self.restored = self.restored.saturating_sub(1);
    }
    if published {
      self.dirty.insert(slot);
      self.removed.remove(&slot);
    }
  }

  /// The slot's record as it stands, encoded under its key: what the transaction of a file-state change one of its
  /// calls made carries.
  pub fn committed(&self, session: &SessionId, slot: u32) -> Option<(RecordKey, Vec<u8>)> {
    let record = self.records.get(&(*session, slot))?;
    Some((record_key(session, slot), record.encode()))
  }

  /// What the shard's next delta carries: the records a change rode since its last publication, encoded under their
  /// keys, and the keys purged since.
  pub fn delta(&self) -> (Vec<(RecordKey, Vec<u8>)>, Vec<RecordKey>) {
    let changed = self
      .dirty
      .iter()
      .filter_map(|slot| {
        self
          .records
          .get(slot)
          .filter(|record| record.published)
          .map(|record| (record_key(&slot.0, slot.1), record.encode()))
      })
      .collect();
    let removed = self
      .removed
      .iter()
      .map(|slot| record_key(&slot.0, slot.1))
      .collect();
    (changed, removed)
  }

  /// What a checkpoint of the shard carries: every record a publication carries, encoded under its key.
  pub fn checkpoint(&self) -> Vec<(RecordKey, Vec<u8>)> {
    self
      .records
      .iter()
      .filter(|(_, record)| record.published)
      .map(|(slot, record)| (record_key(&slot.0, slot.1), record.encode()))
      .collect()
  }

  /// A publication carrying [`CallRecords::delta`] or [`CallRecords::checkpoint`] committed: nothing is owed to the
  /// next one.
  pub fn published(&mut self) {
    self.dirty.clear();
    self.removed.clear();
  }

  /// Takes back the records a restarted shard recovered — from its image and from its partition, in any order — as
  /// durable records; of two copies of one slot's record, the later compound's, or the longer of one compound's.
  /// Returns how many could not be decoded, which are left out.
  pub fn restore(&mut self, kept: impl IntoIterator<Item = (RecordKey, Vec<u8>)>) -> usize {
    let mut refused = 0usize;
    for (key, bytes) in kept {
      let Some(record) = Record::decode(&bytes) else {
        refused = refused.saturating_add(1);
        continue;
      };
      let slot = slot_of(&key);
      let keep = self
        .records
        .get(&slot)
        .is_none_or(|existing| record.supersedes(existing));
      if keep {
        let after = record.bytes;
        match self.records.insert(slot, record) {
          Some(existing) => {
            self.bytes = self.bytes.saturating_sub(existing.bytes);
            if !existing.restored {
              self.restored = self.restored.saturating_add(1);
            }
          }
          None => self.restored = self.restored.saturating_add(1),
        }
        self.bytes = self.bytes.saturating_add(after);
      }
    }
    refused
  }

  /// Drops every record of `clientid` (its client was dropped), each owed to the next delta as purged.
  pub fn purge_client(&mut self, clientid: u64) {
    self.purge(|slot, record| {
      let _ = slot;
      record.clientid == clientid
    });
  }

  /// Drops every record of `session` (it was destroyed), each owed to the next delta as purged.
  pub fn purge_session(&mut self, session: &SessionId) {
    self.purge(|slot, _| slot.0 == *session);
  }

  /// Drops the records `doomed` names.
  fn purge(&mut self, doomed: impl Fn(&(SessionId, u32), &Record) -> bool) {
    let gone: Vec<(SessionId, u32)> = self
      .records
      .iter()
      .filter(|(slot, record)| doomed(slot, record))
      .map(|(slot, _)| *slot)
      .collect();
    for slot in gone {
      if let Some(record) = self.records.remove(&slot) {
        self.bytes = self.bytes.saturating_sub(record.bytes);
        if record.restored {
          self.restored = self.restored.saturating_sub(1);
        }
      }
      self.dirty.remove(&slot);
      self.removed.insert(slot);
    }
  }

  /// Whether a purge's removal is still owed to the next publication.
  pub fn owes_removals(&self) -> bool {
    !self.removed.is_empty()
  }

  /// The bytes the records' encodings hold, which the bound counts.
  pub fn bytes(&self) -> usize {
    self.bytes
  }

  /// How many slots hold a record.
  pub fn len(&self) -> usize {
    self.records.len()
  }

  /// Whether no slot holds a record.
  pub fn is_empty(&self) -> bool {
    self.records.is_empty()
  }
}

/// Whether `procedure` is a bulk read: recorded by its key alone and served again on a retry (the module doc).
pub fn is_bulk(procedure: u32) -> bool {
  matches!(
    procedure,
    NFSPROC3_READ
      | NFSPROC3_READLINK
      | NFSPROC3_READDIR
      | NFSPROC3_READDIRPLUS
      | extension::XATTR_GET
      | extension::XATTR_LIST
      | extension::READ_STATE
      | extension::READDIR4
  )
}

/// Format: the bytes a STATE_OPEN's arguments end with that a retry of its compound may not repeat: the delegations
/// the client may take (a word, which the session's back channel decides and a restart puts down) and the caller's
/// clock (eight bytes) — `crate::procedures::extension::STATE_OPEN`'s layout.
const STATE_OPEN_VOLATILE_BYTES: usize = size_of::<u32>() + size_of::<u64>();

/// Format: the multiplier of the word mix the digest runs: FxHash's (rustc's `rustc-hash`, which hashes every key of
/// the compiler's tables this way), an odd constant whose product spreads each word's bits across the state.
const DIGEST_MULTIPLIER: u64 = 0x51_7c_c1_b7_27_22_0a_95;

/// Format: the rotation of the word mix, FxHash's.
const DIGEST_ROTATION: u32 = 5;

/// The digest's word mix over `bytes` from `state`, eight bytes at a time, the last word zero-padded.
fn mix(state: u64, bytes: &[u8]) -> u64 {
  bytes.chunks(size_of::<u64>()).fold(state, |state, chunk| {
    let mut word = [0u8; size_of::<u64>()];
    if let Some(head) = word.get_mut(..chunk.len()) {
      head.copy_from_slice(chunk);
    }
    (state.rotate_left(DIGEST_ROTATION) ^ u64::from_le_bytes(word)).wrapping_mul(DIGEST_MULTIPLIER)
  })
}

/// The digest a call's key carries: its procedure, its arguments' length and its arguments, under a word mix
/// ([`DIGEST_MULTIPLIER`]). It tells one compound's calls apart and a retried call from another; it need not resist an
/// adversary, since a client can only confuse its own retry, so it costs a few nanoseconds a call rather than a
/// cryptographic hash's setup (BLAKE3 was 0.47% of the daemon's samples under a create storm, 2026-10-09). What a
/// retry of the same call need not repeat is left out: a WRITE's data (its length is kept), so a kept compound's WRITE
/// costs a digest of its header, never of its bytes; and a STATE_OPEN's delegation wish and clock
/// ([`STATE_OPEN_VOLATILE_BYTES`]), which differ on a retry after a restart though the open is the same. Arguments
/// that do not parse as their procedure's are digested whole.
pub fn digest(procedure: u32, args: &[u8]) -> u64 {
  let length = u64::try_from(args.len()).unwrap_or(u64::MAX);
  let state = mix(
    mix(0, &u64::from(procedure).to_le_bytes()),
    &length.to_le_bytes(),
  );
  match data_span(procedure, args) {
    Some((head, tail)) => mix(mix(state, head), tail),
    None => {
      let stable = match procedure {
        extension::STATE_OPEN => args
          .len()
          .checked_sub(STATE_OPEN_VOLATILE_BYTES)
          .and_then(|end| args.get(..end))
          .unwrap_or(args),
        _ => args,
      };
      mix(state, stable)
    }
  }
}

/// A WRITE's arguments around its data: everything up to and including the data's length word, and everything
/// after the data (a state-carrying WRITE's client and state id). `None` for any other procedure, or arguments that
/// do not parse as a WRITE.
fn data_span(procedure: u32, args: &[u8]) -> Option<(&[u8], &[u8])> {
  if procedure != NFSPROC3_WRITE && procedure != extension::WRITE_STATE {
    return None;
  }
  let mut reader = XdrReader::new(args);
  let limit = reader.remaining();
  reader.opaque(limit).ok()?; // the file handle
  reader.u64().ok()?; // the offset
  reader.u32().ok()?; // the count
  reader.u32().ok()?; // the stability
  let data = usize::try_from(reader.u32().ok()?).ok()?;
  let head_end = args.len().checked_sub(reader.remaining())?;
  let padded = data.checked_next_multiple_of(size_of::<u32>())?;
  let tail_start = head_end.checked_add(padded)?;
  Some((args.get(..head_end)?, args.get(tail_start..)?))
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Shape: a bound no test reaches unless it means to.
  const ROOMY: usize = 1 << 20;

  fn identity(sequence: u32) -> CallIdentity {
    CallIdentity {
      session: [7; SESSIONID_SIZE],
      slot: 3,
      sequence,
      clientid: 0x0102_0304_0506_0708,
    }
  }

  fn key(procedure: u32, occurrence: u32) -> CallKey {
    CallKey {
      procedure,
      digest: u64::from(procedure).wrapping_mul(31),
      occurrence,
    }
  }

  /// The records a restarted shard holds after a daemon that held `records` died: what a retry meets.
  fn after_restart(records: &CallRecords) -> CallRecords {
    let mut restored = CallRecords::new(ROOMY);
    assert_eq!(restored.restore(records.checkpoint()), 0);
    restored
  }

  /// T-127.1: record a REMOVE of compound 5, restart, then ask for it — answered at sequence 5 and that key, and at no
  /// other sequence, occurrence or procedure; before the restart nothing is asked of it (a retry within one daemon
  /// life is the slot cache's).
  #[test]
  fn a_recorded_call_is_answered_only_for_its_own_compound_and_key() {
    let mut records = CallRecords::new(ROOMY);
    let remove = key(crate::procedures::NFSPROC3_REMOVE, 0);
    records.record(&identity(5), remove, Some(b"removed"), true);
    assert_eq!(
      records.answer(&identity(5), &remove),
      None,
      "this life's record"
    );
    let records = after_restart(&records);
    assert_eq!(records.answer(&identity(5), &remove), Some(&b"removed"[..]));
    assert_eq!(
      records.answer(&identity(6), &remove),
      None,
      "another compound"
    );
    assert_eq!(
      records.answer(&identity(5), &key(crate::procedures::NFSPROC3_REMOVE, 1)),
      None,
      "a second REMOVE of the same compound"
    );
    assert_eq!(
      records.answer(&identity(5), &key(crate::procedures::NFSPROC3_RMDIR, 0)),
      None,
      "another procedure"
    );
  }

  /// T-127.2: record compound 5, then a call of compound 6 on the same slot — the slot holds only compound 6, the bytes
  /// counted are its alone, and after a restart compound 6 is answered and compound 5 is not.
  #[test]
  fn a_later_compound_replaces_its_slots_record() {
    let mut records = CallRecords::new(ROOMY);
    records.record(&identity(5), key(1, 0), Some(&[0; 100]), true);
    let first = records.bytes();
    records.record(&identity(6), key(2, 0), Some(&[0; 10]), true);
    assert_eq!(records.len(), 1);
    assert!(
      records.bytes() < first,
      "the replaced record's bytes are given back"
    );
    let records = after_restart(&records);
    assert_eq!(records.answer(&identity(5), &key(1, 0)), None);
    assert_eq!(
      records.answer(&identity(6), &key(2, 0)),
      Some(&[0u8; 10][..])
    );
  }

  /// T-127.3: record a READ by key alone — a retry is not answered from it and is served again.
  #[test]
  fn a_bulk_read_is_served_again_never_answered() {
    let mut records = CallRecords::new(ROOMY);
    let read = key(NFSPROC3_READ, 0);
    assert!(is_bulk(NFSPROC3_READ) && !is_bulk(crate::procedures::NFSPROC3_REMOVE));
    records.record(&identity(5), read, None, false);
    assert_eq!(records.answer(&identity(5), &read), None);
  }

  /// T-127.4: a record whose calls changed nothing rides no publication; one with a change rides the next delta,
  /// whole, until a publication carries it; a call recorded after the change makes it ride nothing new; a change to
  /// file state rides its transaction, not a publication.
  #[test]
  fn only_a_record_with_a_change_rides_a_publication() {
    let mut records = CallRecords::new(ROOMY);
    records.record(&identity(5), key(1, 0), Some(b"looked up"), false);
    assert!(records.delta().0.is_empty() && records.checkpoint().is_empty());
    records.record(&identity(5), key(2, 0), Some(b"created"), true);
    let (changed, removed) = records.delta();
    assert_eq!(changed.len(), 1);
    assert!(removed.is_empty());
    let (_, bytes) = changed.first().unwrap();
    let carried = Record::decode(bytes).unwrap();
    assert_eq!(
      carried.entries.len(),
      2,
      "the lookup before the change rides with it"
    );
    records.published();
    assert!(
      records.delta().0.is_empty(),
      "nothing owed after the publication"
    );
    records.record(&identity(5), key(3, 0), Some(b"attributes after"), false);
    assert!(
      records.delta().0.is_empty(),
      "a read after the change rides nothing new"
    );
    assert_eq!(
      records.checkpoint().len(),
      1,
      "a checkpoint still carries it"
    );
    records.record(&identity(6), key(4, 0), Some(b"closed"), false);
    assert!(records.delta().0.is_empty() && records.checkpoint().is_empty());
    let (_, committed) = records
      .committed(&identity(6).session, identity(6).slot)
      .unwrap();
    assert_eq!(
      Record::decode(&committed).unwrap().entries.len(),
      1,
      "its transaction carries it"
    );
  }

  /// T-127.12: a restored record answers until its slot's next compound replaces it or a purge drops it; after that
  /// no record is restored and answering costs nothing (non-vacuity: the restored one answered first).
  #[test]
  fn a_restored_record_answers_until_it_is_replaced() {
    let mut kept = CallRecords::new(ROOMY);
    let mut other = identity(1);
    other.slot = 9;
    kept.record(&identity(5), key(1, 0), Some(b"a"), true);
    kept.record(&other, key(1, 0), Some(b"b"), true);
    let mut restored = CallRecords::new(ROOMY);
    restored.restore(kept.checkpoint());
    assert_eq!(restored.restored, 2);
    assert_eq!(restored.answer(&identity(5), &key(1, 0)), Some(&b"a"[..]));
    restored.record(&identity(6), key(2, 0), Some(b"c"), true);
    assert_eq!(
      restored.restored, 1,
      "the slot's next compound retired its restored record"
    );
    restored.purge_session(&other.session);
    assert_eq!(restored.restored, 0, "the purge dropped the other");
    assert_eq!(
      restored.answer(&identity(6), &key(2, 0)),
      None,
      "nothing restored answers"
    );
  }

  /// T-127.11: a record's counted bytes are its encoding's, as entries are added and replaced, so the bound counts
  /// what a publication writes.
  #[test]
  fn a_records_counted_bytes_are_its_encodings() {
    let mut records = CallRecords::new(ROOMY);
    records.record(&identity(5), key(1, 0), Some(b"odd"), true);
    records.record(&identity(5), key(2, 0), None, true);
    records.record(&identity(5), key(1, 0), Some(b"longer result"), true);
    let (_, encoded) = records.checkpoint().pop().unwrap();
    assert_eq!(records.bytes(), encoded.len());
  }

  /// T-127.5: purge a client and a session — their records are gone, their bytes given back, and each is owed to the
  /// next delta as purged.
  #[test]
  fn a_purge_drops_records_and_owes_their_removal() {
    let mut records = CallRecords::new(ROOMY);
    let mut other = identity(1);
    other.session = [9; SESSIONID_SIZE];
    other.clientid = 77;
    records.record(&identity(5), key(1, 0), Some(b"a"), true);
    records.record(&other, key(1, 0), Some(b"b"), true);
    records.published();
    records.purge_client(77);
    assert_eq!(records.len(), 1);
    assert_eq!(
      records.delta().1,
      vec![record_key(&other.session, other.slot)]
    );
    records.purge_session(&identity(5).session);
    assert!(records.is_empty());
    assert_eq!(records.bytes(), 0);
    assert_eq!(records.delta().1.len(), 2);
  }

  /// T-127.6: restore two copies of one slot's record in either order — the later compound's wins, and of one
  /// compound's, the longer.
  #[test]
  fn a_restore_keeps_the_later_or_longer_copy() {
    let mut early = CallRecords::new(ROOMY);
    early.record(&identity(5), key(1, 0), Some(b"a"), true);
    let mut late = CallRecords::new(ROOMY);
    late.record(&identity(6), key(1, 0), Some(b"b"), true);
    let mut longer = CallRecords::new(ROOMY);
    longer.record(&identity(6), key(1, 0), Some(b"b"), true);
    longer.record(&identity(6), key(2, 0), Some(b"c"), true);
    for order in [[&early, &late, &longer], [&longer, &late, &early]] {
      let mut restored = CallRecords::new(ROOMY);
      let kept: Vec<_> = order
        .iter()
        .flat_map(|records| records.checkpoint())
        .collect();
      assert_eq!(restored.restore(kept), 0);
      assert_eq!(restored.answer(&identity(6), &key(2, 0)), Some(&b"c"[..]));
      assert_eq!(restored.answer(&identity(5), &key(1, 0)), None);
    }
    // Sequence ids wrap: the compound after u32::MAX is later.
    let mut wrapped = CallRecords::new(ROOMY);
    let mut last = CallRecords::new(ROOMY);
    last.record(&identity(u32::MAX), key(1, 0), Some(b"x"), true);
    let mut first = CallRecords::new(ROOMY);
    first.record(&identity(0), key(1, 0), Some(b"y"), true);
    wrapped.restore(last.checkpoint().into_iter().chain(first.checkpoint()));
    assert_eq!(wrapped.answer(&identity(0), &key(1, 0)), Some(&b"y"[..]));
  }

  /// T-127.10: a record of this life never reports a divergence (two calls of one compound with one procedure and
  /// different arguments are both occurrence zero); a restored record does, for a retry's call whose digest differs
  /// from the original's at the same procedure and occurrence, and not for one it answers.
  #[test]
  fn only_a_restored_record_reports_a_divergence() {
    let mut records = CallRecords::new(ROOMY);
    let first = key(crate::procedures::NFSPROC3_GETATTR, 0);
    let mut other = first;
    other.digest ^= 1;
    records.record(&identity(5), first, Some(b"attrs"), true);
    assert!(
      !records.diverged(&identity(5), &other),
      "this life's record"
    );
    let mut restored = CallRecords::new(ROOMY);
    restored.restore(records.checkpoint());
    assert!(
      restored.diverged(&identity(5), &other),
      "a retry's call under another digest"
    );
    assert!(
      !restored.diverged(&identity(5), &first),
      "the call it answers"
    );
    assert!(!restored.diverged(&identity(6), &other), "another compound");
  }

  /// T-127.7: fill the bound — a new slot's call is refused, while a slot whose earlier record frees the room is not.
  #[test]
  fn a_call_is_refused_at_the_bound_unless_its_slot_frees_the_room() {
    let mut records = CallRecords::new(64);
    records.record(&identity(5), key(1, 0), Some(&[0; 40]), true);
    assert!(records.bytes() >= 64);
    let mut other = identity(1);
    other.slot = 4;
    assert!(records.refuses(&other), "a new slot past the bound");
    assert!(
      !records.refuses(&identity(6)),
      "the slot's own later compound replaces it"
    );
    assert!(
      records.refuses(&identity(5)),
      "the same compound would grow past it"
    );
  }

  /// T-127.8 (hostile input): a record cut at every length, one claiming more entries than its bytes hold, and one
  /// with bytes after it are each refused, never decoded and never allocated for.
  #[test]
  fn a_malformed_record_is_refused() {
    let mut records = CallRecords::new(ROOMY);
    records.record(&identity(5), key(1, 0), Some(b"result"), true);
    records.record(&identity(5), key(2, 0), None, true);
    let (_, bytes) = records.checkpoint().pop().unwrap();
    assert!(Record::decode(&bytes).is_some());
    for cut in 0..bytes.len() {
      assert!(Record::decode(&bytes[..cut]).is_none(), "cut at {cut}");
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(Record::decode(&trailing).is_none());
    let mut huge = bytes.clone();
    huge[12..16].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(Record::decode(&huge).is_none());
    let mut restored = CallRecords::new(ROOMY);
    assert_eq!(restored.restore([(record_key(&[1; 16], 0), huge)]), 1);
    assert!(restored.is_empty());
  }

  /// T-127.9: a WRITE's digest covers its handle, offset, count, stability and length, not its data; a state-carrying
  /// WRITE's covers its state id too.
  #[test]
  fn a_writes_digest_leaves_out_its_data() {
    fn write(offset: u64, data: &[u8], state: &[u8]) -> Vec<u8> {
      let mut args = XdrWriter::new();
      args.opaque(&[5; 32]);
      args.u64(offset);
      args.u32(u32::try_from(data.len()).unwrap());
      args.u32(2);
      args.opaque(data);
      args.fixed(state);
      args.into_bytes()
    }
    let plain = |offset, data: &[u8]| digest(NFSPROC3_WRITE, &write(offset, data, &[]));
    assert_eq!(plain(0, b"abcd"), plain(0, b"wxyz"), "the data is left out");
    assert_ne!(plain(0, b"abcd"), plain(4, b"abcd"), "the offset is not");
    assert_ne!(plain(0, b"abcd"), plain(0, b"abcde"), "nor the length");
    let state = |id: u8| digest(extension::WRITE_STATE, &write(0, b"abcd", &[id; 24]));
    assert_ne!(
      state(1),
      state(2),
      "a state-carrying WRITE's state id is digested"
    );
    let open = |wish: u32, clock: u64, deny: u32| {
      let mut args = XdrWriter::new();
      args.opaque(&[5; 32]);
      args.u64(9);
      args.opaque(b"owner");
      args.u32(3);
      args.u32(deny);
      args.u32(wish);
      args.u64(clock);
      digest(extension::STATE_OPEN, &args.into_bytes())
    };
    assert_eq!(
      open(1, 100, 0),
      open(0, 200, 0),
      "a STATE_OPEN's wish and clock are left out"
    );
    assert_ne!(open(1, 100, 0), open(1, 100, 1), "its share is not");
    assert_ne!(
      digest(crate::procedures::NFSPROC3_REMOVE, b"x"),
      digest(crate::procedures::NFSPROC3_RMDIR, b"x"),
      "the procedure is digested"
    );
  }
}
