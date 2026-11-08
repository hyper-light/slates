//! Request ids and the RIFL completion window: `(client, sequence)` names a request; the server
//! keeps each completion until the client acknowledges it by advancing its window, so a retry
//! returns the original result rather than re-executing [A: Lee et al., "Implementing
//! linearizability at large scale and low latency", SOSP'15] (§4.9 "Exactly-once").
//!
//! An acknowledgement is a watermark over every id the client issued and the few ids at or below it the client still
//! owes: awaited, or held for a resend under the same id. An owed id stays open, its record kept or yet to be made, and
//! every other id at or below the watermark is released. Before, the watermark stopped below the lowest owed id, so
//! one slow or held request kept every later completion of its client, and the server's bound on them refused the
//! client's next requests (`AcknowledgementOwed`: 906,317 against 59,646 served on the owner shard of a 1,000-client
//! `f = 1` rung, 2026-11-07). An open id is closed at most once and never reopened, so an acknowledgement that arrives
//! late can only close one, and a released id is never executed again.

use std::collections::{BTreeMap, BTreeSet};

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
  /// Run, and waiting for a version of a green to commit before it completes (§4.16 "Commit", A-252): never run again,
  /// answered that version once it commits.
  Awaiting {
    /// The green (a `VolumeId`'s bytes).
    object: [u8; 16],
    /// The version it waits for.
    version: u64,
  },
  /// Seen and already acknowledged by the client: a stale retry, refuse it.
  Acknowledged,
}

/// One client's completion window.
#[derive(Debug)]
pub struct ClientWindow<T> {
  acknowledged_up_to: Option<u32>,
  /// The ids at or below `acknowledged_up_to` the client still owes: their records are kept, or recorded when they
  /// complete.
  open: BTreeSet<u32>,
  completions: BTreeMap<u32, T>,
  /// The requests that ran and wait for a version to commit (A-252): each leaves when its completion is recorded or the
  /// client acknowledges it, and counts toward the window's retained entries, so the client's bound covers it.
  awaiting: BTreeMap<u32, ([u8; 16], u64)>,
}

impl<T> Default for ClientWindow<T> {
  fn default() -> Self {
    Self {
      acknowledged_up_to: None,
      open: BTreeSet::new(),
      completions: BTreeMap::new(),
      awaiting: BTreeMap::new(),
    }
  }
}

impl<T: Clone> ClientWindow<T> {
  /// Whether `sequence` was acknowledged: at or below the watermark and not owed.
  fn released(&self, sequence: u32) -> bool {
    self
      .acknowledged_up_to
      .is_some_and(|up_to| sequence <= up_to)
      && !self.open.contains(&sequence)
  }

  /// What the window knows about `sequence`.
  pub fn lookup(&self, sequence: u32) -> Seen<T> {
    if self.released(sequence) {
      return Seen::Acknowledged;
    }
    if let Some(result) = self.completions.get(&sequence) {
      return Seen::Completed(result.clone());
    }
    match self.awaiting.get(&sequence) {
      Some((object, version)) => Seen::Awaiting {
        object: *object,
        version: *version,
      },
      None => Seen::New,
    }
  }

  /// Records that `sequence` ran and waits for `version` of `object` to commit (A-252); one the client acknowledged is
  /// a stale retry the client can never ask for again, so it is not retained.
  pub fn await_version(&mut self, sequence: u32, object: [u8; 16], version: u64) {
    if self.released(sequence) {
      return;
    }
    self.awaiting.insert(sequence, (object, version));
  }

  /// Records a completion; one the client acknowledged is a stale retry the client can never ask for again, so it is
  /// not retained.
  pub fn record(&mut self, sequence: u32, result: T) {
    if self.released(sequence) {
      return;
    }
    self.awaiting.remove(&sequence);
    self.completions.insert(sequence, result);
  }

