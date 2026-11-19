//! One SSH connection served whole: the transport, user authentication and the session channel (A-633; RFC 4253,
//! RFC 4252, RFC 4254; §4.17.2 "Transports" item 2). Sans-io as the transport is: the caller passes the client's bytes
//! and the time, reads [`SessionEvent`]s, serves the command an [`SessionEvent::Exec`] names, and writes
//! [`Session::pending_output`] to the socket.
//!
//! **Order.** The transport's events drive the rest: the first exchange's session id starts authentication
//! ([`crate::userauth`]); messages 50 to 79 go to it, 80 to 127 to the channel layer ([`crate::channel`]) once a
//! principal is authenticated, and before that are refused. A number neither layer recognizes is answered
//! `SSH_MSG_UNIMPLEMENTED` with its sequence number (RFC 4253 §11.4); one the server itself sends is refused.
//!
//! **Sending.** Every message the session sends, replies and data alike, goes through one queue in order, flushed as
//! far as the transport takes it: not while the server's own key exchange is open, since "it MUST NOT send any messages
//! other than" the exchange's \[B: RFC 4253 §7.1\], and not past a full output. The queue holds at most about a packet
//! cap of bytes: reading stops while it is full, and [`Session::send_data`] takes only what fits, so a client that does
//! not read cannot grow it. [`SessionEvent::Writable`] says when a send that was held back may go again.
//!
//! **Refusals.** A refusal of any layer closes the connection with its own disconnect reason
//! ([`SessionRefusal::reason`]): a protocol error, a failed key exchange, a MAC error, no such service, or no more
//! authentication methods. A misuse by the caller ([`SessionUse`]) closes nothing.

use std::collections::VecDeque;

use crate::channel::{
  CHANNEL_FAILURE, ChannelConfig, ChannelEvent, ChannelRefusal, ChannelUse, Connection,
  GLOBAL_REQUEST, max_data,
};
use crate::command::Command;
use crate::entropy::Entropy;
use crate::hostkey::HostKey;
use crate::kex::HASH_LEN;
use crate::refusal::{DisconnectReason, Refusal};
use crate::transport::{
  ConfigRefusal, Event, Extension, SendRefusal, ServerTransport, TransportConfig,
};
use crate::userauth::{
  AuthRefusal, Authenticated, Authenticator, KeyHolders, SERVER_SIG_ALGS, USERAUTH_BANNER,
  USERAUTH_FAILURE, USERAUTH_PK_OK, USERAUTH_REQUEST, USERAUTH_SUCCESS, server_sig_algs,
};

/// What a session is set up with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionConfig {
  /// The transport's; the session adds `server-sig-algs` to its extensions.
  pub transport: TransportConfig,
  /// The text sent once before authentication (RFC 4252 §5.4), if any.
  pub banner: Option<String>,
  /// The failed authentication attempts allowed before the server disconnects (RFC 4252 §4 recommends
  /// [`crate::userauth::RECOMMENDED_FAILURE_LIMIT`]).
  pub failure_limit: u32,
  /// The client bytes the session channel may hold unconsumed, from the caller's budget.
  pub channel_window: u32,
}

/// Why a session could not be set up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionConfigRefusal {
  /// The transport's own refusal.
  Transport(ConfigRefusal),
  /// A channel window of nothing, under which a client could never send.
  EmptyWindow,
}

/// What the session tells its caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionEvent<P> {
  /// A principal authenticated.
  Authenticated(Authenticated<P>),
  /// The client asked for a command; the caller answers with [`Session::answer_exec`].
  Exec {
    /// The command.
    command: Command,
    /// The `GIT_PROTOCOL` value passed before it, if any.
    git_protocol: Option<Vec<u8>>,
  },
  /// The client's standard input; the caller gives back what it consumed with [`Session::consumed`].
  Data(Vec<u8>),
  /// The client will send no more data.
  Eof,
  /// The channel is closed both ways.
  ChannelClosed,
  /// A send held back by a key exchange, a full output or a shut window may go again.
  Writable,
  /// The client disconnected.
  Disconnected {
    /// Its reason code.
    reason: u32,
    /// Its description.
    description: String,
  },
}

