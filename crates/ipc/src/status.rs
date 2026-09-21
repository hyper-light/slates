//! Bounded daemon-status paging (§4.7, §4.14). A report is one immutable, schema-checked
//! message split into slot-sized pages. The receiver admits at most its reply ring's bulk
//! credit, checks identity and contiguous progress, and only then decodes the complete
//! report. A one-core/1-GiB fleet produced 5,888 bytes for a 4,096-byte slot on 2026-09-20;
//! paging preserves that report without enlarging every client's per-slot allocation.

use crate::protocol::{ReplyBody, RequestBody, decode_body, encode_body};
use crate::{ClientRegion, IpcError};

/// The status snapshot's byte bound: the already admitted bulk credit of the reply ring.
pub fn snapshot_capacity(region: &ClientRegion) -> usize {
  region.bulk().len() / 2
}

/// What a page holds past its framing and cursor, derived using the actual wire encoder.
pub fn page_capacity(region: &ClientRegion) -> usize {
  let chunk = snapshot_capacity(region) / region.cmd().slots().max(1);
  let header = encode_body(&ReplyBody::DaemonStatusPage {
    snapshot: 0,
    offset: 0,
    total: 0,
    bytes: Vec::new(),
  });
  chunk.saturating_sub(header.len())
}

/// Reads one complete status through the caller's existing request path. Every continuation
/// adds at least one byte toward the admitted total, so the number of calls is bounded by
/// `capacity`; there is no retry loop or second transport. A daemon refusal stays typed.
pub fn collect<E: From<IpcError>>(
  capacity: usize,
  mut exchange: impl FnMut(&RequestBody) -> Result<ReplyBody, E>,
) -> Result<ReplyBody, E> {
  let mut request = RequestBody::DaemonStatus;
  let mut identity = None;
  let mut payload = Vec::new();
  loop {
    let reply = exchange(&request)?;
    if matches!(reply, ReplyBody::Refused { .. }) {
      return Ok(reply);
    }
    let ReplyBody::DaemonStatusPage {
      snapshot,
      offset,
      total,
      bytes,
    } = reply
    else {
      return Err(
        IpcError::BadSlot {
          reason: "status reply is not a page",
        }
        .into(),
      );
    };
    let total_bytes = usize::try_from(total).map_err(|_| IpcError::BadSlot {
      reason: "status length does not fit this target",
    })?;
    if total_bytes > capacity {
      return Err(
        IpcError::PayloadTooLarge {
          offered: total_bytes,
          capacity,
        }
        .into(),
      );
    }
    if identity.is_some_and(|expected| expected != (snapshot, total))
      || usize::try_from(offset).ok() != Some(payload.len())
      || bytes.is_empty()
      || bytes.len() > total_bytes.saturating_sub(payload.len())
    {
      return Err(
        IpcError::BadSlot {
          reason: "status page changed identity, overlapped, skipped or made no progress",
        }
        .into(),
      );
    }
    if identity.is_none() {
      payload = Vec::with_capacity(total_bytes);
      identity = Some((snapshot, total));
    }
    payload.extend_from_slice(&bytes);
    if payload.len() == total_bytes {
      let reply = decode_body::<ReplyBody>(&payload)?;
      return if matches!(reply, ReplyBody::DaemonStatus { .. }) {
        Ok(reply)
      } else {
        Err(
          IpcError::BadSlot {
            reason: "status pages did not contain a daemon report",
          }
          .into(),
        )
      };
    }
    request = RequestBody::DaemonStatusNext {
      snapshot,
      offset: u64::try_from(payload.len()).map_err(|_| IpcError::BadSlot {
        reason: "status offset does not fit the wire",
      })?,
    };
  }
}
