//! Status paging's receiver (§4.7, §4.14): complete reports survive fragmentation, while
//! hostile lengths, identities and offsets refuse before decoding or unbounded allocation.
#![allow(clippy::unwrap_used, clippy::panic)]

use slates_ipc::IpcError;
use slates_ipc::protocol::{
  DaemonReport, FleetReport, GroupReport, Refusal, ReplyBody, RequestBody, encode_body,
};
use slates_ipc::status::collect;

fn report() -> ReplyBody {
  let group = GroupReport {
    leads: true,
    base_periods: 1,
    span_periods: 1,
    rtt_tail_ns: 0,
    rtt_spread_ns: 0,
    samples: 0,
  };
  ReplyBody::DaemonStatus {
    report: Box::new(DaemonReport {
      pid: 7,
      generation: 3,
      restarts: 2,
      heartbeat_age_ns: 17,
      clients_reaped: 11,
      clients_refused: 5,
      shards: Vec::new(),
      fleet: FleetReport {
        host: 19,
        f: 0,
        host_epoch: 23,
        members: vec![19, 29, 31],
        peers_probed: 2,
        unknown_id: 0,
        inbox_full: 0,
        sessions_refused: 0,
        replaced: 0,
        council: group.clone(),
        root: group,
      },
    }),
  }
}

fn page(snapshot: u64, offset: u64, total: u64, bytes: &[u8]) -> ReplyBody {
  ReplyBody::DaemonStatusPage {
    snapshot,
    offset,
    total,
    bytes: bytes.to_vec(),
  }
}

/// AC-2.6: every fragmentation, including single-byte pages, returns the exact report and
/// asks only for the next contiguous offset in the same capture.
#[test]
fn fragmented_status_reassembles_without_losing_fields() {
  let expected = report();
  let encoded = encode_body(&expected);
  for width in 1..=encoded.len() {
    let mut offset = 0;
    let actual = collect::<IpcError>(encoded.len(), |request| {
      if offset == 0 {
        assert_eq!(request, &RequestBody::DaemonStatus);
      } else {
        assert_eq!(
          request,
          &RequestBody::DaemonStatusNext {
            snapshot: 7,
            offset: offset as u64
          }
        );
      }
      let end = (offset + width).min(encoded.len());
      let reply = page(
        7,
        offset as u64,
        encoded.len() as u64,
        &encoded[offset..end],
      );
      offset = end;
      Ok(reply)
    })
    .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(offset, encoded.len());
  }
}

/// AC-2.6: a hostile page cannot make the receiver allocate past its credit, loop without
/// progress, combine captures, accept overlapping/gapped bytes, or decode another verb.
#[test]
fn malformed_pages_refuse_at_the_first_bad_page() {
  let encoded = encode_body(&report());
  let total = encoded.len() as u64;
  let first = page(7, 0, total, &encoded[..1]);
  let cases = vec![
    vec![page(7, 0, u64::MAX, &[0])],
    vec![page(7, 0, total, &[])],
    vec![page(7, 1, total, &encoded)],
    vec![page(7, 0, total - 1, &encoded)],
    vec![first.clone(), page(8, 1, total, &encoded[1..])],
    vec![first.clone(), page(7, 1, total + 1, &encoded[1..])],
    vec![first.clone(), page(7, 0, total, &encoded[1..])],
    vec![first.clone(), page(7, 2, total, &encoded[1..])],
    vec![first, page(7, 1, total, &[])],
    vec![page(7, 0, 1, &[0])],
    vec![ReplyBody::Acknowledged],
  ];
  for pages in cases {
    let mut pages = pages.into_iter();
    let result = collect::<IpcError>(encoded.len(), |_| Ok(pages.next().unwrap()));
    assert!(result.is_err(), "malformed sequence must refuse");
    assert!(
      pages.next().is_none(),
      "the bad page ends the exchange immediately"
    );
  }
  let wrong = encode_body(&ReplyBody::Acknowledged);
  assert!(
    collect::<IpcError>(wrong.len(), |_| Ok(page(7, 0, wrong.len() as u64, &wrong))).is_err()
  );
}

/// AC-2.6: an expired/cancelled capture's typed refusal reaches the caller unchanged.
#[test]
fn continuation_refusals_remain_typed() {
  let mut pages = [
    page(7, 0, 2, &[0]),
    ReplyBody::Refused {
      refusal: Refusal::NotFound,
    },
  ]
  .into_iter();
  assert_eq!(
    collect::<IpcError>(2, |_| Ok(pages.next().unwrap())).unwrap(),
    ReplyBody::Refused {
      refusal: Refusal::NotFound
    }
  );
}