/// Why the session ended the connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionRefusal {
  /// The transport's refusal.
  Transport(Refusal),
  /// Authentication's refusal.
  Auth(AuthRefusal),
  /// The channel layer's refusal.
  Channel(ChannelRefusal),
  /// A message of a layer that may not speak yet, or one only the server sends.
  Unexpected {
    /// The message's number.
    number: u8,
  },
  /// The transport refused a message the session built (a refusal that names a defect, never a client's doing).
  Send(SendRefusal),
}

impl SessionRefusal {
  /// The reason the server's disconnect carries.
  pub fn reason(&self) -> DisconnectReason {
    match self {
      SessionRefusal::Transport(refusal) => refusal.reason(),
      SessionRefusal::Auth(AuthRefusal::ServiceNotAvailable) => {
        DisconnectReason::ServiceNotAvailable
      }
      SessionRefusal::Auth(AuthRefusal::TooManyFailures { .. }) => {
        DisconnectReason::NoMoreAuthMethodsAvailable
      }
      SessionRefusal::Send(_) => DisconnectReason::ByApplication,
      SessionRefusal::Auth(AuthRefusal::Malformed(_))
      | SessionRefusal::Channel(_)
      | SessionRefusal::Unexpected { .. } => DisconnectReason::ProtocolError,
    }
  }

  /// The description the server's disconnect carries.
  pub fn description(&self) -> &'static str {
    match self {
      SessionRefusal::Transport(refusal) => refusal.description(),
      SessionRefusal::Auth(AuthRefusal::Malformed(_)) => "malformed authentication request",
      SessionRefusal::Auth(AuthRefusal::ServiceNotAvailable) => "service not available",
      SessionRefusal::Auth(AuthRefusal::TooManyFailures { .. }) => {
        "too many authentication failures"
      }
      SessionRefusal::Channel(ChannelRefusal::WindowExceeded) => "channel window exceeded",
      SessionRefusal::Channel(ChannelRefusal::PacketTooLarge) => "channel packet too large",
      SessionRefusal::Channel(_) => "channel protocol error",
      SessionRefusal::Unexpected { .. } => "unexpected message",
      SessionRefusal::Send(_) => "internal error",
    }
  }
}

/// A call the session could not take; only [`SessionUse::Fatal`] has closed the connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionUse {
  /// The channel's own misuse.
  Channel(ChannelUse),
  /// The connection is closed.
  Closed,
  /// The call ended the connection.
  Fatal(SessionRefusal),
}

/// One connection, the server's side.
pub struct Session<E: Entropy, H: KeyHolders> {
  /// The transport.
  transport: ServerTransport<E>,
  /// The key lookup.
  holders: H,
  /// The banner, until authentication starts.
  banner: Option<String>,
  /// The failures allowed.
  failure_limit: u32,
  /// Authentication, once the session id is known.
  authenticator: Option<Authenticator<H::Principal>>,
  /// Whether a principal authenticated.
  authenticated: bool,
  /// The channel layer.
  connection: Connection,
  /// Messages to send, in order.
  outbox: VecDeque<Vec<u8>>,
  /// Their bytes.
  outbox_bytes: usize,
  /// The most bytes the queue is let to hold before reading stops: the transport's packet cap.
  outbox_cap: usize,
  /// The event waiting for the caller.
  pending: Option<SessionEvent<H::Principal>>,
  /// Whether a send was held back.
  send_blocked: bool,
  /// Whether [`SessionEvent::Writable`] is owed once the waiting event is taken.
  writable_owed: bool,
  /// The time last given.
  now_ns: u64,
}

impl<E: Entropy, H: KeyHolders> core::fmt::Debug for Session<E, H> {
  fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    formatter
      .debug_struct("Session")
      .field("transport", &self.transport)
      .field("authenticated", &self.authenticated)
      .field("connection", &self.connection)
      .field("outbox_bytes", &self.outbox_bytes)
      .finish()
  }
}

