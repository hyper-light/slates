//! Request ids and the RIFL completion window: `(client, sequence)` names a request; the server
//! keeps each completion until the client acknowledges it by advancing its window, so a retry
//! returns the original result rather than re-executing [A: Lee et al., "Implementing
//! linearizability at large scale and low latency", SOSP'15] (§4.9 "Exactly-once").

use std::collections::BTreeMap;

/// A request id: the client's id and its per-client sequence, packed into the header's word.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestId {
  /// The client.
  pub client: u32,
  /// The sequence within the client.
  pub sequence: u32,
}

impl RequestId {
  /// The packed word: client in the high half, sequence in the low half.
  pub const fn word(self) -> u64 {
    ((self.client as u64) << u32::BITS) | self.sequence as u64
  }

  /// From the packed word.
  pub fn from_word(word: u64) -> RequestId {
    RequestId {
      client: u32::try_from(word >> u32::BITS).unwrap_or(u32::MAX),
      sequence: u32::try_from(word & u64::from(u32::MAX)).unwrap_or(u32::MAX),
    }
  }
}

/// What a lookup says about a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seen<T> {
  /// Never seen: execute it.
  New,
  /// Seen and completed: return the retained result.
  Completed(T),
  /// Seen and already acknowledged by the client: a stale retry, refuse it.
  Acknowledged,
}

/// One client's completion window.
#[derive(Debug)]
pub struct ClientWindow<T> {
  acknowledged_up_to: Option<u32>,
  completions: BTreeMap<u32, T>,
}

impl<T> Default for ClientWindow<T> {
  fn default() -> Self {
    Self {
      acknowledged_up_to: None,
      completions: BTreeMap::new(),
    }
  }
}

impl<T: Clone> ClientWindow<T> {
  /// What the window knows about `sequence`.
  pub fn lookup(&self, sequence: u32) -> Seen<T> {
    if self
      .acknowledged_up_to
      .is_some_and(|up_to| sequence <= up_to)
    {
      return Seen::Acknowledged;
    }
    match self.completions.get(&sequence) {
      Some(result) => Seen::Completed(result.clone()),
      None => Seen::New,
    }
  }

  /// Records a completion.
  pub fn record(&mut self, sequence: u32, result: T) {
    self.completions.insert(sequence, result);
  }

  /// The client acknowledges every sequence up to and including `up_to`; their completions are
  /// released.
  pub fn acknowledge(&mut self, up_to: u32) {
    self.acknowledged_up_to = Some(self.acknowledged_up_to.map_or(up_to, |a| a.max(up_to)));
    self.completions = self.completions.split_off(&up_to.saturating_add(1));
  }

  /// Completions retained.
  pub fn retained(&self) -> usize {
    self.completions.len()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn ids_pack_and_unpack() {
    let id = RequestId {
      client: 0xABCD,
      sequence: 77,
    };
    assert_eq!(RequestId::from_word(id.word()), id);
    assert_eq!(id.word() >> 32, 0xABCD);
  }

  #[test]
  fn a_retry_returns_the_original_result_until_acknowledged() {
    let mut w: ClientWindow<&str> = ClientWindow::default();
    assert_eq!(w.lookup(5), Seen::New);
    w.record(5, "created volume v1");
    assert_eq!(w.lookup(5), Seen::Completed("created volume v1"));
    w.record(6, "second");
    w.acknowledge(5);
    assert_eq!(w.lookup(5), Seen::Acknowledged);
    assert_eq!(w.lookup(6), Seen::Completed("second"));
    assert_eq!(w.retained(), 1);
    w.acknowledge(3);
    assert_eq!(
      w.lookup(6),
      Seen::Completed("second"),
      "an older ack never moves the window back"
    );
  }
}
