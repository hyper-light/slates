//! Staging buffers for edits too large for one request (§4.12, §4.16): a request rides one bulk chunk of its
//! client's region (`config::BULK_CHUNK_BYTES`), so an edit inserting more than that is staged on the work's owner
//! first, in pages (`StageBegin`, `StagePut`), and then applied as one splice (`EditStaged`). One edit stays one
//! journal operation: splitting it into several edits would leave half of it applied if a later page failed.
//!
//! Bounds: every buffer's bytes, and a fixed charge for its record, are reserved from the shard's metadata budget
//! when it begins, whole or not at all, so the buffers a shard holds are bounded by that budget; an empty buffer is
//! refused (an empty insert fits one request). A buffer expires one lease after its last use and is released then,
//! by the shard's deadline keeper at that moment (`crate::deadlines`, A-522) or by the next staging call if it comes
//! first, so an abandoned upload holds its credit for at most a lease. Buffers are not durable: a daemon restart drops
//! them, and the client stages again.
//!
//! **An open buffer** (A-880; `StageOpen`) is for a file write whose length its writer learns only at its input's end
//! (`slates write` reading a pipe): it states no length, reserves its record when it opens and each put's bytes as the
//! put extends it, refused `BudgetExceeded` past the budget with what it holds kept, and is taken at whatever it holds.
//! It lives as long as the write attachment it was opened under, and is released when that attachment ends, however it
//! ends ([`after_borrow`], which closes every state borrow and walks the open buffers only when the shard's partition
//! has ended an attachment since), or when applied, never at a deadline: its writer holds the
//! attachment while it reads (`slates_client::Hold`, A-879), so a producer slower than a lease between pages never
//! loses it, and one that dies leaves it for at most the attachment's term. Until A-880 the CLI read its whole input
//! before it staged a byte, a second whole copy beside this buffer.
//!
//! A buffer belongs to the principal that began it: a put or an apply by any other principal is refused as if the
//! token did not exist, so a token is never a capability.

use std::collections::{BTreeMap, BTreeSet};

use slates_db::catalog::{Principal, VolumeId};
use slates_ipc::protocol::Refusal;
use slates_mem::budget::{MetadataBudget, MetadataCredit};

/// One staging buffer.
#[derive(Debug)]
struct Buffer {
  work: VolumeId,
  principal: Principal,
  bytes: Vec<u8>,
  /// Its length, stated when it began; `None` for an open buffer (A-880), whose length is what its puts put.
  len: Option<u64>,
  life: Life,
  credit: MetadataCredit,
}

/// How long a buffer lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Life {
  /// Until this deadline on the shard's monotonic clock, moved by each use.
  Until(u64),
  /// As long as this write attachment (A-880): an open buffer, released when the attachment ends.
  WithAttachment(u64),
}

/// Derived: the fixed charge for one buffer's record beside its bytes, so a shard's buffer count is bounded by its
/// metadata budget even for small buffers: the record's own size and its expiry index entry.
const RECORD_BYTES: u64 = (size_of::<Buffer>() + size_of::<(u64, u64)>()) as u64;

/// A shard's staging buffers.
#[derive(Debug, Default)]
pub(crate) struct Staging {
  next: u64,
  buffers: BTreeMap<u64, Buffer>,
  /// Every buffer with a deadline by expiry, so expiring reads only the expired.
  by_expiry: BTreeSet<(u64, u64)>,
  /// The open buffers held (A-880), each token with the attachment it lives with: what [`after_borrow`] walks once an
  /// attachment ended, never the sized buffers. Bounded by the buffers.
  open: BTreeMap<u64, u64>,
  /// The partition's count of ended attachments when [`after_borrow`] last looked
  /// (`slates_db::partition::Partition::attachments_ended`): a borrow that ended none costs one comparison.
  ended_seen: u64,
}

