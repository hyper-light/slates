//! The session channel, the server's side (A-633; RFC 4254 §4-§6; §4.17.2 "Transports" item 2).
//!
//! **What is served.** One `session` channel at a time, which "accepts one `exec` request" naming a command the
//! version-control servers answer ([`crate::command`]), and `env` for `GIT_PROTOCOL` alone ("It is recommended that
//! implementations either maintain a list of allowable variable names" \[B: RFC 4254 §6.4\]). "There is no shell, no
//! subsystem and no forwarding" (§4.17.2): every other channel request is answered `SSH_MSG_CHANNEL_FAILURE` when the
//! client wants a reply, every global request `SSH_MSG_REQUEST_FAILURE`, and every other channel type
//! `SSH_MSG_CHANNEL_OPEN_FAILURE`: administratively prohibited for the types RFC 4254 itself defines (`x11`,
//! `forwarded-tcpip`, `direct-tcpip`), and an unknown type for the rest, since "if the server does not understand the
//! proposed 'channel type' ... the 'reason code' MUST be 0x00000003" (§5.1). A second session while one is open is
//! refused for want of resources. The command is never run: an accepted `exec` is raised to the caller, who serves it.
//!
//! **Flow control, with no fixed size.** The window the server offers is the caller's (`ChannelConfig::window`, the
//! client bytes the session may hold unconsumed, from its budget). The largest packet it offers is what its transport
//! accepts less the framing ([`max_data`]), since it "MUST NOT advertise a maximum packet size that would result in
//! transport packets larger than its transport layer is willing to receive" (§5.2). A client that sends past its window
//! or past that packet size is refused. The window is given back as the caller consumes what it was given: once a
//! maximum packet's worth is consumed, or sooner when what is left could not carry one, so a small window never
//! deadlocks and a large one is not answered byte by byte. What the server sends stays within the client's window and
//! its maximum packet, and within the largest packet every peer must accept.
//!
//! **Ending.** [`Connection::finish`] sends `exit-status`, then `SSH_MSG_CHANNEL_EOF`, then `SSH_MSG_CHANNEL_CLOSE`, in
//! that order (§6.10: "The channel needs to be closed with SSH_MSG_CHANNEL_CLOSE after this message"). A client's
//! close is answered with the server's own unless it was sent (§5.3), and the channel is gone once both have been.

use crate::command::{self, Command};
use crate::packet::{
  AES_BLOCK_LEN, LENGTH_LEN, MAX_PACKET_EVERY_PEER_ACCEPTS, MIN_PADDING, TAG_LEN,
};
use crate::wire::{Reader, UINT32_LEN, WireRefusal, put_boolean, put_byte, put_string, put_uint32};

