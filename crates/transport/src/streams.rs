//! The session plane's stream-id space (§4.10a; RFC 9000 §2.1 stream ids, §4.6 stream concurrency,
//! §19.11 `MAX_STREAMS`): which streams exist, which are closed, and how many the peer may open — pure,
//! sans-io, and bounded.
//!
//! **The id.** A stream id packs, low bits first, the request **kind** the server dispatches on
//! ([`KIND_BITS`]), the [`Priority`] class the sender schedules it in ([`CLASS_BITS`]), the **initiator**
//! (one bit: [`Role::Client`] or [`Role::Server`]), and a per-initiator **sequence** above. Each end
//! allocates its own sequences in order, from zero, and never reuses one (RFC 9000 §2.1), so both ends
//! can open exchanges on one session without their ids colliding, and a request and its reply share an id
//! (a bidirectional stream, RFC 9000 §2.1).
//!
//! **Closed is implicit and exact.** Because sequences are allocated in order, a stream this end opened
//! whose sequence is below the next one it would allocate, and that is no longer open, is closed; a
//! stream the peer opened whose sequence is below the highest it has used, and that is neither open nor
//! still awaited (a lower sequence reordered behind a higher one, RFC 9000 §3.2 "implicitly opened"), is
//! closed. A late copy of a closed stream's frame — a retransmission still in flight, a probe copy, a
//! network duplicate, arriving however late — is recognised as closed forever with no per-stream memory.
//! The time-pruned tombstone set this replaces forgot a closed stream after three probe timeouts, so a
//! copy arriving later (a burst loss holding the acknowledgements back past that) reopened it as a
//! partial stream that was never freed, and each one crept toward the concurrency limit.
//!
//! **Concurrency is credited.** A receiver holds state for at most [`StreamSpace::limit`] streams its
//! peer opened: the peer may open only sequences below this end's credit — the number of the peer's
//! streams this end has closed plus the limit — which rides every acknowledgement in a `MaxStreams`
//! frame, as the connection credit does. Both ends derive the same limit from the session's shape (R8),
//! so each starts with the other's credit already known and a `MaxStreams` frame only ever raises it. A
//! stream the sender opens past the credit waits, unsent, until credit arrives (a sender with such a
//! stream and nothing in flight reports `StreamsBlocked`, whose acknowledgement carries the credit); at
//! most one more limit's worth may wait before [`StreamSpace::open_local`] refuses typed. The credit is
//! never sent in an ack-eliciting packet of its own: a peer idle between exchanges acknowledges only when
//! it next reads, so such a packet's round trip spanned the idle gap and inflated the RTT estimate — a
//! server's probe timeout reached 6.6 s on a 100 ms path, and a lost reply cost 7 s (2026-09-28). Before this, a peer past the receiver's stream limit had its frames dropped **but its packets
//! acknowledged**, so the sender counted the data delivered and the exchange waited forever.
//!
//! **Closing takes both halves.** A stream closes when its receiving half is done (read through,
//! discarded, or reset by the peer) and its sending half is done (every byte acknowledged, or reset); only
//! then does it return credit, so a peer's request still awaiting this end's reply keeps its credit
//! spent — the receiver's pending work is the sender's backpressure.

use std::collections::{BTreeMap, BTreeSet};

/// A stream's priority class (the constrained-link design, research note §5.3): the sender fills each
/// packet from the most urgent class with data ready, so a membership probe or a record commit never waits
/// behind a bulk transfer's unsent bytes. Ordered most urgent first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
  /// Membership, fencing, registers: the fleet's control traffic.
  Control = 0,
  /// Lookups, attributes, small forwarded operations.
  Metadata = 1,
  /// Content transfers and archive shipping.
  Bulk = 2,
}

impl Priority {
  /// Every class, most urgent first.
  pub const ALL: [Priority; 3] = [Priority::Control, Priority::Metadata, Priority::Bulk];

  /// How many classes are more urgent than this one: the classes whose share of connection credit it leaves
  /// untouched (`crate::connection::class_credit_reserve`).
  pub fn classes_above(self) -> u64 {
    match self {
      Priority::Control => 0,
      Priority::Metadata => 1,
      Priority::Bulk => 2,
    }
  }
}

/// Which end of the session opened a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
  /// The end that dialed.
  Client = 0,
  /// The end that accepted.
  Server = 1,
}