impl Staging {
  /// Reserves a buffer of `len` bytes for `work` on behalf of `principal`, expiring at `expires`: its token. The
  /// token carries the shard's `partition` in its high bits, so tokens never collide across shards.
  pub(crate) fn begin(
    &mut self,
    (work, principal): (VolumeId, &Principal),
    len: u64,
    (partition, expires): (u16, u64),
    budget: &mut MetadataBudget,
  ) -> Result<u64, Refusal> {
    if len == 0 {
      return Err(Refusal::BadRequest {
        reason: "an empty staging buffer: an empty insert fits one request".to_owned(),
      });
    }
    let capacity = usize::try_from(len).map_err(|_| Refusal::BudgetExceeded {
      available: budget.admittable(),
    })?;
    let credit = budget
      .reserve(len.saturating_add(RECORD_BYTES))
      .map_err(|_| Refusal::BudgetExceeded {
        available: budget.admittable(),
      })?;
    self.next = self.next.wrapping_add(1);
    let token = (u64::from(partition) << TOKEN_COUNTER_BITS) | (self.next & TOKEN_COUNTER_MASK);
    let mut bytes = Vec::new();
    if bytes.try_reserve_exact(capacity).is_err() {
      budget.release(credit);
      return Err(Refusal::BudgetExceeded {
        available: budget.admittable(),
      });
    }
    self.by_expiry.insert((expires, token));
    self.buffers.insert(
      token,
      Buffer {
        work,
        principal: principal.clone(),
        bytes,
        len: Some(len),
        life: Life::Until(expires),
        credit,
      },
    );
    Ok(token)
  }

  /// Opens a buffer of no stated length for `work` on behalf of `principal`, living as long as its write `attachment`
  /// (A-880): its token, from the shard's `partition` as [`Staging::begin`]'s. Only its record is reserved now; each
  /// put reserves its own bytes ([`Staging::put`]).
  pub(crate) fn open(
    &mut self,
    (work, principal): (VolumeId, &Principal),
    attachment: u64,
    partition: u16,
    budget: &mut MetadataBudget,
  ) -> Result<u64, Refusal> {
    let credit = budget
      .reserve(RECORD_BYTES)
      .map_err(|_| Refusal::BudgetExceeded {
        available: budget.admittable(),
      })?;
    self.next = self.next.wrapping_add(1);
    let token = (u64::from(partition) << TOKEN_COUNTER_BITS) | (self.next & TOKEN_COUNTER_MASK);
    self.buffers.insert(
      token,
      Buffer {
        work,
        principal: principal.clone(),
        bytes: Vec::new(),
        len: None,
        life: Life::WithAttachment(attachment),
        credit,
      },
    );
    self.open.insert(token, attachment);
    Ok(token)
  }

  /// Writes `bytes` at `offset` of buffer `token` for `work`, as `principal`, and moves a sized buffer's expiry to
  /// `expires`: how many bytes are written now. The bytes go where the previous put ended; a put repeating bytes already
  /// written (a retried request) is answered again without change; anything else is refused. An open buffer's put
  /// reserves its bytes from `budget` first (A-880), refused `BudgetExceeded` with the buffer as it was.
  pub(crate) fn put(
    &mut self,
    (work, principal, token): (VolumeId, &Principal, u64),
    offset: u64,
    bytes: &[u8],
    (expires, budget): (u64, &mut MetadataBudget),
  ) -> Result<u64, Refusal> {
    let buffer = self.owned(work, principal, token)?;
    let filled = u64::try_from(buffer.bytes.len()).unwrap_or(u64::MAX);
    let incoming = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let end = offset.checked_add(incoming).ok_or(Refusal::BadRequest {
      reason: "a staging put past the end of addressable bytes".to_owned(),
    })?;
    if end <= filled {
      let start = usize::try_from(offset).unwrap_or(usize::MAX);
      let stop = usize::try_from(end).unwrap_or(usize::MAX);
      if buffer.bytes.get(start..stop) == Some(bytes) {
        return Ok(filled);
      }
      return Err(Refusal::BadRequest {
        reason: "a staging put overwriting different bytes".to_owned(),
      });
    }
    if offset != filled || buffer.len.is_some_and(|len| end > len) {
      return Err(Refusal::BadRequest {
        reason: format!(
          "a staging put at {offset} of {incoming} bytes; the buffer holds {filled} of {}",
          buffer
            .len
            .map_or_else(|| "an open length".to_owned(), |len| len.to_string())
        ),
      });
    }
    if buffer.len.is_none() {
      grow_open(buffer, bytes.len(), budget)?;
    }
    buffer.bytes.extend_from_slice(bytes);
    if let Life::Until(old) = buffer.life {
      buffer.life = Life::Until(expires);
      self.by_expiry.remove(&(old, token));
      self.by_expiry.insert((expires, token));
    }
    Ok(end)
  }