/// Format: `SSH_MSG_GLOBAL_REQUEST` (RFC 4254 §9).
pub const GLOBAL_REQUEST: u8 = 80;
/// Format: `SSH_MSG_REQUEST_SUCCESS`.
pub const REQUEST_SUCCESS: u8 = 81;
/// Format: `SSH_MSG_REQUEST_FAILURE`.
pub const REQUEST_FAILURE: u8 = 82;
/// Format: `SSH_MSG_CHANNEL_OPEN`.
pub const CHANNEL_OPEN: u8 = 90;
/// Format: `SSH_MSG_CHANNEL_OPEN_CONFIRMATION`.
pub const CHANNEL_OPEN_CONFIRMATION: u8 = 91;
/// Format: `SSH_MSG_CHANNEL_OPEN_FAILURE`.
pub const CHANNEL_OPEN_FAILURE: u8 = 92;
/// Format: `SSH_MSG_CHANNEL_WINDOW_ADJUST`.
pub const CHANNEL_WINDOW_ADJUST: u8 = 93;
/// Format: `SSH_MSG_CHANNEL_DATA`.
pub const CHANNEL_DATA: u8 = 94;
/// Format: `SSH_MSG_CHANNEL_EXTENDED_DATA`.
pub const CHANNEL_EXTENDED_DATA: u8 = 95;
/// Format: `SSH_MSG_CHANNEL_EOF`.
pub const CHANNEL_EOF: u8 = 96;
/// Format: `SSH_MSG_CHANNEL_CLOSE`.
pub const CHANNEL_CLOSE: u8 = 97;
/// Format: `SSH_MSG_CHANNEL_REQUEST`.
pub const CHANNEL_REQUEST: u8 = 98;
/// Format: `SSH_MSG_CHANNEL_SUCCESS`.
pub const CHANNEL_SUCCESS: u8 = 99;
/// Format: `SSH_MSG_CHANNEL_FAILURE`.
pub const CHANNEL_FAILURE: u8 = 100;
/// The channel type served (RFC 4254 §6.1).
pub const SESSION: &str = "session";
/// The one environment variable taken (gitprotocol-v2: "the GIT_PROTOCOL environment variable must be set").
pub const GIT_PROTOCOL: &str = "GIT_PROTOCOL";
/// Format: `SSH_EXTENDED_DATA_STDERR` (RFC 4254 §5.2).
pub const EXTENDED_DATA_STDERR: u32 = 1;
/// Format: `SSH_OPEN_ADMINISTRATIVELY_PROHIBITED` (RFC 4254 §5.1).
const OPEN_ADMINISTRATIVELY_PROHIBITED: u32 = 1;
/// Format: `SSH_OPEN_UNKNOWN_CHANNEL_TYPE`.
const OPEN_UNKNOWN_CHANNEL_TYPE: u32 = 3;
/// Format: `SSH_OPEN_RESOURCE_SHORTAGE`.
const OPEN_RESOURCE_SHORTAGE: u32 = 4;
/// The channel types RFC 4254 defines besides `session` (§6.3.2, §7.2): understood, and prohibited.
const FORWARDING_TYPES: [&str; 3] = ["x11", "forwarded-tcpip", "direct-tcpip"];
/// The request that returns the command's status (§6.10).
const EXIT_STATUS: &str = "exit-status";
/// The request that starts a command (§6.5).
const EXEC: &str = "exec";
/// The request that passes an environment variable (§6.4).
const ENV: &str = "env";

/// Format: what a data message adds to its data, at most: its number, the channel, the extended type and the data's
/// length (RFC 4254 §5.2).
const DATA_OVERHEAD: usize = UINT32_LEN.saturating_mul(3).saturating_add(1);

/// The most data one message may carry inside a packet of `packet_cap` bytes on the wire: the cap less the length
/// field, the padding byte, the most padding a cipher adds (an AES block less one, plus four) and the tag, and less the
/// data message's own fields.
pub fn max_data(packet_cap: usize) -> u32 {
  let most_padding = AES_BLOCK_LEN.saturating_add(MIN_PADDING).saturating_sub(1);
  let framing = LENGTH_LEN
    .saturating_add(1)
    .saturating_add(most_padding)
    .saturating_add(TAG_LEN);
  let data = packet_cap
    .saturating_sub(framing)
    .saturating_sub(DATA_OVERHEAD);
  u32::try_from(data).unwrap_or(u32::MAX)
}

/// What the server offers each session channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelConfig {
  /// The client bytes the session may hold unconsumed, from the caller's budget.
  pub window: u32,
  /// The most data one client message may carry ([`max_data`] of the transport's packet cap).
  pub max_packet: u32,
}

/// Why the channel layer ended the connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelRefusal {
  /// A message's fields did not read.
  Malformed {
    /// The message's number.
    number: u8,
    /// Where it stopped.
    refusal: WireRefusal,
  },
  /// A message names a channel that is not open.
  UnknownChannel {
    /// The channel it named.
    channel: u32,
  },
  /// The client sent past the window it was given.
  WindowExceeded,
  /// The client sent a message past the packet size it was given.
  PacketTooLarge,
  /// The client grew the server's window past 2^32 - 1 (§5.2: "MUST NOT be increased above 2^32 - 1 bytes").
  WindowOverflow,
  /// A message the server never asked for: a reply to a request or an open it did not send.
  Unexpected {
    /// The message's number.
    number: u8,
  },
}