/// Format: eight kind bits — the fleet names fourteen request kinds, 256 leaves room.
pub const KIND_BITS: u32 = 8;
/// Format: two class bits above the kind — three [`Priority`] classes.
pub const CLASS_BITS: u32 = 2;
/// Format: the initiator bit's position, above the class.
const INITIATOR_SHIFT: u32 = KIND_BITS + CLASS_BITS;
/// Format: the sequence's position, above the initiator bit.
const SEQUENCE_SHIFT: u32 = INITIATOR_SHIFT + 1;
/// Derived: the largest sequence an id carries — every bit above [`SEQUENCE_SHIFT`].
pub const MAX_SEQUENCE: u64 = u64::MAX >> SEQUENCE_SHIFT;
/// Derived: the mask of the kind bits.
const KIND_MASK: u64 = (1 << KIND_BITS) - 1;
/// Derived: the mask of the class bits once shifted down.
const CLASS_MASK: u64 = (1 << CLASS_BITS) - 1;

/// The id of the `sequence`-th stream `initiator` opens, of `kind` in class `priority`. Kind bits past
/// [`KIND_BITS`] are dropped; a sequence past [`MAX_SEQUENCE`] is never composed ([`StreamSpace`] refuses
/// it first).
pub fn compose(kind: u64, priority: Priority, initiator: Role, sequence: u64) -> u64 {
  (sequence << SEQUENCE_SHIFT)
    | ((initiator as u64) << INITIATOR_SHIFT)
    | ((priority as u64) << KIND_BITS)
    | (kind & KIND_MASK)
}

/// The request kind a stream id carries.
pub fn kind(stream_id: u64) -> u64 {
  stream_id & KIND_MASK
}

/// The priority class a stream id carries; the unused fourth class value reads as the least urgent.
pub fn priority(stream_id: u64) -> Priority {
  match (stream_id >> KIND_BITS) & CLASS_MASK {
    0 => Priority::Control,
    1 => Priority::Metadata,
    _ => Priority::Bulk,
  }
}

/// Which end opened a stream.
pub fn initiator(stream_id: u64) -> Role {
  if (stream_id >> INITIATOR_SHIFT) & 1 == 0 {
    Role::Client
  } else {
    Role::Server
  }
}

/// The stream's sequence among its initiator's streams.
pub fn sequence(stream_id: u64) -> u64 {
  stream_id >> SEQUENCE_SHIFT
}

/// Why a stream could not be opened or replied on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamRefusal {
  /// A limit's worth of this end's streams already waits past the peer's credit; the caller must let some
  /// finish first.
  Backlogged {
    /// The streams waiting past the credit.
    waiting: u64,
    /// The most that may wait.
    limit: u64,
  },
  /// Every sequence an id can carry has been used; the session must be replaced.
  SequencesExhausted,
  /// A reply was offered on a stream the peer never opened (a local id, or one the peer never sent).
  NotAPeerStream {
    /// The stream.
    stream_id: u64,
  },
  /// A reply was offered twice on one stream.
  AlreadyReplied {
    /// The stream.
    stream_id: u64,
  },
}

/// What a frame naming a stream meets on arrival.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrival {
  /// The stream's receiving half is open (it was, or this frame opened it): apply the frame.
  Open,
  /// The stream's receiving half is closed: a late copy, dropped.
  Closed,
  /// The peer broke the stream rules (a stream it may not open, a local stream never opened, or an id
  /// whose kind or class differs from the stream's first frame): dropped and counted.
  Violation,
}

/// Whether a reply may be sent on a peer's stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplyAdmission {
  /// The stream is open and awaits its reply: send it.
  Send,
  /// The stream is already closed (the peer stopped it or reset it): the reply is moot and dropped.
  Moot,
}

/// A stream's two halves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Halves {
  /// The id the stream was opened under (its kind and class bits must never change).
  stream_id: u64,
  /// The receiving half is done: read through, discarded, or reset by the peer.
  receive_done: bool,
  /// The sending half has begun (a local stream's request; a peer stream's reply).
  send_started: bool,
  /// The sending half is done: every byte acknowledged, or reset.
  send_done: bool,
}

/// How many streams each kind of state holds — the leak witness the tests assert returns to zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamCensus {
  /// Streams this end opened that are not yet closed.
  pub local_open: usize,
  /// Streams the peer opened that are not yet closed.
  pub peer_open: usize,
  /// Peer sequences below the highest seen that have not arrived yet.
  pub peer_awaited: usize,
}