impl<E: Entropy, H: KeyHolders> Session<E, H> {
  /// A session for a new connection.
  pub fn new(
    config: SessionConfig,
    host_key: HostKey,
    entropy: E,
    holders: H,
    now_ns: u64,
  ) -> Result<Self, SessionConfigRefusal> {
    if config.channel_window == 0 {
      return Err(SessionConfigRefusal::EmptyWindow);
    }
    let packet_cap = config.transport.packet_cap;
    let mut transport_config = config.transport;
    transport_config.extensions.push(Extension {
      name: SERVER_SIG_ALGS.to_string(),
      value: server_sig_algs(),
    });
    let transport = ServerTransport::new(transport_config, host_key, entropy, now_ns)
      .map_err(SessionConfigRefusal::Transport)?;
    let channel = ChannelConfig {
      window: config.channel_window,
      max_packet: max_data(packet_cap),
    };
    Ok(Self {
      transport,
      holders,
      banner: config.banner,
      failure_limit: config.failure_limit,
      authenticator: None,
      authenticated: false,
      connection: Connection::new(channel),
      outbox: VecDeque::new(),
      outbox_bytes: 0,
      outbox_cap: packet_cap,
      pending: None,
      send_blocked: false,
      writable_owed: false,
      now_ns,
    })
  }

  /// Reads the client's bytes: how many were taken. Fewer than all means an event waits or the output is full.
  pub fn receive(&mut self, input: &[u8], now_ns: u64) -> Result<usize, SessionRefusal> {
    self.now_ns = now_ns;
    let mut consumed = 0usize;
    loop {
      self.flush()?;
      if self.blocked() {
        break;
      }
      if let Some(event) = self.transport.poll_event() {
        self.on_transport_event(event)?;
        continue;
      }
      let rest = input.get(consumed..).unwrap_or(&[]);
      if rest.is_empty() {
        break;
      }
      let used = self
        .transport
        .receive(rest, now_ns)
        .map_err(SessionRefusal::Transport)?;
      consumed = consumed.saturating_add(used);
      if used == 0 && !self.take_transport_event()? {
        break;
      }
    }
    Ok(consumed)
  }

  /// The event waiting, if any.
  pub fn poll_event(&mut self) -> Option<SessionEvent<H::Principal>> {
    let event = self.pending.take();
    if event.is_some() && self.writable_owed {
      self.writable_owed = false;
      self.pending = Some(SessionEvent::Writable);
    }
    event
  }

  /// Bytes to write to the socket.
  pub fn pending_output(&self) -> &[u8] {
    self.transport.pending_output()
  }

  /// Gives back `count` written bytes, and sends what the full output held back.
  pub fn consume_output(&mut self, count: usize) -> Result<(), SessionRefusal> {
    self.transport.consume_output(count);
    let was_blocked = self.send_blocked;
    self.send_blocked = false;
    self.flush()?;
    if was_blocked && !self.send_blocked && self.connection.send_window() > 0 {
      self.raise_writable();
    } else if was_blocked {
      self.send_blocked = true;
    }
    Ok(())
  }

  /// Starts a key exchange if the key time has run out.
  pub fn poll_timers(&mut self, now_ns: u64) -> Result<(), SessionRefusal> {
    self.now_ns = now_ns;
    self
      .transport
      .poll_timers(now_ns)
      .map_err(SessionRefusal::Transport)?;
    self.flush()
  }

  /// When [`Session::poll_timers`] would start an exchange.
  pub fn next_timer_ns(&self) -> Option<u64> {
    self.transport.next_timer_ns()
  }

  /// Sets how long a key serves.
  pub fn set_key_time_ns(&mut self, key_time_ns: u64) {
    self.transport.set_key_time_ns(key_time_ns);
  }

  /// Whether the connection is closed.
  pub fn is_closed(&self) -> bool {
    self.transport.is_closed()
  }

  /// The session id, once the first exchange hash is known.
  pub fn session_id(&self) -> Option<&[u8; HASH_LEN]> {
    self.transport.session_id()
  }

  /// The transport, to read its state.
  pub fn transport(&self) -> &ServerTransport<E> {
    &self.transport
  }

  /// What the server may still send on the channel.
  pub fn send_window(&self) -> u64 {
    self.connection.send_window()
  }

