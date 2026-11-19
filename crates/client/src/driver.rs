//! The async client's driver (§4.7, R6; AUD-29-19, AUD-29-20): the one state machine both SDKs' event loops
//! drive, so admission, deadlines, cancellation and recovery are written once and every binding keeps only
//! its loop's plumbing — a reader on the completion descriptor, a timer, and its own futures.
//!
//! Every operation here returns without waiting. A call is **submitted** and gets a [`Ticket`] at once: it is
//! sent when the command ring has a slot and the client admits it, and **queued** otherwise (a full ring, the
//! outstanding bound, a lost channel, a channel still being bound) — the queue is bounded too, and a call
//! past it is refused `TooManyOutstanding`. The binding calls [`Driver::pump`] when the completion descriptor
//! is readable and [`Driver::tick`] at [`Driver::next_wake_ns`]; both hand back [`Event`]s: a reply landed
//! for a ticket (the binding decodes it with its verb's poll and calls [`Driver::finish`]), or the call
//! failed, typed.
//!
//! **Every call ends (AUD-29-20), and waits while its daemon lives (A-232).** The client's one rule, which the
//! synchronous round trip and its wait for a ring slot keep too (`crate::client`): a call, sent and unanswered or queued
//! unsent, asks the daemon's liveness on A-443's doubling (`slates_ipc::looks`, A-523): first once it has waited twice
//! the longer of its wait when the driver first judged it and the client's wake estimate, then at twice the time waited.
//! The loop learns of a reply from the completion descriptor but of nothing about the daemon's departure, so it asks;
//! the looks cost the logarithm of the wait, and a departure is seen within the time already waited. A live daemon's
//! call goes on waiting: a daemon answers every call it is given, a long one (a granted landing, a forward across the
//! fleet) and a starved one alike, and a daemon judged stuck is ended by its supervisor, or by its own watchdog when it
//! runs alone, which this client then sees as gone. Until A-523 a call asked at every reply deadline, the anchor's
//! liveness budget (a fixed second), so a call whose daemon died waited up to that second to learn it.
//! A gone daemon starts recovery — single reconnect steps, a claim polled as an async connect polls it, the
//! channel bound to the consumer without a blocking round trip, then every call still awaited resent under its own id
//! (the daemon answers a call it already served from its completion record) and the queue admitted. Recovery goes on
//! while the instance's rendezvous has a holder, its anchor restarting the daemon; with none every call fails
//! `DaemonGone` at once (A-468). A binding whose reader fails, or whose descriptor closes, fails
//! every call with [`Driver::fail_all`]; a cancelled call is released at once with [`Driver::cancel`]. Nothing is left
//! waiting on a reply that cannot come.
//!
//! Until A-232 (2026-11-13) a live daemon's overdue call failed `Stalled`, and only a call submitted patient
//! (`submit_patient`, a landing) or one whose request the daemon may defer (`defers_reply`) waited. A daemon at
//! `cpu.weight` 1 beside spinners, alive by its own watchdog, had calls failed `Stalled` in 10 of 10 runs on both of
//! CI's Linux runners (`docs/bugs/2026-11-13-a-live-starved-daemons-call-was-failed-stalled.md`); the second path is
//! gone with the failure.
//!
//! Until 2026-10-01 `begin` spun on a full ring until the reply deadline and could reconnect through a
//! parking loop on the event loop's thread, and the SDKs had no deadline, no failure path and no cancel: a
//! call whose daemon died stayed pending for good.

use std::collections::{BTreeMap, VecDeque};
use std::time::Instant;

use slates_ipc::looks::Looks;

use crate::{Client, ClientError, RequestId};

/// How a call is begun: a verb's typed begin over the client (`Client::create_begin` and the rest), kept
/// while the call is queued so it is begun once the client admits it.
pub type Begin = Box<dyn FnMut(&mut Client) -> Result<RequestId, ClientError>>;