/// The stream-id space of one end of a session (see the module doc).
#[derive(Debug)]
pub struct StreamSpace {
  role: Role,
  limit: u64,
  /// The next sequence this end allocates.
  local_next: u64,
  /// This end's open streams, by sequence.
  local_open: BTreeMap<u64, Halves>,
  /// The peer's credit: this end may send on its sequences below this.
  peer_credit: u64,
  /// One past the highest sequence of the peer's streams seen.
  peer_next: u64,
  /// The peer's open streams, by sequence.
  peer_open: BTreeMap<u64, Halves>,
  /// The peer's sequences below `peer_next` not yet seen (implicitly opened, RFC 9000 §3.2).
  peer_awaited: BTreeSet<u64>,
  /// How many of the peer's streams this end has closed.
  peer_closed: u64,
}

impl StreamSpace {
  /// The space of the end playing `role`, holding at most `limit` of the peer's streams at once (at least
  /// one). Both ends derive the same limit, so each starts with the other's credit already known.
  pub fn new(role: Role, limit: u64) -> StreamSpace {
    let limit = limit.max(1);
    StreamSpace {
      role,
      limit,
      local_next: 0,
      local_open: BTreeMap::new(),
      peer_credit: limit,
      peer_next: 0,
      peer_open: BTreeMap::new(),
      peer_awaited: BTreeSet::new(),
      peer_closed: 0,
    }
  }

  /// The most of the peer's streams held at once.
  pub fn limit(&self) -> u64 {
    self.limit
  }

  /// The role this end plays.
  pub fn role(&self) -> Role {
    self.role
  }

  /// Opens this end's next stream, of `kind` in class `priority`, returning its id. Refused when a
  /// limit's worth of streams already waits past the peer's credit, or when the sequences are exhausted.
  pub fn open_local(&mut self, kind: u64, priority: Priority) -> Result<u64, StreamRefusal> {
    let waiting = self.local_next.saturating_sub(self.peer_credit);
    if waiting >= self.limit {
      return Err(StreamRefusal::Backlogged {
        waiting,
        limit: self.limit,
      });
    }
    if self.local_next > MAX_SEQUENCE {
      return Err(StreamRefusal::SequencesExhausted);
    }
    let sequence = self.local_next;
    let stream_id = compose(kind, priority, self.role, sequence);
    self.local_open.insert(
      sequence,
      Halves {
        stream_id,
        receive_done: false,
        send_started: true,
        send_done: false,
      },
    );
    self.local_next = sequence.saturating_add(1);
    Ok(stream_id)
  }

  /// Whether this end may send on `stream_id` now: a peer's stream always (it is a reply), this end's
  /// own only once the peer's credit covers its sequence.
  pub fn sendable(&self, stream_id: u64) -> bool {
    initiator(stream_id) != self.role || sequence(stream_id) < self.peer_credit
  }

  /// What a frame naming `stream_id` meets, opening the peer's stream (and awaiting every lower sequence
  /// not yet seen) when it is the first frame of a stream within the credit.
  pub fn arrive(&mut self, stream_id: u64) -> Arrival {
    if initiator(stream_id) == self.role {
      self.arrive_local(stream_id)
    } else {
      self.arrive_peer(stream_id)
    }
  }

  /// A frame on one of this end's own streams (a reply): open, closed, or never opened.
  fn arrive_local(&self, stream_id: u64) -> Arrival {
    let sequence = sequence(stream_id);
    match self.local_open.get(&sequence) {
      Some(halves) => Self::open_state(halves, stream_id),
      None if sequence < self.local_next => Arrival::Closed,
      None => Arrival::Violation,
    }
  }

  /// What a frame for an open stream meets: its id must match the stream's first frame.
  fn open_state(halves: &Halves, stream_id: u64) -> Arrival {
    if halves.stream_id != stream_id {
      Arrival::Violation
    } else if halves.receive_done {
      Arrival::Closed
    } else {
      Arrival::Open
    }
  }