  /// Answers the raised [`SessionEvent::Exec`]: the command runs when `accept`.
  pub fn answer_exec(&mut self, accept: bool) -> Result<(), SessionUse> {
    let replies = self
      .connection
      .answer_exec(accept)
      .map_err(SessionUse::Channel)?;
    self.queue_all(replies)
  }

  /// Gives back `bytes` of the data raised, as the caller consumed them.
  pub fn consumed(&mut self, bytes: usize) -> Result<(), SessionUse> {
    let adjust = self
      .connection
      .consumed(crate::to_u64(bytes))
      .map_err(SessionUse::Channel)?;
    self.queue_all(adjust.into_iter().collect())
  }

  /// Sends the command's standard output: how many bytes of `data` were taken, within the client's window and what
  /// the queue may hold.
  pub fn send_data(&mut self, data: &[u8]) -> Result<usize, SessionUse> {
    self.send_stream(data, false)
  }

  /// Sends the command's standard error, as [`Session::send_data`] does its output.
  pub fn send_stderr(&mut self, data: &[u8]) -> Result<usize, SessionUse> {
    self.send_stream(data, true)
  }

  /// Ends the channel: `exit_status`, EOF and close, in order.
  pub fn finish(&mut self, exit_status: u32) -> Result<(), SessionUse> {
    let messages = self
      .connection
      .finish(exit_status)
      .map_err(SessionUse::Channel)?;
    self.queue_all(messages)
  }

  /// Ends the connection with `reason` and `description`.
  pub fn close(&mut self, reason: DisconnectReason, description: &str) {
    self.transport.close(reason, description);
  }

  /// Queues data messages for `data`, standard error's when `stderr`.
  fn send_stream(&mut self, data: &[u8], stderr: bool) -> Result<usize, SessionUse> {
    if self.transport.is_closed() {
      return Err(SessionUse::Closed);
    }
    let room = self.outbox_cap.saturating_sub(self.outbox_bytes);
    let (taken, messages) = self
      .connection
      .send(data, room, stderr)
      .map_err(SessionUse::Channel)?;
    if taken < data.len() {
      self.send_blocked = true;
    }
    self.queue_all(messages)?;
    Ok(taken)
  }

  /// Queues `messages` and flushes what the transport takes.
  fn queue_all(&mut self, messages: Vec<Vec<u8>>) -> Result<(), SessionUse> {
    if self.transport.is_closed() {
      return Err(SessionUse::Closed);
    }
    for message in messages {
      self.queue(message);
    }
    self.flush().map_err(SessionUse::Fatal)
  }

  /// Queues one message.
  fn queue(&mut self, message: Vec<u8>) {
    self.outbox_bytes = self.outbox_bytes.saturating_add(message.len());
    self.outbox.push_back(message);
  }

  /// Sends queued messages as far as the transport takes them.
  fn flush(&mut self) -> Result<(), SessionRefusal> {
    while let Some(front) = self.outbox.front() {
      match self.transport.send(front, self.now_ns) {
        Ok(()) => {
          let sent = front.len();
          self.outbox.pop_front();
          self.outbox_bytes = self.outbox_bytes.saturating_sub(sent);
        }
        Err(SendRefusal::Rekeying | SendRefusal::OutputFull) => {
          self.send_blocked = true;
          return Ok(());
        }
        Err(SendRefusal::Closed) => {
          self.outbox.clear();
          self.outbox_bytes = 0;
          return Ok(());
        }
        Err(SendRefusal::Fatal(refusal)) => return Err(SessionRefusal::Transport(refusal)),
        Err(other) => return Err(self.fail(SessionRefusal::Send(other))),
      }
    }
    Ok(())
  }

  /// Whether reading must wait for the caller.
  fn blocked(&self) -> bool {
    self.pending.is_some() || self.outbox_bytes >= self.outbox_cap || self.transport.is_closed()
  }

  /// Takes and handles the transport's waiting event; whether there was one.
  fn take_transport_event(&mut self) -> Result<bool, SessionRefusal> {
    let Some(event) = self.transport.poll_event() else {
      return Ok(false);
    };
    self.on_transport_event(event)?;
    Ok(true)
  }

