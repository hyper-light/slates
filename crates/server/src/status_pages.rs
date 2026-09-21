//! Client-owned status snapshots (§4.7, §4.14). The 2026-09-20 one-core fleet regression
//! produced a 5,888-byte report for a 4,096-byte reply slot. Pages preserve that observation
//! under the client's existing bulk credit, with retained bytes charged to the metadata
//! ledger. A new operation, completion, expiry or disconnect returns the credit.

use slates_ipc::protocol::{Refusal, ReplyBody, encode_body};
use slates_mem::budget::{MetadataBudget, MetadataCredit};

/// At most one capture, including a scatter still awaiting its other shards, per client.
#[derive(Default)]
pub(crate) struct StatusPages {
  pending: Option<(u64, u64)>,
  held: Option<Held>,
}

struct Held {
  bytes: Vec<u8>,
  credit: MetadataCredit,
}

impl StatusPages {
  /// Starts a capture; a late result from the replaced scatter cannot install its bytes.
  pub(crate) fn begin(&mut self, request: u64, expires: u64, budget: &mut MetadataBudget) {
    self.clear(budget);
    self.pending = Some((request, expires));
  }

  /// Returns all retained credit, including on cancellation or removal of the client.
  pub(crate) fn clear(&mut self, budget: &mut MetadataBudget) {
    if let Some(held) = self.held.take() {
      budget.release(held.credit);
    }
    self.pending = None;
  }

  /// The absolute capture deadline bounds abandoned state even while the client stays alive.
  pub(crate) fn expire(&mut self, now: u64, budget: &mut MetadataBudget) {
    if self.pending.is_some_and(|(_, expires)| now >= expires) {
      self.clear(budget);
    }
  }

  /// Freezes the report once. Retained allocation capacity, rather than just wire length,
  /// is admitted, and a report beyond the client's credit refuses without truncation.
  pub(crate) fn capture(
    &mut self,
    request: u64,
    reply: &ReplyBody,
    capacity: usize,
    budget: &mut MetadataBudget,
  ) -> Result<(), Refusal> {
    if !self
      .pending
      .is_some_and(|(expected, _)| expected == request)
    {
      return Err(Refusal::NotFound);
    }
    let bytes = encode_body(reply);
    if bytes.len() > capacity {
      self.clear(budget);
      return Err(Refusal::BudgetExceeded {
        available: u64::try_from(capacity).unwrap_or(u64::MAX),
      });
    }
    let charged = u64::try_from(bytes.capacity()).map_err(|_| Refusal::BudgetExceeded {
      available: budget.admittable(),
    })?;
    let credit = match budget.reserve(charged) {
      Ok(credit) => credit,
      Err(_) => {
        self.clear(budget);
        return Err(Refusal::BudgetExceeded {
          available: budget.admittable(),
        });
      }
    };
    if let Some(previous) = self.held.replace(Held { bytes, credit }) {
      budget.release(previous.credit);
    }
    Ok(())
  }

  /// Reads from this client's frozen capture. Invalid cursors do not destroy a valid capture.
  pub(crate) fn page(
    &mut self,
    request: u64,
    offset: u64,
    capacity: usize,
    now: u64,
    budget: &mut MetadataBudget,
  ) -> Result<ReplyBody, Refusal> {
    self.expire(now, budget);
    if !self
      .pending
      .is_some_and(|(expected, _)| expected == request)
    {
      return Err(Refusal::NotFound);
    }
    let held = self.held.as_ref().ok_or(Refusal::NotFound)?;
    let start = usize::try_from(offset).map_err(|_| bad_cursor())?;
    if start >= held.bytes.len() || capacity == 0 {
      return Err(bad_cursor());
    }
    let end = start.saturating_add(capacity).min(held.bytes.len());
    let total = u64::try_from(held.bytes.len()).map_err(|_| bad_cursor())?;
    let reply = ReplyBody::DaemonStatusPage {
      snapshot: request,
      offset,
      total,
      bytes: held.bytes[start..end].to_vec(),
    };
    if end == held.bytes.len() {
      self.clear(budget);
    }
    Ok(reply)
  }
}

fn bad_cursor() -> Refusal {
  Refusal::BadRequest {
    reason: "status cursor is outside the captured report or its page has no capacity".to_owned(),
  }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
  use super::*;

  /// AC-2.6: a frozen capture returns exact bytes across pages, and returns its metadata
  /// credit after completion, replacement, cancellation and the absolute expiry boundary.
  #[test]
  fn captured_bytes_survive_paging_and_release_their_credit() {
    let reply = ReplyBody::Acknowledged;
    let expected = encode_body(&reply);
    let charge = u64::try_from(expected.capacity()).unwrap();
    let mut budget = MetadataBudget::new(charge);
    let mut pages = StatusPages::default();
    pages.begin(7, 10, &mut budget);
    pages
      .capture(7, &reply, expected.len(), &mut budget)
      .unwrap();
    assert_eq!(
      budget.admittable(),
      0,
      "retained allocation is fully charged"
    );
    assert_eq!(pages.page(8, 0, 1, 0, &mut budget), Err(Refusal::NotFound));
    assert!(pages.page(7, u64::MAX, 1, 0, &mut budget).is_err());
    let mut received = Vec::new();
    for offset in 0..expected.len() {
      let ReplyBody::DaemonStatusPage { bytes, .. } =
        pages.page(7, offset as u64, 1, 0, &mut budget).unwrap()
      else {
        panic!("expected a page");
      };
      received.extend(bytes);
    }
    assert_eq!(received, expected);
    assert_eq!(budget.admittable(), charge);
    assert_eq!(pages.page(7, 0, 1, 0, &mut budget), Err(Refusal::NotFound));
  }

  /// AC-2.6: replacement, cancellation and expiry release retained bytes; a late scatter
  /// for the earlier capture cannot reinstall them.
  #[test]
  fn replaced_cancelled_and_expired_captures_release_their_credit() {
    let reply = ReplyBody::Acknowledged;
    let expected = encode_body(&reply);
    let charge = u64::try_from(expected.capacity()).unwrap();
    let mut budget = MetadataBudget::new(charge);
    let mut pages = StatusPages::default();
    for action in 0..3 {
      pages.begin(7, 10, &mut budget);
      pages
        .capture(7, &reply, expected.len(), &mut budget)
        .unwrap();
      match action {
        0 => pages.begin(8, 10, &mut budget),
        1 => pages.clear(&mut budget),
        _ => {
          pages.expire(9, &mut budget);
          assert_eq!(budget.admittable(), 0);
          pages.expire(10, &mut budget);
        }
      }
      assert_eq!(budget.admittable(), charge);
      assert_eq!(
        pages.capture(7, &reply, expected.len(), &mut budget),
        Err(Refusal::NotFound)
      );
    }
  }

  /// AC-2.6: both the channel credit and metadata class refuse a capture without leaking
  /// a reservation; a later affordable capture still works.
  #[test]
  fn refused_capture_leaves_credit_available() {
    let reply = ReplyBody::Acknowledged;
    let encoded = encode_body(&reply);
    for (credit, capacity) in [
      (encoded.capacity() - 1, encoded.len()),
      (encoded.capacity(), encoded.len() - 1),
    ] {
      let mut budget = MetadataBudget::new(credit as u64);
      let mut pages = StatusPages::default();
      pages.begin(7, 10, &mut budget);
      assert!(matches!(
        pages.capture(7, &reply, capacity, &mut budget),
        Err(Refusal::BudgetExceeded { .. })
      ));
      assert_eq!(budget.admittable(), credit as u64);
    }
  }
}