/// A misuse of the channel by the server's own caller, which ends nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelUse {
  /// No channel is open.
  NoChannel,
  /// No command was asked for, or it was already answered.
  NoCommand,
  /// The command is not running.
  NotRunning,
  /// More was consumed than was given.
  Overconsumed,
  /// The channel was already closed by the server.
  Finished,
}

/// What the channel layer tells its caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChannelEvent {
  /// The client asked for a command; the caller answers with [`Connection::answer_exec`].
  Exec {
    /// The command.
    command: Command,
    /// The `GIT_PROTOCOL` value the client passed before it, if any.
    git_protocol: Option<Vec<u8>>,
  },
  /// The client's standard input; the caller gives back what it consumed with [`Connection::consumed`].
  Data(Vec<u8>),
  /// The client will send no more data.
  Eof,
  /// The channel is closed both ways.
  Closed,
  /// The client's window opened again after it had closed to nothing.
  Writable,
}

/// What one message produced: the messages to send, in order, and an event.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
  /// The messages to send.
  pub replies: Vec<Vec<u8>>,
  /// The event to raise.
  pub event: Option<ChannelEvent>,
}

/// Where the channel's command stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Started {
  /// No `exec` yet.
  NotAsked,
  /// An `exec` raised and not yet answered; whether the client wants a reply.
  Asked {
    /// Whether the client wants a reply.
    want_reply: bool,
  },
  /// Accepted.
  Running,
  /// Refused.
  Refused,
}

/// The open session channel.
#[derive(Clone, Debug)]
struct Channel {
  /// The server's number for it.
  ours: u32,
  /// The client's number for it.
  theirs: u32,
  /// What the client may still send.
  receive_window: u64,
  /// What was raised as data and not yet consumed.
  unconsumed: u64,
  /// What was consumed and not yet given back.
  unreturned: u64,
  /// What the server may still send.
  send_window: u64,
  /// The most data one server message may carry.
  send_max_packet: u32,
  /// `GIT_PROTOCOL`, if passed.
  git_protocol: Option<Vec<u8>>,
  /// The command.
  started: Started,
  /// Whether the client sent EOF.
  eof_received: bool,
  /// Whether the server sent EOF.
  eof_sent: bool,
  /// Whether the server sent the exit status.
  exit_sent: bool,
  /// Whether the server sent its close.
  close_sent: bool,
}

/// The channel layer of one connection.
#[derive(Clone, Debug)]
pub struct Connection {
  /// What each channel is offered.
  config: ChannelConfig,
  /// The open channel.
  channel: Option<Channel>,
  /// The server's number for the next channel.
  next_number: u32,
}

impl Connection {
  /// A connection offering `config` to each channel.
  pub fn new(config: ChannelConfig) -> Self {
    Self {
      config,
      channel: None,
      next_number: 0,
    }
  }

  /// Whether a channel is open.
  pub fn is_open(&self) -> bool {
    self.channel.is_some()
  }

  /// What the server may still send on the open channel.
  pub fn send_window(&self) -> u64 {
    self
      .channel
      .as_ref()
      .map_or(0, |channel| channel.send_window)
  }

  /// Handles one connection-protocol message from the client.
  pub fn handle(&mut self, payload: &[u8]) -> Result<Outcome, ChannelRefusal> {
    let number = payload.first().copied().unwrap_or(0);
    let malformed = |refusal| ChannelRefusal::Malformed { number, refusal };
    let mut reader = Reader::new(payload);
    reader.byte().map_err(malformed)?;
    match number {
      GLOBAL_REQUEST => global_request(&mut reader).map_err(malformed),
      CHANNEL_OPEN => self.open(&mut reader).map_err(malformed),
      CHANNEL_WINDOW_ADJUST => self.window_adjust(&mut reader),
      CHANNEL_DATA => self.data(&mut reader, false),
      CHANNEL_EXTENDED_DATA => self.data(&mut reader, true),
      CHANNEL_EOF => self.eof(&mut reader),
      CHANNEL_CLOSE => self.close(&mut reader),
      CHANNEL_REQUEST => self.request(&mut reader),
      _ => Err(ChannelRefusal::Unexpected { number }),
    }
  }