  /// A frame on one of the peer's streams: open, awaited (opened now), closed, or past the credit.
  fn arrive_peer(&mut self, stream_id: u64) -> Arrival {
    let sequence = sequence(stream_id);
    if let Some(halves) = self.peer_open.get(&sequence) {
      return Self::open_state(halves, stream_id);
    }
    if sequence < self.peer_next {
      if self.peer_awaited.remove(&sequence) {
        self.open_peer(sequence, stream_id);
        return Arrival::Open;
      }
      return Arrival::Closed;
    }
    if sequence >= self.credit() {
      return Arrival::Violation;
    }
    // Every sequence between the highest seen and this one is implicitly opened (RFC 9000 §3.2); the
    // credit bounds how many: `credit - peer_next <= limit`.
    for awaited in self.peer_next..sequence {
      self.peer_awaited.insert(awaited);
    }
    self.peer_next = sequence.saturating_add(1);
    self.open_peer(sequence, stream_id);
    Arrival::Open
  }

  fn open_peer(&mut self, sequence: u64, stream_id: u64) {
    self.peer_open.insert(
      sequence,
      Halves {
        stream_id,
        receive_done: false,
        send_started: false,
        send_done: false,
      },
    );
  }

  /// Whether a reply may be sent on the peer's stream `stream_id` (and records that it has begun).
  pub fn begin_reply(&mut self, stream_id: u64) -> Result<ReplyAdmission, StreamRefusal> {
    let sequence = sequence(stream_id);
    if initiator(stream_id) == self.role {
      return Err(StreamRefusal::NotAPeerStream { stream_id });
    }
    match self.peer_open.get_mut(&sequence) {
      Some(halves) if halves.stream_id != stream_id => {
        Err(StreamRefusal::NotAPeerStream { stream_id })
      }
      Some(halves) if halves.send_done => Ok(ReplyAdmission::Moot),
      Some(halves) if halves.send_started => Err(StreamRefusal::AlreadyReplied { stream_id }),
      Some(halves) => {
        halves.send_started = true;
        Ok(ReplyAdmission::Send)
      }
      None if sequence < self.peer_next && !self.peer_awaited.contains(&sequence) => {
        Ok(ReplyAdmission::Moot)
      }
      None => Err(StreamRefusal::NotAPeerStream { stream_id }),
    }
  }

  /// Whether `stream_id`'s receiving half is open here (a frame for it would be applied).
  pub fn receiving(&self, stream_id: u64) -> bool {
    self
      .halves(stream_id)
      .is_some_and(|halves| !halves.receive_done)
  }

  /// Whether `stream_id`'s sending half may still carry data: open, and not yet done.
  pub fn send_open(&self, stream_id: u64) -> bool {
    self
      .halves(stream_id)
      .is_some_and(|halves| !halves.send_done)
  }

  fn halves(&self, stream_id: u64) -> Option<&Halves> {
    let sequence = sequence(stream_id);
    let open = if initiator(stream_id) == self.role {
      &self.local_open
    } else {
      &self.peer_open
    };
    open
      .get(&sequence)
      .filter(|halves| halves.stream_id == stream_id)
  }

  /// Marks `stream_id`'s receiving half done; closes the stream if its sending half is done too.
  pub fn close_receive(&mut self, stream_id: u64) {
    self.update(stream_id, |halves| halves.receive_done = true);
  }

  /// Marks `stream_id`'s sending half done; closes the stream if its receiving half is done too.
  pub fn close_send(&mut self, stream_id: u64) {
    self.update(stream_id, |halves| halves.send_done = true);
  }

  fn update(&mut self, stream_id: u64, change: impl FnOnce(&mut Halves)) {
    let sequence = sequence(stream_id);
    let local = initiator(stream_id) == self.role;
    let open = if local {
      &mut self.local_open
    } else {
      &mut self.peer_open
    };
    let Some(halves) = open.get_mut(&sequence) else {
      return;
    };
    if halves.stream_id != stream_id {
      return;
    }
    change(halves);
    if halves.receive_done && halves.send_done {
      open.remove(&sequence);
      if !local {
        self.peer_closed = self.peer_closed.saturating_add(1);
      }
    }
  }

  /// The stream credit this end extends: the peer may open sequences below it — the number of the peer's
  /// streams this end has closed plus the limit. It only rises, and rides every acknowledgement.
  pub fn credit(&self) -> u64 {
    self.peer_closed.saturating_add(self.limit)
  }

  /// The peer's credit this end is blocked at: `Some(credit)` while one of this end's streams waits past
  /// it (RFC 9000 §19.14 `STREAMS_BLOCKED`), `None` otherwise.
  pub fn waiting_limit(&self) -> Option<u64> {
    (self.local_next > self.peer_credit).then_some(self.peer_credit)
  }