  /// The full bytes of buffer `token` for `work`, taken by `principal`: the buffer is released and its credit
  /// returned. Refused while a sized buffer is not yet full; an open one is whole at whatever its puts put (A-880).
  pub(crate) fn take(
    &mut self,
    (work, principal, token): (VolumeId, &Principal, u64),
    budget: &mut MetadataBudget,
  ) -> Result<Vec<u8>, Refusal> {
    let buffer = self.owned(work, principal, token)?;
    let filled = u64::try_from(buffer.bytes.len()).unwrap_or(u64::MAX);
    if let Some(len) = buffer.len.filter(|len| filled != *len) {
      return Err(Refusal::BadRequest {
        reason: format!("the staging buffer holds {filled} of its {len} bytes"),
      });
    }
    let buffer = self.remove(token).ok_or(Refusal::NotFound)?;
    budget.release(buffer.credit);
    Ok(buffer.bytes)
  }

  /// The full bytes of buffer `token` for `work`, looked at and left in place: an edit-and-submit's edit is applied to
  /// what goes to the green's owner before the buffer is taken (A-247). `None` while it is not there or not yet full.
  pub(crate) fn peek(&self, work: VolumeId, token: u64) -> Option<&[u8]> {
    let buffer = self
      .buffers
      .get(&token)
      .filter(|buffer| buffer.work == work)?;
    let filled = u64::try_from(buffer.bytes.len()).ok()?;
    buffer
      .len
      .is_none_or(|len| filled == len)
      .then_some(buffer.bytes.as_slice())
  }

  /// Releases every buffer expired at `now`.
  pub(crate) fn expire(&mut self, now: u64, budget: &mut MetadataBudget) {
    let expired: Vec<u64> = self
      .by_expiry
      .iter()
      .take_while(|(expires, _)| *expires <= now)
      .map(|(_, token)| *token)
      .collect();
    for token in expired {
      if let Some(buffer) = self.remove(token) {
        budget.release(buffer.credit);
      }
    }
  }

  /// The earliest expiry among the buffers, the deadline the shard's keeper sleeps until (A-522, `crate::deadlines`).
  pub(crate) fn next_expiry(&self) -> Option<u64> {
    self.by_expiry.first().map(|(expires, _)| *expires)
  }

  /// Releases every open buffer whose attachment `live` no longer answers for (A-880), however that attachment ended:
  /// how many. Walks the open buffers alone.
  pub(crate) fn release_orphans(
    &mut self,
    live: impl Fn(u64) -> bool,
    budget: &mut MetadataBudget,
  ) -> usize {
    let orphans: Vec<u64> = self
      .open
      .iter()
      .filter(|(_, attachment)| !live(**attachment))
      .map(|(token, _)| *token)
      .collect();
    for token in &orphans {
      if let Some(buffer) = self.remove(*token) {
        budget.release(buffer.credit);
      }
    }
    orphans.len()
  }

  /// Whether any open buffer is held (A-880).
  pub(crate) fn holds_open(&self) -> bool {
    !self.open.is_empty()
  }

  /// Notes the partition's count of ended attachments, `ended`, and answers whether it moved since the last note while
  /// an open buffer is held (A-880): whether a borrow may have ended an open buffer's attachment.
  pub(crate) fn attachments_ended_since(&mut self, ended: u64) -> bool {
    let moved = ended != self.ended_seen;
    self.ended_seen = ended;
    moved && self.holds_open()
  }

  /// How many buffers are held.
  #[cfg(test)]
  pub(crate) fn len(&self) -> usize {
    self.buffers.len()
  }

  /// Buffer `token`, when it is `work`'s and `principal` began it; any other is refused as missing.
  fn owned(
    &mut self,
    work: VolumeId,
    principal: &Principal,
    token: u64,
  ) -> Result<&mut Buffer, Refusal> {
    self
      .buffers
      .get_mut(&token)
      .filter(|buffer| buffer.work == work && &buffer.principal == principal)
      .ok_or(Refusal::NotFound)
  }