  /// `SSH_MSG_CHANNEL_OPEN`: a session when none is open; every other type refused.
  fn open(&mut self, reader: &mut Reader<'_>) -> Result<Outcome, WireRefusal> {
    let kind = reader.string()?;
    let theirs = reader.uint32()?;
    let window = reader.uint32()?;
    let max_packet = reader.uint32()?;
    let refusal = if kind != SESSION.as_bytes() {
      let prohibited = FORWARDING_TYPES
        .iter()
        .any(|known| known.as_bytes() == kind);
      Some(if prohibited {
        OPEN_ADMINISTRATIVELY_PROHIBITED
      } else {
        OPEN_UNKNOWN_CHANNEL_TYPE
      })
    } else if self.channel.is_some() {
      Some(OPEN_RESOURCE_SHORTAGE)
    } else {
      None
    };
    if let Some(reason) = refusal {
      return Ok(Outcome {
        replies: vec![open_failure(theirs, reason)?],
        event: None,
      });
    }
    let ours = self.next_number;
    self.next_number = self.next_number.wrapping_add(1);
    self.channel = Some(Channel {
      ours,
      theirs,
      receive_window: u64::from(self.config.window),
      unconsumed: 0,
      unreturned: 0,
      send_window: u64::from(window),
      send_max_packet: max_packet,
      git_protocol: None,
      started: Started::NotAsked,
      eof_received: false,
      eof_sent: false,
      exit_sent: false,
      close_sent: false,
    });
    let mut confirmation = Vec::new();
    put_byte(&mut confirmation, CHANNEL_OPEN_CONFIRMATION);
    put_uint32(&mut confirmation, theirs);
    put_uint32(&mut confirmation, ours);
    put_uint32(&mut confirmation, self.config.window);
    put_uint32(&mut confirmation, self.config.max_packet);
    Ok(Outcome {
      replies: vec![confirmation],
      event: None,
    })
  }

  /// The open channel the client's message names.
  fn named(&mut self, reader: &mut Reader<'_>, number: u8) -> Result<&mut Channel, ChannelRefusal> {
    let channel = reader
      .uint32()
      .map_err(|refusal| ChannelRefusal::Malformed { number, refusal })?;
    match self.channel.as_mut() {
      Some(open) if open.ours == channel => Ok(open),
      _ => Err(ChannelRefusal::UnknownChannel { channel }),
    }
  }

  /// `SSH_MSG_CHANNEL_WINDOW_ADJUST`.
  fn window_adjust(&mut self, reader: &mut Reader<'_>) -> Result<Outcome, ChannelRefusal> {
    let channel = self.named(reader, CHANNEL_WINDOW_ADJUST)?;
    let malformed = |refusal| ChannelRefusal::Malformed {
      number: CHANNEL_WINDOW_ADJUST,
      refusal,
    };
    let added = reader.uint32().map_err(malformed)?;
    reader.clone().finish().map_err(malformed)?;
    let was_shut = channel.send_window == 0;
    let window = channel.send_window.saturating_add(u64::from(added));
    if window > u64::from(u32::MAX) {
      return Err(ChannelRefusal::WindowOverflow);
    }
    channel.send_window = window;
    let event = (was_shut && window > 0 && !channel.close_sent).then_some(ChannelEvent::Writable);
    Ok(Outcome {
      replies: Vec::new(),
      event,
    })
  }