  /// The client acknowledges every sequence up to and including `up_to` except those in `owed`; the acknowledged
  /// completions are released, and whether it was applied is returned. An acknowledgement closes only what it speaks
  /// for (raft's rule): an open id at or below its `up_to` that it does not name is closed, an open id above its
  /// `up_to` is left as it is, an id above the old watermark is opened if named, and an id closed is never opened
  /// again. So an acknowledgement that arrives after a newer one can close ids it covers and reopen none. The watermark
  /// never moves back. One that names an owed id above its own `up_to` is malformed and ignored whole (the server
  /// refuses it before it is logged).
  pub fn acknowledge(&mut self, up_to: u32, owed: &[u32]) -> bool {
    if owed.iter().any(|sequence| *sequence > up_to) {
      return false;
    }
    let before = self.acknowledged_up_to;
    let watermark = before.map_or(up_to, |a| a.max(up_to));
    let named: BTreeSet<u32> = owed.iter().copied().collect();
    let kept = self
      .open
      .iter()
      .copied()
      .filter(|s| *s > up_to || named.contains(s));
    let opened = named
      .iter()
      .copied()
      .filter(|s| before.is_none_or(|a| *s > a));
    self.open = kept.chain(opened).collect();
    self.acknowledged_up_to = Some(watermark);
    let open = &self.open;
    self
      .completions
      .retain(|sequence, _| *sequence > watermark || open.contains(sequence));
    self
      .awaiting
      .retain(|sequence, _| *sequence > watermark || open.contains(sequence));
    true
  }

  /// Entries retained: completions and requests awaiting a version, each holding the client's bound.
  pub fn retained(&self) -> usize {
    self.completions.len().saturating_add(self.awaiting.len())
  }

  /// The highest sequence the client acknowledged, when it acknowledged any.
  pub fn acknowledged_up_to(&self) -> Option<u32> {
    self.acknowledged_up_to
  }