  /// Handles one of the transport's events.
  fn on_transport_event(&mut self, event: Event) -> Result<(), SessionRefusal> {
    match event {
      Event::Established(established) => {
        let banner = self.banner.take();
        self.authenticator = Some(Authenticator::new(
          established.session_id,
          banner,
          self.failure_limit,
        ));
      }
      Event::Rekeyed(_) => {
        if self.send_blocked {
          self.raise_writable();
        }
      }
      Event::ServiceAccepted => {}
      Event::Message { payload, sequence } => self.on_message(&payload, sequence)?,
      Event::Disconnected {
        reason,
        description,
      } => {
        self.pending = Some(SessionEvent::Disconnected {
          reason,
          description,
        });
      }
    }
    Ok(())
  }

  /// Routes one upper-layer message by its number.
  fn on_message(&mut self, payload: &[u8], sequence: u32) -> Result<(), SessionRefusal> {
    let number = payload.first().copied().unwrap_or(0);
    match number {
      USERAUTH_REQUEST => self.on_userauth(payload),
      USERAUTH_FAILURE | USERAUTH_SUCCESS | USERAUTH_BANNER | USERAUTH_PK_OK => {
        Err(self.fail(SessionRefusal::Unexpected { number }))
      }
      GLOBAL_REQUEST..=CHANNEL_FAILURE => {
        if self.authenticated {
          self.on_channel(payload)
        } else {
          Err(self.fail(SessionRefusal::Unexpected { number }))
        }
      }
      _ => self
        .transport
        .unimplemented(sequence)
        .map_err(|refusal| match refusal {
          SendRefusal::Fatal(refusal) => SessionRefusal::Transport(refusal),
          other => SessionRefusal::Send(other),
        }),
    }
  }

  /// An authentication request.
  fn on_userauth(&mut self, payload: &[u8]) -> Result<(), SessionRefusal> {
    let Some(authenticator) = self.authenticator.as_mut() else {
      return Err(self.fail(SessionRefusal::Unexpected {
        number: USERAUTH_REQUEST,
      }));
    };
    let answer = match authenticator.request(payload, &mut self.holders) {
      Ok(answer) => answer,
      Err(refusal) => return Err(self.fail(SessionRefusal::Auth(refusal))),
    };
    for reply in answer.replies {
      self.queue(reply);
    }
    if let Some(authenticated) = answer.authenticated {
      self.authenticated = true;
      self.pending = Some(SessionEvent::Authenticated(authenticated));
    }
    Ok(())
  }

  /// A connection-protocol message.
  fn on_channel(&mut self, payload: &[u8]) -> Result<(), SessionRefusal> {
    let outcome = match self.connection.handle(payload) {
      Ok(outcome) => outcome,
      Err(refusal) => return Err(self.fail(SessionRefusal::Channel(refusal))),
    };
    for reply in outcome.replies {
      self.queue(reply);
    }
    if outcome.event == Some(ChannelEvent::Writable) {
      self.send_blocked = false;
    }
    self.pending = outcome.event.map(|event| match event {
      ChannelEvent::Exec {
        command,
        git_protocol,
      } => SessionEvent::Exec {
        command,
        git_protocol,
      },
      ChannelEvent::Data(data) => SessionEvent::Data(data),
      ChannelEvent::Eof => SessionEvent::Eof,
      ChannelEvent::Closed => SessionEvent::ChannelClosed,
      ChannelEvent::Writable => SessionEvent::Writable,
    });
    Ok(())
  }

  /// Raises [`SessionEvent::Writable`] now, or once the waiting event is taken.
  fn raise_writable(&mut self) {
    self.send_blocked = false;
    if self.pending.is_none() {
      self.pending = Some(SessionEvent::Writable);
    } else if self.pending != Some(SessionEvent::Writable) {
      self.writable_owed = true;
    }
  }

  /// Closes the connection on `refusal`, sending its disconnect, and gives it back.
  fn fail(&mut self, refusal: SessionRefusal) -> SessionRefusal {
    self
      .transport
      .close(refusal.reason(), refusal.description());
    refusal
  }
}