  /// `SSH_MSG_CHANNEL_DATA`, or `SSH_MSG_CHANNEL_EXTENDED_DATA` when `extended`: within the window and the packet
  /// size. Extended data from a client has no reader in a session; its window is given back at once.
  fn data(&mut self, reader: &mut Reader<'_>, extended: bool) -> Result<Outcome, ChannelRefusal> {
    let number = if extended {
      CHANNEL_EXTENDED_DATA
    } else {
      CHANNEL_DATA
    };
    let max_packet = u64::from(self.config.max_packet);
    let channel = self.named(reader, number)?;
    let malformed = |refusal| ChannelRefusal::Malformed { number, refusal };
    if extended {
      reader.uint32().map_err(malformed)?;
    }
    let data = reader.string().map_err(malformed)?;
    reader.clone().finish().map_err(malformed)?;
    let length = crate::to_u64(data.len());
    if length > max_packet {
      return Err(ChannelRefusal::PacketTooLarge);
    }
    channel.receive_window = channel
      .receive_window
      .checked_sub(length)
      .ok_or(ChannelRefusal::WindowExceeded)?;
    if channel.close_sent {
      return Ok(Outcome::default());
    }
    if extended {
      channel.unreturned = channel.unreturned.saturating_add(length);
      let replies = return_window(channel, max_packet).into_iter().collect();
      return Ok(Outcome {
        replies,
        event: None,
      });
    }
    channel.unconsumed = channel.unconsumed.saturating_add(length);
    Ok(Outcome {
      replies: Vec::new(),
      event: Some(ChannelEvent::Data(data.to_vec())),
    })
  }

  /// `SSH_MSG_CHANNEL_EOF`.
  fn eof(&mut self, reader: &mut Reader<'_>) -> Result<Outcome, ChannelRefusal> {
    let channel = self.named(reader, CHANNEL_EOF)?;
    reader
      .clone()
      .finish()
      .map_err(|refusal| ChannelRefusal::Malformed {
        number: CHANNEL_EOF,
        refusal,
      })?;
    let first = !channel.eof_received;
    channel.eof_received = true;
    let event = (first && !channel.close_sent).then_some(ChannelEvent::Eof);
    Ok(Outcome {
      replies: Vec::new(),
      event,
    })
  }

  /// `SSH_MSG_CHANNEL_CLOSE`: answered with the server's close unless it was sent, and the channel is gone.
  fn close(&mut self, reader: &mut Reader<'_>) -> Result<Outcome, ChannelRefusal> {
    let channel = self.named(reader, CHANNEL_CLOSE)?;
    reader
      .clone()
      .finish()
      .map_err(|refusal| ChannelRefusal::Malformed {
        number: CHANNEL_CLOSE,
        refusal,
      })?;
    let replies = if channel.close_sent {
      Vec::new()
    } else {
      vec![channel_message(CHANNEL_CLOSE, channel.theirs)]
    };
    self.channel = None;
    Ok(Outcome {
      replies,
      event: Some(ChannelEvent::Closed),
    })
  }

  /// `SSH_MSG_CHANNEL_REQUEST`: `env` for `GIT_PROTOCOL`, one `exec`, and nothing else.
  fn request(&mut self, reader: &mut Reader<'_>) -> Result<Outcome, ChannelRefusal> {
    let malformed = |refusal| ChannelRefusal::Malformed {
      number: CHANNEL_REQUEST,
      refusal,
    };
    let channel = self.named(reader, CHANNEL_REQUEST)?;
    let kind = reader.string().map_err(malformed)?;
    let want_reply = reader.boolean().map_err(malformed)?;
    let answer = |accepted: bool| {
      let reply = if accepted {
        CHANNEL_SUCCESS
      } else {
        CHANNEL_FAILURE
      };
      want_reply
        .then(|| channel_message(reply, channel.theirs))
        .into_iter()
        .collect::<Vec<_>>()
    };
    if kind == ENV.as_bytes() {
      let name = reader.string().map_err(malformed)?;
      let value = reader.string().map_err(malformed)?;
      let taken = name == GIT_PROTOCOL.as_bytes() && channel.started == Started::NotAsked;
      let replies = answer(taken);
      if taken {
        channel.git_protocol = Some(value.to_vec());
      }
      return Ok(Outcome {
        replies,
        event: None,
      });
    }
    if kind != EXEC.as_bytes() || channel.started != Started::NotAsked {
      return Ok(Outcome {
        replies: answer(false),
        event: None,
      });
    }
    let line = reader.string().map_err(malformed)?;
    let Ok(command) = command::parse(line) else {
      return Ok(Outcome {
        replies: answer(false),
        event: None,
      });
    };
    channel.started = Started::Asked { want_reply };
    let git_protocol = channel.git_protocol.clone();
    Ok(Outcome {
      replies: Vec::new(),
      event: Some(ChannelEvent::Exec {
        command,
        git_protocol,
      }),
    })
  }