  /// The ids at or below the watermark the client still owes, in order.
  pub fn owed(&self) -> impl Iterator<Item = u32> + '_ {
    self.open.iter().copied()
  }

  /// The retained completions, by sequence (what a snapshot of the window carries).
  pub fn retained_entries(&self) -> impl Iterator<Item = (u32, &T)> {
    self.completions.iter().map(|(s, r)| (*s, r))
  }

  /// The requests awaiting a version: sequence, green, version (what a snapshot of the window carries).
  pub fn awaiting_entries(&self) -> impl Iterator<Item = (u32, [u8; 16], u64)> + '_ {
    self
      .awaiting
      .iter()
      .map(|(sequence, (object, version))| (*sequence, *object, *version))
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
    w.acknowledge(5, &[]);
    assert_eq!(w.lookup(5), Seen::Acknowledged);
    assert_eq!(w.lookup(6), Seen::Completed("second"));
    assert_eq!(w.retained(), 1);
    w.acknowledge(3, &[]);
    assert_eq!(
      w.lookup(6),
      Seen::Completed("second"),
      "an older ack never moves the window back"
    );
  }

  /// §4.9: an acknowledgement past an owed id releases every other record below its watermark, keeps the owed one
  /// answerable and recordable, and closes it once an acknowledgement no longer names it.
  #[test]
  fn an_owed_id_stays_answerable_while_every_other_record_below_the_watermark_is_released() {
    let mut w: ClientWindow<&str> = ClientWindow::default();
    for sequence in 1..=10 {
      if sequence != 3 {
        w.record(sequence, "done");
      }
    }
    w.acknowledge(10, &[3]);
    assert_eq!(w.retained(), 0, "every completed id released");
    assert_eq!(w.lookup(3), Seen::New, "the owed id is still to run");
    assert_eq!(w.lookup(4), Seen::Acknowledged);
    w.record(3, "late");
    assert_eq!(
      w.lookup(3),
      Seen::Completed("late"),
      "and its record is kept"
    );
    w.acknowledge(12, &[3]);
    assert_eq!(w.lookup(3), Seen::Completed("late"), "still owed");
    w.acknowledge(12, &[]);
    assert_eq!(
      w.lookup(3),
      Seen::Acknowledged,
      "closed once no longer named"
    );
    assert_eq!(w.retained(), 0);
  }

  /// §4.9: an acknowledgement that arrives after a newer one closes ids and reopens none, and never moves the
  /// watermark back.
  #[test]
  fn a_late_acknowledgement_reopens_nothing() {
    let mut w: ClientWindow<&str> = ClientWindow::default();
    w.acknowledge(8, &[2, 5]);
    w.acknowledge(12, &[5, 11]);
    assert_eq!(
      w.lookup(2),
      Seen::Acknowledged,
      "2 closed by the newer acknowledgement"
    );
    w.acknowledge(9, &[2, 5]);
    assert_eq!(
      w.lookup(2),
      Seen::Acknowledged,
      "the late one does not reopen 2"
    );
    assert_eq!(w.lookup(5), Seen::New, "5 still owed");
    assert_eq!(
      w.lookup(11),
      Seen::New,
      "11 is above the late one's watermark, untouched"
    );
    assert_eq!(w.acknowledged_up_to(), Some(12));
    w.acknowledge(9, &[]);
    assert_eq!(
      w.lookup(5),
      Seen::Acknowledged,
      "a late acknowledgement may close an id it covers"
    );
    assert_eq!(w.lookup(11), Seen::New);
  }

  /// §4.9 (raft): an acknowledgement that names an owed id above its own watermark is malformed and changes nothing.
  #[test]
  fn an_acknowledgement_naming_an_id_above_its_watermark_changes_nothing() {
    let mut w: ClientWindow<&str> = ClientWindow::default();
    w.acknowledge(12, &[11]);
    assert!(
      !w.acknowledge(20, &[11, 21]),
      "an owed id above the acknowledgement's own watermark is malformed"
    );
    assert_eq!(w.lookup(11), Seen::New, "the malformed one changed nothing");
    assert_eq!(w.lookup(15), Seen::New, "nor moved the watermark");
    assert_eq!(w.acknowledged_up_to(), Some(12));
  }

  /// §4.16 "Commit", A-252: a request awaiting a version is never `New`, its completion replaces the wait, and it counts
  /// toward the client's bound.
  #[test]
  fn a_request_awaiting_a_version_is_never_new_and_its_completion_replaces_the_wait() {
    let mut w: ClientWindow<&str> = ClientWindow::default();
    w.await_version(5, [7; 16], 3);
    assert_eq!(
      w.lookup(5),
      Seen::Awaiting {
        object: [7; 16],
        version: 3
      }
    );
    assert_eq!(w.retained(), 1, "a wait holds the client's bound");
    w.record(5, "version 3");
    assert_eq!(w.lookup(5), Seen::Completed("version 3"));
    assert_eq!(w.retained(), 1, "the completion replaced the wait");
    assert_eq!(w.awaiting_entries().count(), 0);
  }

  /// A-252: an acknowledgement releases a wait it covers and does not name, keeps one it names owed, and a wait on an id
  /// already released is not retained.
  #[test]
  fn an_acknowledgement_releases_a_wait_it_covers_and_keeps_one_it_names_owed() {
    let mut w: ClientWindow<&str> = ClientWindow::default();
    w.await_version(4, [1; 16], 1);
    w.await_version(5, [1; 16], 2);
    assert!(w.acknowledge(6, &[5]));
    assert_eq!(w.lookup(4), Seen::Acknowledged);
    assert_eq!(
      w.lookup(5),
      Seen::Awaiting {
        object: [1; 16],
        version: 2
      }
    );
    assert_eq!(w.awaiting_entries().count(), 1);
    w.await_version(3, [1; 16], 9);
    assert_eq!(
      w.lookup(3),
      Seen::Acknowledged,
      "a released id takes no wait"
    );
    assert_eq!(w.awaiting_entries().count(), 1);
  }
}