/// A submitted call's handle, stable from submission to its end, whether it was sent at once or queued.
pub type Ticket = u64;

/// What the driver tells its binding.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
  /// The reply to `ticket`, sent as `word`, is in the client's buffer: decode it with the verb's poll by
  /// `word`, then [`Driver::finish`] the ticket.
  Ready {
    /// The call.
    ticket: Ticket,
    /// Its request id word.
    word: u64,
  },
  /// The call ended without a reply.
  Failed {
    /// The call.
    ticket: Ticket,
    /// Why.
    error: ClientError,
  },
}

/// One call the driver owns until it ends.
struct Call {
  /// How to begin it while it waits in the queue; spent once sent (a resend uses the body the client kept).
  begin: Option<Begin>,
  /// Its request id word once sent; `None` while queued.
  word: Option<u64>,
  /// When its wait began: when it was submitted, last sent, or continued.
  armed: Instant,
  /// When it next asks the daemon's liveness, from `armed` ([`Looks`], A-523): fixed at the first tick that judges it,
  /// each later look at twice the time waited.
  looks: Looks,
  /// Whether it must be sent again under its id on the current channel (after a reconnect).
  resend: bool,
  /// Whether its reply was reported [`Event::Ready`] and the binding has not finished it yet: the client
  /// holds a reply until it is taken, so a pump would otherwise report it again — twice in one loop step when
  /// the step's tick pumps after its pump, the second event finding the reply already taken.
  reported: bool,
}

impl Call {
  /// Starts the call's wait again at `now`: its clock, and a look schedule the next tick fixes afresh.
  fn rearm(&mut self, now: Instant) {
    self.armed = now;
    self.looks = Looks::default();
  }
}

/// A lost channel's recovery.
struct Lost {
  next_attempt: Instant,
  pause_ns: u64,
}

/// The async client's driver (see the module doc).
pub struct Driver {
  calls: BTreeMap<Ticket, Call>,
  by_word: BTreeMap<u64, Ticket>,
  queued: VecDeque<Ticket>,
  queue_cap: usize,
  next_ticket: Ticket,
  lost: Option<Lost>,
}