  /// Answers the raised `exec`: `SSH_MSG_CHANNEL_SUCCESS` when `accept`, `SSH_MSG_CHANNEL_FAILURE` otherwise, if the
  /// client wanted a reply.
  pub fn answer_exec(&mut self, accept: bool) -> Result<Vec<Vec<u8>>, ChannelUse> {
    let channel = self.channel.as_mut().ok_or(ChannelUse::NoChannel)?;
    let Started::Asked { want_reply } = channel.started else {
      return Err(ChannelUse::NoCommand);
    };
    channel.started = if accept {
      Started::Running
    } else {
      Started::Refused
    };
    let reply = if accept {
      CHANNEL_SUCCESS
    } else {
      CHANNEL_FAILURE
    };
    Ok(
      want_reply
        .then(|| channel_message(reply, channel.theirs))
        .into_iter()
        .collect(),
    )
  }

  /// Records that the caller consumed `bytes` of the data raised; the window to give back, when it is due.
  pub fn consumed(&mut self, bytes: u64) -> Result<Option<Vec<u8>>, ChannelUse> {
    let max_packet = u64::from(self.config.max_packet);
    let channel = self.channel.as_mut().ok_or(ChannelUse::NoChannel)?;
    channel.unconsumed = channel
      .unconsumed
      .checked_sub(bytes)
      .ok_or(ChannelUse::Overconsumed)?;
    channel.unreturned = channel.unreturned.saturating_add(bytes);
    Ok(return_window(channel, max_packet))
  }

  /// Data messages carrying up to `room` bytes of `data` (standard error's when `stderr`), within the client's window
  /// and maximum packet; how many bytes they carry.
  pub fn send(
    &mut self,
    data: &[u8],
    room: usize,
    stderr: bool,
  ) -> Result<(usize, Vec<Vec<u8>>), ChannelUse> {
    let channel = self.channel.as_mut().ok_or(ChannelUse::NoChannel)?;
    if channel.started != Started::Running {
      return Err(ChannelUse::NotRunning);
    }
    if channel.close_sent || channel.eof_sent {
      return Err(ChannelUse::Finished);
    }
    let window = usize::try_from(channel.send_window).unwrap_or(usize::MAX);
    let ceiling = usize::try_from(max_data(MAX_PACKET_EVERY_PEER_ACCEPTS)).unwrap_or(usize::MAX);
    let chunk = usize::try_from(channel.send_max_packet)
      .unwrap_or(usize::MAX)
      .min(ceiling);
    let total = data.len().min(window).min(room);
    let mut messages = Vec::new();
    let mut sent = 0usize;
    while sent < total && chunk > 0 {
      let end = sent.saturating_add(chunk).min(total);
      let piece = data.get(sent..end).unwrap_or(&[]);
      messages.push(data_message(channel.theirs, piece, stderr).map_err(|_| ChannelUse::Finished)?);
      sent = end;
    }
    channel.send_window = channel.send_window.saturating_sub(crate::to_u64(sent));
    Ok((sent, messages))
  }