  /// Takes the peer's `MaxStreams` credit (it only ever rises; a stale, reordered one is ignored).
  pub fn on_max_streams(&mut self, max: u64) {
    self.peer_credit = self.peer_credit.max(max);
  }

  /// How many streams each kind of state holds.
  pub fn census(&self) -> StreamCensus {
    StreamCensus {
      local_open: self.local_open.len(),
      peer_open: self.peer_open.len(),
      peer_awaited: self.peer_awaited.len(),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use proptest::prelude::*;

  /// An id round-trips its four fields, and the two initiators' ids of one sequence never collide.
  #[test]
  fn an_id_carries_kind_class_initiator_and_sequence() {
    for &class in &Priority::ALL {
      for role in [Role::Client, Role::Server] {
        let id = compose(13, class, role, 42);
        assert_eq!(kind(id), 13);
        assert_eq!(priority(id), class);
        assert_eq!(initiator(id), role);
        assert_eq!(sequence(id), 42);
      }
    }
    assert_ne!(
      compose(1, Priority::Control, Role::Client, 0),
      compose(1, Priority::Control, Role::Server, 0)
    );
  }

  /// A server of limit two holding two client streams (the credit spent), and the ids of the client's
  /// first three streams; the third is refused past the credit, holding no state.
  fn a_server_at_its_credit() -> (StreamSpace, [u64; 3]) {
    let mut server = StreamSpace::new(Role::Server, 2);
    let ids = [0, 1, 2].map(|sequence| compose(1, Priority::Control, Role::Client, sequence));
    let [first, second, third] = ids;
    assert_eq!(server.arrive(first), Arrival::Open);
    assert_eq!(server.arrive(second), Arrival::Open);
    assert_eq!(server.arrive(third), Arrival::Violation, "past the credit");
    assert_eq!(server.census().peer_open, 2);
    (server, ids)
  }

  /// A peer that opens a stream beyond the credit is refused (the frame is a violation, holding no state),
  /// and once this end closes one of the peer's streams its credit rises, the next sequence is admitted.
  #[test]
  fn the_credit_bounds_the_peers_open_streams_and_closing_raises_it() {
    let (mut server, [first, _, third]) = a_server_at_its_credit();
    server.close_receive(first);
    assert_eq!(server.credit(), 2, "the reply is still owed");
    assert_eq!(server.begin_reply(first), Ok(ReplyAdmission::Send));
    server.close_send(first);
    assert_eq!(server.credit(), 3, "closing the stream raised the credit");
    assert_eq!(server.arrive(third), Arrival::Open);
    assert_eq!(server.arrive(first), Arrival::Closed, "a late copy");
  }

  /// A lower sequence reordered behind a higher one is awaited, then opened when it arrives; a closed one
  /// stays closed however late a copy arrives.
  #[test]
  fn a_reordered_lower_stream_is_awaited_then_opened() {
    let mut server = StreamSpace::new(Role::Server, 4);
    let low = compose(2, Priority::Bulk, Role::Client, 0);
    let high = compose(2, Priority::Bulk, Role::Client, 2);
    assert_eq!(server.arrive(high), Arrival::Open);
    assert_eq!(server.census().peer_awaited, 2);
    assert_eq!(server.arrive(low), Arrival::Open);
    assert_eq!(server.census().peer_awaited, 1);
  }

  /// This end's streams past the peer's credit wait unsendable; a limit's worth may wait, then opening
  /// refuses typed; the peer's `MaxStreams` makes them sendable.
  #[test]
  fn local_streams_past_the_credit_wait_then_open_refuses() {
    let mut client = StreamSpace::new(Role::Client, 2);
    let ids: Vec<u64> = (0..4)
      .map(|_| client.open_local(1, Priority::Metadata).unwrap())
      .collect();
    assert_eq!(
      ids
        .iter()
        .map(|&id| client.sendable(id))
        .collect::<Vec<_>>(),
      vec![true, true, false, false]
    );
    assert_eq!(
      client.open_local(1, Priority::Metadata),
      Err(StreamRefusal::Backlogged {
        waiting: 2,
        limit: 2
      })
    );
    assert_eq!(
      client.waiting_limit(),
      Some(2),
      "blocked at the peer's credit"
    );
    client.on_max_streams(3);
    client.on_max_streams(1);
    assert_eq!(
      client.waiting_limit(),
      Some(3),
      "still one past the raised credit"
    );
    assert!(ids.get(2).is_some_and(|&id| client.sendable(id)));
    assert!(ids.get(3).is_some_and(|&id| !client.sendable(id)));
    client.on_max_streams(4);
    assert_eq!(
      client.waiting_limit(),
      None,
      "every stream is within the credit"
    );
  }

  /// A frame on a local stream never opened, or with a kind differing from the stream's first frame, is a
  /// violation; a reply on a local id is refused.
  #[test]
  fn forged_ids_are_violations() {
    let mut client = StreamSpace::new(Role::Client, 2);
    let opened = client.open_local(3, Priority::Control).unwrap();
    assert_eq!(client.arrive(opened), Arrival::Open);
    assert_eq!(
      client.arrive(compose(3, Priority::Control, Role::Client, 7)),
      Arrival::Violation,
      "never opened"
    );
    assert_eq!(
      client.arrive(compose(4, Priority::Control, Role::Client, 0)),
      Arrival::Violation,
      "a different kind under an opened sequence"
    );
    assert_eq!(
      client.begin_reply(opened),
      Err(StreamRefusal::NotAPeerStream { stream_id: opened })
    );
  }

  /// One step of the model-based test.
  #[derive(Clone, Debug)]
  enum Step {
    /// The peer's frame for its stream of this sequence.
    Arrive(u64),
    /// This end closes the receiving half of the peer's stream of this sequence.
    CloseReceive(u64),
    /// This end replies on the peer's stream of this sequence and the reply completes.
    Reply(u64),
  }

  fn step() -> impl Strategy<Value = Step> {
    // A tuple of (which step, which sequence), not `prop_oneof!` (it boxes its arms in an `Arc`, R2).
    (0u8..3, 0u64..24).prop_map(|(which, sequence)| match which {
      0 => Step::Arrive(sequence),
      1 => Step::CloseReceive(sequence),
      _ => Step::Reply(sequence),
    })
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]
    /// The oracle (T-transport streams): the space against a serial model that remembers every stream the
    /// peer ever opened and closed, over generated histories. The space must agree on every arrival, never
    /// hold more than the limit of the peer's streams (open plus awaited), and remember a closed stream as
    /// closed forever with bounded state.
    #[test]
    fn the_space_equals_a_model_that_remembers_everything(
      limit in 1u64..6,
      steps in proptest::collection::vec(step(), 1..200),
    ) {
      let mut space = StreamSpace::new(Role::Server, limit);
      // The model: every sequence ever opened, those whose halves are done, and the credit.
      let mut opened: BTreeSet<u64> = BTreeSet::new();
      let mut receive_done: BTreeSet<u64> = BTreeSet::new();
      let mut send_done: BTreeSet<u64> = BTreeSet::new();
      let id = |sequence: u64| compose(1, Priority::Metadata, Role::Client, sequence);
      for step in steps {
        match step {
          Step::Arrive(sequence) => {
            let closed = receive_done.contains(&sequence) && send_done.contains(&sequence);
            let credit = limit + (opened.iter().filter(|s| receive_done.contains(s) && send_done.contains(s)).count() as u64);
            let expected = if opened.contains(&sequence) && !receive_done.contains(&sequence) {
              Arrival::Open
            } else if opened.contains(&sequence) || closed {
              Arrival::Closed
            } else if sequence < credit {
              Arrival::Open
            } else {
              Arrival::Violation
            };
            // (A never-seen sequence below the highest seen is awaited, so it opens like any in-credit one.)
            prop_assert_eq!(space.arrive(id(sequence)), expected);
            if expected == Arrival::Open {
              opened.insert(sequence);
            }
          }
          Step::CloseReceive(sequence) => {
            if opened.contains(&sequence) {
              receive_done.insert(sequence);
            }
            space.close_receive(id(sequence));
          }
          Step::Reply(sequence) => {
            let admission = space.begin_reply(id(sequence));
            if opened.contains(&sequence) && !send_done.contains(&sequence) {
              prop_assert_eq!(admission, Ok(ReplyAdmission::Send));
              send_done.insert(sequence);
              space.close_send(id(sequence));
            }
          }
        }
        let census = space.census();
        prop_assert!((census.peer_open + census.peer_awaited) as u64 <= limit);
        let model_open = opened.iter().filter(|s| !(receive_done.contains(s) && send_done.contains(s))).count();
        prop_assert_eq!(census.peer_open, model_open);
      }
    }
  }
}
