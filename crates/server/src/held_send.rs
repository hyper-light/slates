//! Whether a held NFS connection a dead daemon left can be served on by its successor, judged from the send it had in
//! flight (§4.6, the loopback listener; A-113's Linux half; A-550).
//!
//! The anchor holds each loopback connection across a restart, so a restart is a slow reply and never a dropped
//! connection (A-113). That rests on one fact: no reply is ever torn on the wire. A request leaves the kernel only once
//! its reply is in it, so a successor answers every request still queued; a reply the dead daemon sent whole may be sent
//! again, and an ONC RPC client drops a reply whose xid it no longer awaits (RFC 5531). A reply cut part-way would
//! instead hand the client the next reply's bytes as the rest of the torn one.
//!
//! macOS makes every send all or nothing (`SO_SNDLOWAT` at a record's size, A-113). Linux keeps `SO_SNDLOWAT` fixed and
//! copies what fits, so a send can end mid-record, and Linux connections have not been held. The remedy A-113 named is an
//! exact check at adoption: the bytes the kernel says the connection's writer handed it (`tcpi_bytes_acked` plus
//! `SIOCOUTQ`) against a ledger of the send in flight the anchor holds, the bytes sent whole before it and its own
//! length. This module is that check's judgment ([`resume`]), pure and held by its test on every host; the ledger's
//! writes and the kernel's counters are the Linux plumbing around it.

/// A held connection's send ledger as the daemon wrote it before its last send (A-550): the bytes its replies had put in
/// the kernel whole before that send, and that send's length (zero when none was in flight).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SendLedger {
  /// The bytes of whole replies the kernel took before the send in flight.
  pub(crate) committed: u64,
  /// The length of the send in flight, a whole reply record; zero when none was.
  pub(crate) in_flight: u64,
}

/// How a held connection's stream stands at its adoption ([`resume`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Resume {
  /// The send in flight put nothing in the kernel: the successor answers its request afresh.
  Untouched,
  /// The send in flight put all of its reply in the kernel: the successor may send it again, which the client drops.
  Whole,
  /// The kernel holds part of the send in flight, or a count the ledger cannot account for: the stream is torn, so the
  /// connection ends and its client reconnects.
  Torn,
}

/// Judges a held connection at its adoption (A-550): `written`, the bytes the kernel says its writer handed it, against
/// the `ledger` of the send that was in flight. Exactly the bytes before that send, or exactly those and the whole send,
/// leaves the stream on a record's boundary and the connection may be served on; any other count is a torn stream.
pub(crate) fn resume(written: u64, ledger: SendLedger) -> Resume {
  if written == ledger.committed {
    return Resume::Untouched;
  }
  match ledger.committed.checked_add(ledger.in_flight) {
    Some(whole) if ledger.in_flight > 0 && written == whole => Resume::Whole,
    _ => Resume::Torn,
  }
}

#[cfg(test)]
mod tests {
  use super::{Resume, SendLedger, resume};

  /// §4.6, A-113's Linux half, A-550: a held connection is served on only when its stream stands on a record's boundary.
  /// Do: judge a connection whose kernel holds the bytes before the send in flight, those and the whole send, part of
  /// it, fewer bytes than the ledger committed, more than the send, a ledger with nothing in flight, and one whose sum
  /// would overflow. Expect `Untouched` and `Whole` on the two boundaries and `Torn` everywhere else. Until A-550 no Linux
  /// connection was judged: none was held, so every restart dropped every Linux client's connection.
  #[test]
  fn a_held_connection_is_served_on_only_on_a_records_boundary() {
    let sending = SendLedger {
      committed: 1_000,
      in_flight: 300,
    };
    assert_eq!(
      resume(1_000, sending),
      Resume::Untouched,
      "nothing of the send"
    );
    assert_eq!(resume(1_300, sending), Resume::Whole, "the whole send");
    assert_eq!(resume(1_150, sending), Resume::Torn, "part of the send");
    assert_eq!(resume(999, sending), Resume::Torn, "fewer than committed");
    assert_eq!(resume(1_301, sending), Resume::Torn, "more than the send");
    let idle = SendLedger {
      committed: 1_000,
      in_flight: 0,
    };
    assert_eq!(resume(1_000, idle), Resume::Untouched, "nothing in flight");
    assert_eq!(
      resume(1_001, idle),
      Resume::Torn,
      "bytes no send accounts for"
    );
    let overflowing = SendLedger {
      committed: u64::MAX,
      in_flight: 1,
    };
    assert_eq!(
      resume(0, overflowing),
      Resume::Torn,
      "a sum past the counter's range"
    );
  }
}