  /// The messages that end the channel, those not yet sent, in order: `exit-status` (when the command ran), EOF and
  /// close.
  pub fn finish(&mut self, exit_status: u32) -> Result<Vec<Vec<u8>>, ChannelUse> {
    let channel = self.channel.as_mut().ok_or(ChannelUse::NoChannel)?;
    if channel.close_sent {
      return Err(ChannelUse::Finished);
    }
    let mut messages = Vec::new();
    if channel.started == Started::Running && !channel.exit_sent {
      messages
        .push(exit_status_message(channel.theirs, exit_status).map_err(|_| ChannelUse::Finished)?);
      channel.exit_sent = true;
    }
    if !channel.eof_sent {
      messages.push(channel_message(CHANNEL_EOF, channel.theirs));
      channel.eof_sent = true;
    }
    messages.push(channel_message(CHANNEL_CLOSE, channel.theirs));
    channel.close_sent = true;
    Ok(messages)
  }
}

/// `SSH_MSG_GLOBAL_REQUEST`: refused when a reply is wanted (RFC 4254 §4: "If the recipient does not recognize or
/// support the request, it simply responds with SSH_MSG_REQUEST_FAILURE").
fn global_request(reader: &mut Reader<'_>) -> Result<Outcome, WireRefusal> {
  reader.string()?;
  let want_reply = reader.boolean()?;
  let replies = if want_reply {
    vec![vec![REQUEST_FAILURE]]
  } else {
    Vec::new()
  };
  Ok(Outcome {
    replies,
    event: None,
  })
}

/// The window to give back, once a maximum packet's worth is consumed or the client cannot send one.
fn return_window(channel: &mut Channel, max_packet: u64) -> Option<Vec<u8>> {
  let due = channel.unreturned >= max_packet || channel.receive_window < max_packet;
  if channel.unreturned == 0 || !due || channel.close_sent {
    return None;
  }
  let returned = u32::try_from(channel.unreturned).unwrap_or(u32::MAX);
  channel.unreturned = channel.unreturned.saturating_sub(u64::from(returned));
  channel.receive_window = channel.receive_window.saturating_add(u64::from(returned));
  let mut message = channel_message(CHANNEL_WINDOW_ADJUST, channel.theirs);
  put_uint32(&mut message, returned);
  Some(message)
}

/// A message of `number` naming the client's channel `theirs` and nothing else.
fn channel_message(number: u8, theirs: u32) -> Vec<u8> {
  let mut message = Vec::new();
  put_byte(&mut message, number);
  put_uint32(&mut message, theirs);
  message
}

/// `SSH_MSG_CHANNEL_OPEN_FAILURE` for the client's channel `theirs`.
fn open_failure(theirs: u32, reason: u32) -> Result<Vec<u8>, WireRefusal> {
  let mut message = channel_message(CHANNEL_OPEN_FAILURE, theirs);
  put_uint32(&mut message, reason);
  let description: &[u8] = match reason {
    OPEN_ADMINISTRATIVELY_PROHIBITED => b"no forwarding",
    OPEN_RESOURCE_SHORTAGE => b"one session at a time",
    _ => b"unknown channel type",
  };
  put_string(&mut message, description)?;
  put_string(&mut message, b"")?;
  Ok(message)
}

/// A data message, `SSH_MSG_CHANNEL_EXTENDED_DATA` with standard error's code when `stderr`.
fn data_message(theirs: u32, data: &[u8], stderr: bool) -> Result<Vec<u8>, WireRefusal> {
  let mut message = channel_message(
    if stderr {
      CHANNEL_EXTENDED_DATA
    } else {
      CHANNEL_DATA
    },
    theirs,
  );
  if stderr {
    put_uint32(&mut message, EXTENDED_DATA_STDERR);
  }
  put_string(&mut message, data)?;
  Ok(message)
}

/// The `exit-status` request, which wants no reply (§6.10).
fn exit_status_message(theirs: u32, exit_status: u32) -> Result<Vec<u8>, WireRefusal> {
  let mut message = channel_message(CHANNEL_REQUEST, theirs);
  put_string(&mut message, EXIT_STATUS.as_bytes())?;
  put_boolean(&mut message, false);
  put_uint32(&mut message, exit_status);
  Ok(message)
}
