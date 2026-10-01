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
//! **Every call ends (AUD-29-20).** A reply overdue past the client's reply deadline asks the daemon's
//! liveness once: a live daemon's silence fails the call `Stalled` (the synchronous path's rule), a gone
//! daemon starts recovery — single reconnect attempts paced as the synchronous reconnect paces them, the
//! channel bound to the consumer without a blocking round trip, then every call still awaited resent under
//! its own id (the daemon answers a call it already served from its completion record) and the queue
//! admitted. Past the reconnect budget every call fails `DaemonGone`. A binding whose reader fails, or whose
//! descriptor closes, fails every call with [`Driver::fail_all`]; a cancelled call is released with
//! [`Driver::cancel`]. Nothing is left waiting on a reply that cannot come.
//!
//! Until 2026-10-01 `begin` spun on a full ring until the reply deadline and could reconnect through a
//! parking loop on the event loop's thread, and the SDKs had no deadline, no failure path and no cancel: a
//! call whose daemon died stayed pending for good.

use std::collections::{BTreeMap, VecDeque};
use std::time::Instant;

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
  /// When it was last sent, the base of its reply deadline.
  sent: Instant,
  /// Whether it must be sent again under its id on the current channel (after a reconnect).
  resend: bool,
  /// Whether it is waited for while the daemon lives ([`crate::defers_reply`]): its deadline asks
  /// the daemon's liveness and restarts, never fails it `Stalled`.
  patient: bool,
  /// Whether its reply was reported [`Event::Ready`] and the binding has not finished it yet: the client
  /// holds a reply until it is taken, so a pump would otherwise report it again — twice in one loop step when
  /// the step's tick pumps after its pump, the second event finding the reply already taken.
  reported: bool,
}

/// A lost channel's recovery.
struct Lost {
  since: Instant,
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
  pub fn submit(&mut self, client: &mut Client, begin: Begin) -> Result<Ticket, ClientError> {
    self.submit_with(client, begin, false)
  }

  /// [`submit`](Self::submit) for a call whose reply the daemon defers until long work ends (a granted
  /// landing): once sent it is waited for while the daemon lives, its deadline only asking the liveness.
  pub fn submit_patient(
    &mut self,
    client: &mut Client,
    begin: Begin,
  ) -> Result<Ticket, ClientError> {
    self.submit_with(client, begin, true)
  }

  fn submit_with(
    &mut self,
    client: &mut Client,
    mut begin: Begin,
    patient: bool,
  ) -> Result<Ticket, ClientError> {
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
        sent: Instant::now(),
        resend: false,
        reported: false,
        patient,
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

  /// The timer's turn: recovery when the channel is lost, else the reply deadlines — an overdue call fails
  /// `Stalled` while the daemon lives, and starts recovery when it is gone — then the queue admitted.
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
    let reply_ns = client.deadlines().reply_ns;
    // Overdue: a call sent and unanswered past the reply deadline, or one still queued that long (a ring
    // that never frees is a stall too, as the synchronous `begin` finds it after the same deadline).
    let overdue: Vec<(Ticket, Option<u64>, bool)> = self
      .calls
      .iter()
      .filter(|(ticket, call)| {
        !answered.contains(ticket) && !call.resend && elapsed_ns(call.sent) >= reply_ns
      })
      .map(|(ticket, call)| (*ticket, call.word, call.patient && call.word.is_some()))
      .collect();
    if !overdue.is_empty() && client.daemon_gone() {
      self.start_recovery();
      self.recover(client, &mut events);
      return events;
    }
    for (ticket, word, patient) in overdue {
      if patient {
        // Its daemon lives: the deferred reply is still being worked for; the next deadline asks again.
        if let Some(call) = self.calls.get_mut(&ticket) {
          call.sent = Instant::now();
        }
        continue;
      }
      self.calls.remove(&ticket);
      match word {
        Some(word) => {
          self.by_word.remove(&word);
          client.abandon(word);
        }
        None => self.queued.retain(|queued| *queued != ticket),
      }
      events.push(Event::Failed {
        ticket,
        error: ClientError::Stalled { after_ns: reply_ns },
      });
    }
    self.resend_and_admit(client, &mut events);
    events
  }

  /// When the binding should call [`tick`](Self::tick) next, in nanoseconds from now: the next reconnect
  /// attempt while recovering, else the earliest reply deadline; `None` when idle.
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
    let reply_ns = client.deadlines().reply_ns;
    // Every call — sent or queued — has a reply deadline from when it was sent or submitted.
    self
      .calls
      .values()
      .map(|call| reply_ns.saturating_sub(elapsed_ns(call.sent)))
      .min()
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
        since: now,
        next_attempt: now,
        pause_ns: 1,
      });
    }
  }

  /// One recovery step, when due: a single reconnect step (never waiting on the daemon); connected, every
  /// call in flight is marked to resend under its id, the queue's clocks restart, and the queue is
  /// admitted; still no daemon, the next attempt is paced (doubling, at most a tenth of the budget, as the
  /// synchronous reconnect paces); past the reconnect budget, every call fails `DaemonGone`.
  fn recover(&mut self, client: &mut Client, events: &mut Vec<Event>) {
    let Some(lost) = &self.lost else {
      return;
    };
    if Instant::now() < lost.next_attempt {
      return;
    }
    let budget = client.deadlines().reconnect_ns;
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
            call.sent = now;
          }
        }
        self.resend_and_admit(client, events);
      }
      Ok(false) if elapsed_ns(lost.since) < budget => {
        let pause_ns = crate::client::next_pause_ns(lost.pause_ns, budget);
        if let Some(lost) = self.lost.as_mut() {
          lost.pause_ns = pause_ns;
          let now = Instant::now();
          lost.next_attempt = now
            .checked_add(std::time::Duration::from_nanos(pause_ns))
            .unwrap_or(now);
        }
      }
      Ok(false) => {
        events.extend(self.fail_all(client, &ClientError::DaemonGone { after_ns: budget }))
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
            call.sent = Instant::now();
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
            call.sent = Instant::now();
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