  fn remove(&mut self, token: u64) -> Option<Buffer> {
    let buffer = self.buffers.remove(&token)?;
    match buffer.life {
      Life::Until(expires) => {
        self.by_expiry.remove(&(expires, token));
      }
      Life::WithAttachment(_) => {
        self.open.remove(&token);
      }
    }
    Some(buffer)
  }
}

/// Closes every state borrow (`state::try_with_state`): the open buffers whose attachment the borrow ended, however it
/// ended it (a detach, its term's lapse, its client's reap, its volume's destroy), are released with it (A-880). A
/// borrow that ended no attachment costs one comparison of the partition's count of ended ones; one that did walks the
/// open buffers, never the sized ones.
pub(crate) fn after_borrow(state: &mut crate::state::ShardState) {
  let crate::state::ShardState {
    staging, db, store, ..
  } = state;
  let partition = db.partition();
  if !staging.attachments_ended_since(partition.attachments_ended()) {
    return;
  }
  let released = staging.release_orphans(
    |attachment| partition.attachment(attachment).is_some(),
    &mut store.metadata,
  );
  if released > 0 {
    state.count(
      RELEASED_WITH_ATTACHMENT,
      u64::try_from(released).unwrap_or(u64::MAX),
    );
  }
}

/// Counter: open staging buffers released because their attachment ended (A-880, [`after_borrow`]): the non-vacuity
/// count of a buffer bound to its attachment.
pub(crate) const RELEASED_WITH_ATTACHMENT: &str = "staging.released_with_attachment";

/// Reserves `incoming` more bytes for an open buffer (A-880), from the budget and from the allocator, both or neither:
/// refused `BudgetExceeded` with nothing charged and the buffer as it was.
fn grow_open(
  buffer: &mut Buffer,
  incoming: usize,
  budget: &mut MetadataBudget,
) -> Result<(), Refusal> {
  let refused = |budget: &MetadataBudget| Refusal::BudgetExceeded {
    available: budget.admittable(),
  };
  let wanted = u64::try_from(incoming).map_err(|_| refused(budget))?;
  let credit = budget.reserve(wanted).map_err(|_| refused(budget))?;
  if buffer.bytes.try_reserve(incoming).is_err() {
    budget.release(credit);
    return Err(refused(budget));
  }
  buffer.credit.bytes = buffer.credit.bytes.saturating_add(credit.bytes);
  Ok(())
}

/// Format: the width of the token's counter: the low forty-eight bits, below the partition in the top sixteen.
const TOKEN_COUNTER_BITS: u32 = u64::BITS - u16::BITS;
/// Format: the token's counter bits ([`TOKEN_COUNTER_BITS`] wide).
const TOKEN_COUNTER_MASK: u64 = (1 << TOKEN_COUNTER_BITS) - 1;