fn elapsed_ns(since: Instant) -> u64 {
  u64::try_from(since.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// The calls whose look at the daemon's life is due now (A-523), a call `answered` this step and one awaiting its resend
/// excepted: each call's schedule ([`Looks`]) is fixed at the first judgment, `wake_ns` being the client's wake
/// estimate, so a call answered before twice the longer of that wait and the wake never asks.
fn looks_due(
  calls: &mut BTreeMap<Ticket, Call>,
  answered: &std::collections::BTreeSet<Ticket>,
  wake_ns: u64,
) -> Vec<Ticket> {
  calls
    .iter_mut()
    .filter(|(ticket, call)| !answered.contains(*ticket) && !call.resend)
    .filter_map(|(ticket, call)| {
      call
        .looks
        .due(elapsed_ns(call.armed), wake_ns)
        .then_some(*ticket)
    })
    .collect()
}

/// How long until the earliest look a call makes at the daemon's life (A-523), every call sent or queued: a schedule not
/// yet judged is read as its first judgment would fix it, and none is moved. `None` with no call.
fn next_look_ns(calls: &BTreeMap<Ticket, Call>, wake_ns: u64) -> Option<u64> {
  calls
    .values()
    .map(|call| call.looks.until_or_first(elapsed_ns(call.armed), wake_ns))
    .min()
}

/// Whether `error` means "not sent yet; try again once the channel can take it".
fn retryable(error: &ClientError) -> bool {
  matches!(
    error,
    ClientError::RingFull
      | ClientError::TooManyOutstanding { .. }
      | ClientError::ChannelLost
      | ClientError::Rebinding
  )
}

impl Driver {
  /// A driver for `client`. Derived: the queue holds as many calls as the client admits outstanding — one
  /// more ring's worth waiting behind the ring's worth in flight.
  pub fn new(client: &Client) -> Driver {
    Driver {
      calls: BTreeMap::new(),
      by_word: BTreeMap::new(),
      queued: VecDeque::new(),
      queue_cap: client.outstanding_limit(),
      next_ticket: 0,
      lost: None,
    }
  }

  /// Whether no call is in flight or queued (the binding then disarms and drops its reader and timer).
  pub fn is_idle(&self) -> bool {
    self.calls.is_empty()
  }

  /// The word a call was sent under, while it is in flight.
  pub fn word_of(&self, ticket: Ticket) -> Option<u64> {
    self.calls.get(&ticket).and_then(|call| call.word)
  }

  /// Submits `body`: sent at once when the client admits it, else queued. Refused `TooManyOutstanding` when
  /// the queue is full, or with the client's typed error when the call cannot be sent at all.
  pub fn submit(&mut self, client: &mut Client, mut begin: Begin) -> Result<Ticket, ClientError> {
    let ticket = self.next_ticket;
    let sent = if self.lost.is_none() && self.queued.is_empty() {
      match begin(client) {
        Ok(id) => Some(id.word()),
        Err(error) if retryable(&error) => {
          if error == ClientError::ChannelLost {
            self.start_recovery();
          }
          None
        }
        Err(error) => return Err(error),
      }
    } else {
      None
    };
    if sent.is_none() && self.queued.len() >= self.queue_cap {
      return Err(ClientError::TooManyOutstanding {
        limit: self.queue_cap,
      });
    }
    self.next_ticket = self.next_ticket.saturating_add(1);
    if let Some(word) = sent {
      self.by_word.insert(word, ticket);
    } else {
      self.queued.push_back(ticket);
    }
    self.calls.insert(
      ticket,
      Call {
        begin: if sent.is_none() { Some(begin) } else { None },
        word: sent,
        armed: Instant::now(),
        looks: Looks::default(),
        resend: false,
        reported: false,
      },
    );
    Ok(ticket)
  }

  /// Ends a call whose reply the binding has taken (by its verb's poll).
  pub fn finish(&mut self, ticket: Ticket) {
    if let Some(call) = self.calls.remove(&ticket)
      && let Some(word) = call.word
    {
      self.by_word.remove(&word);
    }
  }

  /// Keeps a call whose reply the binding took by its verb's poll and that is not over: a chained call (a listing's
  /// page, a large edit's step, a `changed_since` page) answered `None` while the client sent its next request under
  /// the same word. Its next reply is reported again, and its wait counts from now: each step looks at the daemon's
  /// life as a call does. Before 2026-11-09 a call was reported once until it was finished, so a chain's second reply
  /// was never reported and the call failed `Stalled` at the deadline.
  pub fn continued(&mut self, ticket: Ticket) {
    if let Some(call) = self.calls.get_mut(&ticket) {
      call.reported = false;
      call.rearm(Instant::now());
    }
  }

  /// Releases a call the caller no longer awaits (a cancelled future): queued, it is never sent; in flight,
  /// its reply is dropped when it comes.
  pub fn cancel(&mut self, client: &mut Client, ticket: Ticket) {
    let Some(call) = self.calls.remove(&ticket) else {
      return;
    };
    match call.word {
      Some(word) => {
        self.by_word.remove(&word);
        client.abandon(word);
      }
      None => self.queued.retain(|queued| *queued != ticket),
    }
  }

  /// Drains every landed reply (the completion descriptor is readable): a `Ready` per reply a call awaits,
  /// then the queue admitted into whatever the replies freed. A drain the channel refuses fails every call.
  pub fn pump(&mut self, client: &mut Client) -> Vec<Event> {
    let ready = match client.take_ready() {
      Ok(ready) => ready,
      Err(error) => return self.fail_all(client, &error),
    };
    // A ready call is reported once, until the binding finishes it (or a reconnect sends it again).
    let mut events: Vec<Event> = ready
      .into_iter()
      .filter_map(|word| {
        let ticket = *self.by_word.get(&word)?;
        let call = self.calls.get_mut(&ticket)?;
        if call.reported {
          return None;
        }
        call.reported = true;
        Some(Event::Ready { ticket, word })
      })
      .collect();
    self.resend_and_admit(client, &mut events);
    events
  }

  /// The timer's turn: recovery when the channel is lost, else the reply deadlines — an overdue call starts
  /// recovery when the daemon is gone, and has its deadline re-armed while the daemon lives — then the queue admitted.
  pub fn tick(&mut self, client: &mut Client) -> Vec<Event> {
    if self.lost.is_some() {
      let mut events = Vec::new();
      self.recover(client, &mut events);
      return events;
    }
    // Replies already on the ring are taken first: a deadline is judged only on a call whose reply has not
    // come, never on one whose readiness the binding has not reported yet.
    let mut events = self.pump(client);
    let answered: std::collections::BTreeSet<Ticket> = events
      .iter()
      .filter_map(|event| match event {
        Event::Ready { ticket, .. } => Some(*ticket),
        Event::Failed { .. } => None,
      })
      .collect();
    let (wake_ns, _) = client.wake_estimate();
    // Due: a call sent and unanswered, or one still queued (a ring that does not free while the daemon lives is waited
    // on, as the synchronous send waits on it), whose look at the daemon's life has come (A-523).
    let due = looks_due(&mut self.calls, &answered, wake_ns);
    if due.is_empty() {
      self.resend_and_admit(client, &mut events);
      return events;
    }
    if client.daemon_gone() {
      self.start_recovery();
      self.recover(client, &mut events);
      return events;
    }
    // The daemon lives: every due call goes on waiting, and looks again at twice the time it has waited (A-232, A-523).
    for ticket in due {
      if let Some(call) = self.calls.get_mut(&ticket) {
        call.looks.looked(elapsed_ns(call.armed));
      }
    }
    self.resend_and_admit(client, &mut events);
    events
  }

  /// When the binding should call [`tick`](Self::tick) next, in nanoseconds from now: the next reconnect
  /// attempt while recovering, else the earliest look a call makes at the daemon's life (A-523); `None` when idle.
  pub fn next_wake_ns(&self, client: &Client) -> Option<u64> {
    if self.calls.is_empty() {
      return None;
    }
    if let Some(lost) = &self.lost {
      return Some(
        u64::try_from(
          lost
            .next_attempt
            .saturating_duration_since(Instant::now())
            .as_nanos(),
        )
        .unwrap_or(u64::MAX),
      );
    }
    let (wake_ns, _) = client.wake_estimate();
    next_look_ns(&self.calls, wake_ns)
  }

  /// Fails every call with `error` (the binding's reader failed, or its descriptor closed): each in-flight
  /// call's reply is abandoned.
  pub fn fail_all(&mut self, client: &mut Client, error: &ClientError) -> Vec<Event> {
    let calls = std::mem::take(&mut self.calls);
    self.by_word.clear();
    self.queued.clear();
    self.lost = None;
    calls
      .into_iter()
      .map(|(ticket, call)| {
        if let Some(word) = call.word {
          client.abandon(word);
        }
        Event::Failed {
          ticket,
          error: error.clone(),
        }
      })
      .collect()
  }

  /// Begins recovery of a lost channel, unless already recovering.
  fn start_recovery(&mut self) {
    if self.lost.is_none() {
      let now = Instant::now();
      self.lost = Some(Lost {
        next_attempt: now,
        pause_ns: 1,
      });
    }
  }

  /// One recovery step, when due: a single reconnect step (never waiting on the daemon); connected, every
  /// call in flight is marked to resend under its id, the queue's clocks restart, and the queue is
  /// admitted; the claim still due, or refused for now while the instance's rendezvous has a holder, the next step is
  /// paced as an async connect's polls are (doubling, at most a tenth of the claim wait); with no holder, every call
  /// fails `DaemonGone` (A-468).
  fn recover(&mut self, client: &mut Client, events: &mut Vec<Event>) {
    let Some(lost) = &self.lost else {
      return;
    };
    if Instant::now() < lost.next_attempt {
      return;
    }
    match client.try_reconnect() {
      Ok(true) => {
        self.lost = None;
        // The new channel restarts every call's clock: a sent call is resent under its id (its clock
        // restarts when it goes), and a queued one waits for admission on the new ring from now.
        let now = Instant::now();
        for call in self.calls.values_mut() {
          if call.word.is_some() {
            call.resend = true;
            call.reported = false;
          } else {
            call.rearm(now);
          }
        }
        self.resend_and_admit(client, events);
      }
      Ok(false) => {
        let pause_ns = crate::client::next_pause_ns(lost.pause_ns, slates_ipc::CLAIM_WAIT_NS);
        if let Some(lost) = self.lost.as_mut() {
          lost.pause_ns = pause_ns;
          let now = Instant::now();
          lost.next_attempt = now
            .checked_add(std::time::Duration::from_nanos(pause_ns))
            .unwrap_or(now);
        }
      }
      Err(error) => events.extend(self.fail_all(client, &error)),
    }
  }

  /// Sends again every call marked for it, then admits the queue, each while the channel takes them; a call
  /// the channel refuses for good fails.
  fn resend_and_admit(&mut self, client: &mut Client, events: &mut Vec<Event>) {
    if self.lost.is_some() {
      return;
    }
    let marked: Vec<Ticket> = self
      .calls
      .iter()
      .filter(|(_, call)| call.resend)
      .map(|(ticket, _)| *ticket)
      .collect();
    for ticket in marked {
      let Some(call) = self.calls.get(&ticket) else {
        continue;
      };
      let Some(word) = call.word else {
        continue;
      };
      match client.resend_awaited(word) {
        Ok(true) => {
          if let Some(call) = self.calls.get_mut(&ticket) {
            call.resend = false;
            call.rearm(Instant::now());
          }
        }
        Ok(false) => return,
        Err(error) if retryable(&error) => {
          if error == ClientError::ChannelLost {
            self.start_recovery();
          }
          return;
        }
        Err(error) => {
          self.calls.remove(&ticket);
          self.by_word.remove(&word);
          client.abandon(word);
          events.push(Event::Failed { ticket, error });
        }
      }
    }
    self.admit(client, events);
  }

  /// Sends queued calls in order while the client admits them.
  fn admit(&mut self, client: &mut Client, events: &mut Vec<Event>) {
    while let Some(&ticket) = self.queued.front() {
      let Some(begin) = self
        .calls
        .get_mut(&ticket)
        .and_then(|call| call.begin.as_mut())
      else {
        self.queued.pop_front();
        continue;
      };
      match begin(client) {
        Ok(id) => {
          self.queued.pop_front();
          self.by_word.insert(id.word(), ticket);
          if let Some(call) = self.calls.get_mut(&ticket) {
            call.word = Some(id.word());
            call.rearm(Instant::now());
            call.begin = None;
          }
        }
        Err(error) if retryable(&error) => {
          if error == ClientError::ChannelLost {
            self.start_recovery();
          }
          return;
        }
        Err(error) => {
          self.queued.pop_front();
          self.calls.remove(&ticket);
          events.push(Event::Failed { ticket, error });
        }
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use std::collections::{BTreeMap, BTreeSet};
  use std::time::{Duration, Instant};

  use slates_ipc::looks::{Looks, first_look_ns};

  use super::{Call, Ticket, looks_due, next_look_ns};

  /// Shape: the client's wake estimate the schedule is judged with: 3 µs, a wake measured on Apple silicon.
  const WAKE_NS: u64 = 3_000;
  /// Shape: how long the call has waited when the driver's first tick judges it: 30 ms, far inside the reply deadline.
  const FIRST_JUDGED_NS: u64 = 30_000_000;
  /// Shape: the reply deadline the driver first asked at before A-523: the anchor's liveness budget, a second.
  const OLD_REPLY_DEADLINE_NS: u64 = 1_000_000_000;

  /// A call sent `waited_ns` ago, its schedule not yet judged.
  fn call_waiting(waited_ns: u64) -> Call {
    Call {
      begin: None,
      word: Some(1),
      armed: Instant::now()
        .checked_sub(Duration::from_nanos(waited_ns))
        .unwrap(),
      looks: Looks::default(),
      resend: false,
      reported: false,
    }
  }

  /// Moves `call`'s wait back by `ns`, as if that much more time had passed.
  fn age(call: &mut Call, ns: u64) {
    call.armed = call.armed.checked_sub(Duration::from_nanos(ns)).unwrap();
  }

  /// A-523 (R3's class C). Do: hold one call sent 30 ms ago; ask the driver's timer when it next wakes, tick, age the
  /// call to the look the tick fixed and tick again, then record the look and ask again. Expect: the timer wakes about
  /// the call's own wait from now (its first look at twice it), far before the old reply deadline; no look due at the
  /// first tick; the look due once the call has waited twice its first judgment's time; and the next look at twice the
  /// time waited. Before A-523 the timer named the old reply deadline's remainder and nothing was due before it.
  #[test]
  fn a_call_looks_at_the_daemons_life_at_twice_its_wait_not_at_the_old_reply_deadline() {
    let ticket: Ticket = 1;
    let mut calls = BTreeMap::from([(ticket, call_waiting(FIRST_JUDGED_NS))]);
    let answered = BTreeSet::new();
    let timer = next_look_ns(&calls, WAKE_NS).unwrap();
    assert!(
      (FIRST_JUDGED_NS..OLD_REPLY_DEADLINE_NS / 10).contains(&timer),
      "the timer wakes {timer} ns on, the call's own wait, not the old deadline's remainder"
    );
    assert!(
      looks_due(&mut calls, &answered, WAKE_NS).is_empty(),
      "no look at the first judgment"
    );
    let until = next_look_ns(&calls, WAKE_NS).unwrap();
    age(calls.get_mut(&ticket).unwrap(), until);
    assert_eq!(
      looks_due(&mut calls, &answered, WAKE_NS),
      vec![ticket],
      "the look is due"
    );
    let call = calls.get_mut(&ticket).unwrap();
    let waited = u64::try_from(call.armed.elapsed().as_nanos()).unwrap();
    assert!(waited >= first_look_ns(FIRST_JUDGED_NS, WAKE_NS));
    call.looks.looked(waited);
    let next = next_look_ns(&calls, WAKE_NS).unwrap();
    assert!(
      next <= waited && next + WAKE_NS >= waited / 2,
      "the next look at twice the wait: {next} ns after {waited} ns"
    );
  }

  /// A-523. Do: hold a call answered this step and one awaiting its resend, both past any look, then an empty driver.
  /// Expect: neither is due (an answered call is done, a resent one's clock restarts when it goes), and an empty driver
  /// names no wake.
  #[test]
  fn an_answered_or_resending_call_never_looks_and_no_call_names_no_wake() {
    let mut calls = BTreeMap::from([
      (1, call_waiting(OLD_REPLY_DEADLINE_NS)),
      (2, call_waiting(OLD_REPLY_DEADLINE_NS)),
    ]);
    calls.get_mut(&2).unwrap().resend = true;
    let answered = BTreeSet::from([1]);
    assert!(looks_due(&mut calls, &answered, WAKE_NS).is_empty());
    assert_eq!(next_look_ns(&BTreeMap::new(), WAKE_NS), None);
  }
}
