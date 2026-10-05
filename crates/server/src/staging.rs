//! Staging buffers for edits too large for one request (§4.12, §4.16): a request rides one bulk chunk of its
//! client's region (`config::BULK_CHUNK_BYTES`), so an edit inserting more than that is staged on the work's owner
//! first, in pages (`StageBegin`, `StagePut`), and then applied as one splice (`EditStaged`). One edit stays one
//! journal operation: splitting it into several edits would leave half of it applied if a later page failed.
//!
//! Bounds: every buffer's bytes, and a fixed charge for its record, are reserved from the shard's metadata budget
//! when it begins, whole or not at all, so the buffers a shard holds are bounded by that budget; an empty buffer is
//! refused (an empty insert fits one request). A buffer expires one lease after its last use and is released then,
//! by the shard's reaper or by the next staging call, whichever comes first, so an abandoned upload holds its
//! credit for at most a lease. Buffers are not durable: a daemon restart drops them, and the client stages again.
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
  len: u64,
  expires: u64,
  credit: MetadataCredit,
}

/// Derived: the fixed charge for one buffer's record beside its bytes, so a shard's buffer count is bounded by its
/// metadata budget even for small buffers: the record's own size and its expiry index entry.
const RECORD_BYTES: u64 = (size_of::<Buffer>() + size_of::<(u64, u64)>()) as u64;

/// A shard's staging buffers.
#[derive(Debug, Default)]
pub(crate) struct Staging {
  next: u64,
  buffers: BTreeMap<u64, Buffer>,
  /// Every buffer by expiry, so expiring reads only the expired.
  by_expiry: BTreeSet<(u64, u64)>,
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
        len,
        expires,
        credit,
      },
    );
    Ok(token)
  }

  /// Writes `bytes` at `offset` of buffer `token` for `work`, as `principal`, and moves its expiry to `expires`: how
  /// many bytes are written now. The bytes go where the previous put ended; a put repeating bytes already written
  /// (a retried request) is answered again without change; anything else is refused.
  pub(crate) fn put(
    &mut self,
    (work, principal, token): (VolumeId, &Principal, u64),
    offset: u64,
    bytes: &[u8],
    expires: u64,
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
    if offset != filled || end > buffer.len {
      return Err(Refusal::BadRequest {
        reason: format!(
          "a staging put at {offset} of {incoming} bytes; the buffer holds {filled} of {}",
          buffer.len
        ),
      });
    }
    buffer.bytes.extend_from_slice(bytes);
    let old = buffer.expires;
    buffer.expires = expires;
    self.by_expiry.remove(&(old, token));
    self.by_expiry.insert((expires, token));
    Ok(end)
  }

  /// The full bytes of buffer `token` for `work`, taken by `principal`: the buffer is released and its credit
  /// returned. Refused while the buffer is not yet full.
  pub(crate) fn take(
    &mut self,
    (work, principal, token): (VolumeId, &Principal, u64),
    budget: &mut MetadataBudget,
  ) -> Result<Vec<u8>, Refusal> {
    let buffer = self.owned(work, principal, token)?;
    let filled = u64::try_from(buffer.bytes.len()).unwrap_or(u64::MAX);
    if filled != buffer.len {
      return Err(Refusal::BadRequest {
        reason: format!(
          "the staging buffer holds {filled} of its {} bytes",
          buffer.len
        ),
      });
    }
    let buffer = self.remove(token).ok_or(Refusal::NotFound)?;
    budget.release(buffer.credit);
    Ok(buffer.bytes)
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
    self.by_expiry.remove(&(buffer.expires, token));
    Some(buffer)
  }
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
    Principal::Uid { uid: 501 }
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
    assert_eq!(staging.put(key, 0, b"hello ", LATER), Ok(6));
    misplaced_puts_are_refused(&mut staging, key, &mut budget);
    assert_eq!(staging.put(key, 6, b"world!", LATER), Ok(12));
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
      staging.put(key, 0, b"hello ", LATER),
      Ok(6),
      "a retried put"
    );
    assert!(
      staging.put(key, 0, b"HELLO ", LATER).is_err(),
      "different bytes"
    );
    assert!(
      staging.put(key, 9, b"xyz", LATER).is_err(),
      "skipping ahead"
    );
    assert!(
      staging.put(key, 6, b"world!!", LATER).is_err(),
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
    let stranger = Principal::Uid { uid: 1000 };
    assert_eq!(
      staging.put((work(), &stranger, token), 0, b"abcd", LATER),
      Err(Refusal::NotFound)
    );
    let other_work = VolumeId { bytes: [4; 16] };
    assert_eq!(
      staging.put((other_work, &owner(), token), 0, b"abcd", LATER),
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
      staging.put((work(), &owner(), token), 0, b"abcd", LATER),
      Err(Refusal::NotFound)
    );
  }
}