#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
  use super::*;

  /// Shape: the metadata budget the tests stage against.
  const BUDGET: u64 = 1 << 20;
  /// Shape: an expiry far in the future, and a time past it.
  const LATER: u64 = 1_000;
  /// Shape: see [`LATER`].
  const PAST: u64 = 2_000;

  fn work() -> VolumeId {
    VolumeId { bytes: [3; 16] }
  }

  fn owner() -> Principal {
    Principal::Uid {
      host: slates_db::HostId(0),
      uid: 501,
    }
  }

  /// §4.12 (staging): do stage twelve bytes in two puts and take them; expect the bytes whole and the budget's
  /// credit returned. Retry the first put; expect it answered again without change. Expect a put overwriting
  /// different bytes, a put past the buffer, and a put skipping ahead each refused.
  #[test]
  fn a_staged_buffer_fills_in_order_and_is_taken_whole() {
    let mut budget = MetadataBudget::new(BUDGET);
    let mut staging = Staging::default();
    let token = staging
      .begin((work(), &owner()), 12, (1, LATER), &mut budget)
      .unwrap();
    assert!(budget.admittable() < BUDGET, "charged");
    let key = (work(), &owner(), token);
    assert_eq!(staging.put(key, 0, b"hello ", (LATER, &mut budget)), Ok(6));
    misplaced_puts_are_refused(&mut staging, key, &mut budget);
    assert_eq!(staging.put(key, 6, b"world!", (LATER, &mut budget)), Ok(12));
    assert_eq!(staging.take(key, &mut budget).unwrap(), b"hello world!");
    assert_eq!(budget.admittable(), BUDGET, "the credit is returned");
    assert_eq!(staging.len(), 0);
  }

  /// The refusals of [`a_staged_buffer_fills_in_order_and_is_taken_whole`]: after six bytes of twelve, a retried put
  /// is answered again, a put of different bytes, one skipping ahead and one past the end are refused, and the
  /// buffer is not taken while partly filled.
  fn misplaced_puts_are_refused(
    staging: &mut Staging,
    key: (VolumeId, &Principal, u64),
    budget: &mut MetadataBudget,
  ) {
    assert_eq!(
      staging.put(key, 0, b"hello ", (LATER, &mut *budget)),
      Ok(6),
      "a retried put"
    );
    assert!(
      staging
        .put(key, 0, b"HELLO ", (LATER, &mut *budget))
        .is_err(),
      "different bytes"
    );
    assert!(
      staging.put(key, 9, b"xyz", (LATER, &mut *budget)).is_err(),
      "skipping ahead"
    );
    assert!(
      staging
        .put(key, 6, b"world!!", (LATER, &mut *budget))
        .is_err(),
      "past the buffer"
    );
    assert!(staging.take(key, budget).is_err(), "not taken until full");
  }

  /// §4.12 (staging): do begin a buffer as one principal and use it as another, or for another work; expect each
  /// refused as missing (a token is never a capability). Expect an empty buffer refused, a buffer past the budget
  /// refused with nothing charged, and an expired buffer released with its credit.
  #[test]
  fn a_buffer_is_its_owners_bounded_and_expires() {
    let mut budget = MetadataBudget::new(BUDGET);
    let mut staging = Staging::default();
    let token = staging
      .begin((work(), &owner()), 4, (1, LATER), &mut budget)
      .unwrap();
    let stranger = Principal::Uid {
      host: slates_db::HostId(0),
      uid: 1000,
    };
    assert_eq!(
      staging.put((work(), &stranger, token), 0, b"abcd", (LATER, &mut budget)),
      Err(Refusal::NotFound)
    );
    let other_work = VolumeId { bytes: [4; 16] };
    assert_eq!(
      staging.put(
        (other_work, &owner(), token),
        0,
        b"abcd",
        (LATER, &mut budget)
      ),
      Err(Refusal::NotFound)
    );
    assert!(
      staging
        .begin((work(), &owner()), 0, (1, LATER), &mut budget)
        .is_err()
    );
    let before = budget.admittable();
    assert!(matches!(
      staging.begin((work(), &owner()), BUDGET, (1, LATER), &mut budget),
      Err(Refusal::BudgetExceeded { .. })
    ));
    assert_eq!(
      budget.admittable(),
      before,
      "a refused begin charges nothing"
    );
    staging.expire(PAST, &mut budget);
    assert_eq!(staging.len(), 0, "expired");
    assert_eq!(budget.admittable(), BUDGET, "its credit returned");
    assert_eq!(
      staging.put((work(), &owner(), token), 0, b"abcd", (LATER, &mut budget)),
      Err(Refusal::NotFound)
    );
  }

  /// Shape: the attachment an open buffer lives with in the tests.
  const ATTACHMENT: u64 = 7;

  /// A-880, §4.12 (staging): do open a buffer with no length under an attachment and put three pages into it, the last
  /// past what the budget has left; expect the first two charged as they arrive, the third refused `BudgetExceeded` with
  /// nothing charged for it and the buffer as it was, a retried put answered again, the buffer not expired at any time,
  /// and taken at what it holds with every credit returned.
  #[test]
  fn an_open_buffer_grows_put_by_put_within_the_budget() {
    let mut budget = MetadataBudget::new(BUDGET);
    let mut staging = Staging::default();
    let token = staging
      .open((work(), &owner()), ATTACHMENT, 1, &mut budget)
      .unwrap();
    let opened = budget.admittable();
    assert!(opened < BUDGET, "its record is charged");
    let key = (work(), &owner(), token);
    let page = vec![7u8; 4096];
    assert_eq!(staging.put(key, 0, &page, (LATER, &mut budget)), Ok(4096));
    assert_eq!(
      budget.admittable(),
      opened - 4096,
      "the first page charged as it came"
    );
    assert_eq!(
      staging.put(key, 4096, &page, (LATER, &mut budget)),
      Ok(8192)
    );
    assert_eq!(
      staging.put(key, 0, &page, (LATER, &mut budget)),
      Ok(8192),
      "a retried put"
    );
    a_put_past_the_budget_is_refused_whole(&mut staging, key, &mut budget);
    staging.expire(PAST, &mut budget);
    assert_eq!(staging.len(), 1, "an open buffer has no deadline");
    let taken = staging.take(key, &mut budget).unwrap();
    assert_eq!(taken.len(), 8192, "taken at what it holds");
    assert!(taken.iter().all(|byte| *byte == 7));
    assert_eq!(budget.admittable(), BUDGET, "every credit returned");
  }

  /// The third put of [`an_open_buffer_grows_put_by_put_within_the_budget`]: one byte past what the budget has left,
  /// refused `BudgetExceeded` with nothing charged.
  fn a_put_past_the_budget_is_refused_whole(
    staging: &mut Staging,
    key: (VolumeId, &Principal, u64),
    budget: &mut MetadataBudget,
  ) {
    let before = budget.admittable();
    let past = vec![9u8; usize::try_from(before + 1).unwrap()];
    assert!(matches!(
      staging.put(key, 8192, &past, (LATER, &mut *budget)),
      Err(Refusal::BudgetExceeded { .. })
    ));
    assert_eq!(budget.admittable(), before, "a refused put charges nothing");
  }

  /// A-880, §4.12 (staging): do open two buffers with no length under two attachments, put a page into one, and end its
  /// attachment; expect it released with its credit, a later put to it refused `NotFound`, and the other kept until its
  /// attachment ends too.
  #[test]
  fn an_open_buffer_is_released_when_its_attachment_ends() {
    let mut budget = MetadataBudget::new(BUDGET);
    let mut staging = Staging::default();
    let page = vec![7u8; 4096];
    let ended = staging
      .open((work(), &owner()), ATTACHMENT, 1, &mut budget)
      .unwrap();
    let _kept = staging
      .open((work(), &owner()), ATTACHMENT + 1, 1, &mut budget)
      .unwrap();
    assert_eq!(
      staging.put((work(), &owner(), ended), 0, &page, (LATER, &mut budget)),
      Ok(4096)
    );
    assert_eq!(
      staging.release_orphans(|attachment| attachment != ATTACHMENT, &mut budget),
      1
    );
    assert_eq!(staging.len(), 1, "the other attachment's buffer kept");
    assert_eq!(
      staging.put((work(), &owner(), ended), 4096, &page, (LATER, &mut budget)),
      Err(Refusal::NotFound),
      "released with its attachment"
    );
    assert!(staging.holds_open());
    assert_eq!(staging.release_orphans(|_| false, &mut budget), 1);
    assert!(!staging.holds_open(), "none held");
    assert_eq!(budget.admittable(), BUDGET, "released with its credit");
  }

  /// A-880, §4.12 (staging): do note the partition's count of ended attachments as borrows close, before any open buffer
  /// and with one held, the count moving or not. Expect a walk owed only when the count moved while an open buffer is
  /// held: a borrow that ended no attachment costs one comparison, and a count that moved while none was held is
  /// noted, so the first borrow after an open owes no walk for an attachment that ended before it.
  #[test]
  fn an_open_buffer_is_looked_for_only_after_an_attachment_ended() {
    let mut budget = MetadataBudget::new(BUDGET);
    let mut staging = Staging::default();
    assert!(!staging.attachments_ended_since(0), "no buffer, no change");
    assert!(
      !staging.attachments_ended_since(3),
      "an end with no open buffer held"
    );
    staging
      .open((work(), &owner()), ATTACHMENT, 1, &mut budget)
      .unwrap();
    assert!(
      !staging.attachments_ended_since(3),
      "no end since the last borrow"
    );
    assert!(
      staging.attachments_ended_since(4),
      "an attachment ended with a buffer open"
    );
    assert!(!staging.attachments_ended_since(4), "and is walked once");
  }
}
