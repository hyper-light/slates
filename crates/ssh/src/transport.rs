//! The server's transport (A-594; RFC 4253 §§4-12; `draft-ietf-sshm-strict-kex-03` §3): one connection's state, driven
//! by its caller.
//!
//! **The loop.** The caller passes the client's bytes to [`ServerTransport::receive`], which consumes what it can and
//! says how much; reads [`ServerTransport::poll_event`] until it is empty; writes [`ServerTransport::pending_output`] to
//! the socket and gives back what was written with [`ServerTransport::consume_output`]; and calls
//! [`ServerTransport::poll_timers`] at [`ServerTransport::next_timer_ns`]. Nothing is read while an event waits or while
//! a packet cap of output is unwritten, so the transport holds at most one packet in, one event and about a cap out.
//!
//! **Order of a connection.** The server sends its banner lines, its identification and its first KEXINIT at once. It
//! reads the client's identification, then packets: the client's KEXINIT, `SSH_MSG_KEX_ECDH_INIT`, which it answers with
//! `SSH_MSG_KEX_ECDH_REPLY` and its `SSH_MSG_NEWKEYS` under which its own packets then go, and the client's
//! `SSH_MSG_NEWKEYS`, after which the client's packets are opened under the new keys and [`Event::Established`] is
//! raised. The client then asks for `ssh-userauth` (RFC 4253 §10), the one service the server runs, and the layers above
//! take over: [`Event::Message`] carries their messages (numbers 50 to 255) and [`ServerTransport::send`] theirs.
//!
//! **What each message may do, when** (RFC 4253 §7.1, §11; the strict-kex draft §3.2):
//! - `SSH_MSG_DISCONNECT` ends the connection whenever it comes, raising [`Event::Disconnected`].
//! - Under strict key exchange, the client's KEXINIT must be its first packet, and until the first exchange ends only
//!   KEXINIT, NEWKEYS and the exchange's own messages (30 to 49) are allowed; anything else ends the connection.
//! - `SSH_MSG_IGNORE`, `SSH_MSG_DEBUG` and `SSH_MSG_UNIMPLEMENTED` are dropped. An unrecognized number below 50 is
//!   answered `SSH_MSG_UNIMPLEMENTED` with its sequence number (§11.4).
//! - Exchange messages out of their place (a second KEXINIT, `SSH_MSG_KEX_ECDH_INIT` twice or before the KEXINITs,
//!   NEWKEYS before the reply, any number the server itself sends) end the connection, as does a service request
//!   before the first exchange ends, during one or twice. An upper-layer message before the service is accepted, or
//!   between the client's KEXINIT and its NEWKEYS, which §7.1 forbids, ends it too.
//!
//! **Sequence numbers.** Each direction counts its packets and wraps at 2^32 (§6.4), except before the first exchange
//! ends, where a wrap ends the connection (strict-kex draft §3.2, and OpenSSH's own rule). Under strict key exchange each
//! direction's count restarts at zero after its `SSH_MSG_NEWKEYS` (§3.3); otherwise it carries on into the new keys.
//!
//! **Rekeying** ([`crate::keylife`]). Either side may start an exchange after the first (§9). The server starts one when
//! a key reaches half its cipher's limit in either direction, or when the key time passes (but never sooner than the
//! last exchange took), and only once the service is accepted, so a service reply never falls inside an exchange.
//! While its KEXINIT is out and its NEWKEYS is not, [`ServerTransport::send`] answers [`SendRefusal::Rekeying`], and the
//! caller sends again after [`Event::Rekeyed`]. A key that reaches its whole limit anyway ends the connection.
//!
//! **Key exchange** ([`crate::kex`]). The method is the client's first choice of `mlkem768x25519-sha256` (A-634) and
//! `curve25519-sha256`; both run as `SSH_MSG_KEX_ECDH_INIT` and `SSH_MSG_KEX_ECDH_REPLY`, the hybrid's values longer, and
//! a value that fails its method's checks ends the connection with `SSH_DISCONNECT_KEY_EXCHANGE_FAILED`.
//!
//! **Extensions.** After the first `SSH_MSG_NEWKEYS`, when the client's KEXINIT lists `ext-info-c`, the server sends
//! RFC 8308's `SSH_MSG_EXT_INFO` with the extensions its caller configured (A-633: `server-sig-algs`).
//!
//! **Refusals.** Every refusal is a [`Refusal`]; the transport sends `SSH_MSG_DISCONNECT` with its reason and closes.
//! Not built here: user authentication and channels ([`crate::session`] above it), compression, and other key exchanges.

use crate::entropy::Entropy;
use crate::hostkey::HostKey;
use crate::ident::{
  IdentificationReader, OwnIdentification, OwnIdentificationRefusal, software_version,
};
use crate::kex::{
  ExchangeInputs, HASH_LEN, KEX_ECDH_INIT, KEX_ECDH_REPLY, KexRefusal, X25519_LEN,
  curve25519_server, ecdh_reply, exchange_hash, mlkem768x25519_server, new_keys, read_ecdh_init,
};
use crate::kexinit::{
  COOKIE_LEN, KEXINIT, KexInit, KexMethod, Negotiated, negotiate, server_kexinit,
};
use crate::keylife::{KeyLimits, UsageCap};
use crate::packet::{
  Cipher, KeyMaterialRefusal, LENGTH_LEN, MAX_PACKET_EVERY_PEER_ACCEPTS, Opener, SealRefusal,
  Sealer, Usage,
};
use crate::refusal::{Direction, DisconnectReason, Refusal};
use crate::to_u64;
use crate::wire::{Reader, WireRefusal, put_byte, put_string, put_uint32};

/// `SSH_MSG_DISCONNECT` (RFC 4250 §4.1.2).
pub const DISCONNECT: u8 = 1;
/// `SSH_MSG_IGNORE`.
pub const IGNORE: u8 = 2;
/// Format: `SSH_MSG_UNIMPLEMENTED`.
pub const UNIMPLEMENTED: u8 = 3;
/// Format: `SSH_MSG_DEBUG`.
pub const DEBUG: u8 = 4;
/// Format: `SSH_MSG_SERVICE_REQUEST`.
pub const SERVICE_REQUEST: u8 = 5;
/// Format: `SSH_MSG_SERVICE_ACCEPT`.
pub const SERVICE_ACCEPT: u8 = 6;
/// Format: `SSH_MSG_EXT_INFO` (RFC 8308 §2.3).
pub const EXT_INFO: u8 = 7;
/// Format: `SSH_MSG_NEWKEYS`.
pub const NEWKEYS: u8 = 21;
/// Format: the first number of the key exchange methods' own messages (RFC 4250 §4.1.2: 30 to 49).
pub const FIRST_KEX_METHOD: u8 = 30;
/// Format: the last.
pub const LAST_KEX_METHOD: u8 = 49;
/// Format: the first number of the layers above the transport (RFC 4250 §4.1.2: 50 and up).
pub const FIRST_UPPER_LAYER: u8 = 50;
/// The service the server runs (RFC 4252 §1).
pub const USERAUTH_SERVICE: &str = "ssh-userauth";

/// What a transport is set up with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransportConfig {
  /// The largest packet the server accepts, in bytes on the wire with its tag, derived by the caller from its budget;
  /// at least [`MAX_PACKET_EVERY_PEER_ACCEPTS`], as RFC 4253 §6.1 requires.
  pub packet_cap: usize,
  /// Lines sent before the identification (RFC 4253 §4.2).
  pub banner: Vec<String>,
  /// The identification line's comments, if any.
  pub comments: Option<String>,
  /// How long a key serves before the server rekeys ([`crate::keylife::key_time_limit_ns`]).
  pub key_time_ns: u64,
  /// An operator's cap on a key's use, below its cipher's limits.
  pub usage_cap: UsageCap,
  /// RFC 8308's extensions, sent in `SSH_MSG_EXT_INFO` as the packet after the server's first NEWKEYS when the
  /// client's first KEXINIT listed `ext-info-c` (§2.4); none sends nothing.
  pub extensions: Vec<Extension>,
}

/// One of RFC 8308's extensions: its name and its value's bytes (§2.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Extension {
  /// The extension's name, such as `server-sig-algs`.
  pub name: String,
  /// Its value, as the extension defines it.
  pub value: Vec<u8>,
}

/// Why a transport could not be set up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigRefusal {
  /// The packet cap is below what RFC 4253 §6.1 requires every implementation to accept.
  PacketCapBelowStandard {
    /// The cap given.
    cap: u64,
    /// The least cap allowed.
    floor: u64,
  },
  /// The banner or the identification line.
  Identification(OwnIdentificationRefusal),
  /// The first KEXINIT's cookie or padding could not be drawn.
  Entropy,
  /// The first KEXINIT would not encode.
  Encoding(WireRefusal),
}

/// What a key exchange agreed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Algorithms {
  /// The key exchange method, so a deployment can count the connections that ran the post-quantum hybrid (A-634).
  pub kex: KexMethod,
  /// What the client sends.
  pub client_to_server: Cipher,
  /// What the server sends.
  pub server_to_client: Cipher,
}

/// The first exchange's outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Established {
  /// The session id: the first exchange hash (RFC 4253 §7.2).
  pub session_id: [u8; HASH_LEN],
  /// Whether strict key exchange is on.
  pub strict: bool,
  /// Whether the client listed `ext-info-c` (RFC 8308 §2.1), for the authentication layer.
  pub client_ext_info: bool,
  /// The method and the ciphers.
  pub algorithms: Algorithms,
}

/// What the transport tells its caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
  /// The first key exchange ended.
  Established(Established),
  /// A later key exchange ended.
  Rekeyed(Algorithms),
  /// The client asked for `ssh-userauth` and the server accepted.
  ServiceAccepted,
  /// A message for the layers above (numbers 50 to 255), with the sequence number it came under, which an
  /// `SSH_MSG_UNIMPLEMENTED` names ([`ServerTransport::unimplemented`]).
  Message {
    /// The message, its number first.
    payload: Vec<u8>,
    /// The packet's sequence number.
    sequence: u32,
  },
  /// The client disconnected.
  Disconnected {
    /// Its reason code (RFC 4253 §11.1).
    reason: u32,
    /// Its description, as UTF-8 with anything else replaced.
    description: String,
  },
}

/// Why a message from the layers above was not sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendRefusal {
  /// The service has not been accepted yet.
  NotEstablished,
  /// A key exchange is under way: send again after [`Event::Rekeyed`].
  Rekeying,
  /// The output holds a packet cap of bytes the caller has not written: write, then send again.
  OutputFull,
  /// The message's number belongs to the transport (below 50).
  NotUpperLayer {
    /// The number.
    number: u8,
  },
  /// The message is empty.
  Empty,
  /// The message would make a packet larger than [`MAX_PACKET_EVERY_PEER_ACCEPTS`].
  TooLarge {
    /// The message's bytes.
    payload: u64,
    /// The largest packet sent.
    ceiling: u64,
  },
  /// The connection is closed.
  Closed,
  /// The send ended the connection.
  Fatal(Refusal),
}

/// What the transport reads next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
  /// The client's identification line.
  Identification,
  /// Binary packets.
  Packets,
  /// Nothing: the connection is closed.
  Closed,
}

/// A key exchange under way.
struct Exchange {
  /// Whether it is the connection's first.
  initial: bool,
  /// The server's KEXINIT payload, `I_S`, already sent.
  ours: Vec<u8>,
  /// The client's KEXINIT payload, `I_C`, once read.
  theirs: Option<Vec<u8>>,
  /// What the two agreed, once both are known.
  negotiated: Option<Negotiated>,
  /// Whether the client's next packet is a wrong guess to ignore.
  skip_guess: bool,
  /// The client's new keys, held from the server's reply until the client's NEWKEYS.
  next_opener: Option<Opener>,
  /// When it began.
  started_ns: u64,
}

/// What each direction's key has processed since it was taken into use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyUsage {
  /// What the server sealed.
  pub sent: Usage,
  /// What the server opened.
  pub received: Usage,
}

/// One connection's transport, the server's side.
pub struct ServerTransport<E: Entropy> {
  /// The random bytes it draws.
  entropy: E,
  /// The host key it signs with.
  host_key: HostKey,
  /// Its banner and identification.
  identification: OwnIdentification,
  /// The largest packet it accepts.
  packet_cap: usize,
  /// How long a key serves.
  key_time_ns: u64,
  /// The operator's cap on a key's use.
  usage_cap: UsageCap,
  /// RFC 8308's extensions, sent after the first NEWKEYS to a client that listed `ext-info-c`.
  extensions: Vec<Extension>,
  /// What it reads next.
  phase: Phase,
  /// The client's identification line, being read.
  reader: IdentificationReader,
  /// The client's identification line, `V_C`, once read.
  client_identification: Option<Vec<u8>>,
  /// How the server's packets are sealed.
  sealer: Sealer,
  /// How the client's packets are opened.
  opener: Opener,
  /// The bytes of the packet being read.
  inbound: Vec<u8>,
  /// Its size on the wire, once its first four bytes are read.
  inbound_size: Option<usize>,
  /// Sealed bytes the caller has not yet written.
  output: Vec<u8>,
  /// The event waiting for the caller.
  pending: Option<Event>,
  /// The exchange under way.
  exchange: Option<Exchange>,
  /// The session id, once the first exchange hash is known.
  session_id: Option<[u8; HASH_LEN]>,
  /// Whether strict key exchange is on.
  strict: bool,
  /// Whether the first exchange has ended.
  established: bool,
  /// Whether the client's service request was accepted.
  service_accepted: bool,
  /// Whether the client's first KEXINIT has come.
  seen_kexinit: bool,
  /// The client's packets before its first KEXINIT.
  packets_before_kexinit: u64,
  /// When the current keys were taken into use.
  key_installed_ns: u64,
  /// How long the last exchange took.
  last_exchange_ns: u64,
  /// Whether the closing disconnect was sealed: none until the connection closes on the server's side.
  disconnect_sealed: Option<bool>,
  /// The last exchange after the first, until its caller takes it ([`ServerTransport::take_rekey`]).
  rekeyed: Option<Rekeyed>,
}

/// What started an exchange after the first (A-745: every rekey is logged with its trigger).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RekeyTrigger {
  /// The key time ran out ([`ServerTransport::poll_timers`]).
  KeyTime,
  /// A direction's key reached its rekey point: half its packets or blocks.
  KeyUsage,
  /// The client sent its KEXINIT first.
  Client,
}

/// An exchange after the first as it began: its trigger, how long the keys it replaces served, and their usage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rekeyed {
  /// What started it.
  pub trigger: RekeyTrigger,
  /// How long the keys it replaces served, in nanoseconds.
  pub served_ns: u64,
  /// The server's key's usage: packets and blocks sealed.
  pub sealed: Usage,
  /// The client's key's usage: packets and blocks opened.
  pub opened: Usage,
  /// The key time the server holds now.
  pub key_time_ns: u64,
}

impl<E: Entropy> core::fmt::Debug for ServerTransport<E> {
  fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    formatter
      .debug_struct("ServerTransport")
      .field("phase", &self.phase)
      .field("sealer", &self.sealer)
      .field("opener", &self.opener)
      .field("established", &self.established)
      .field("strict", &self.strict)
      .field("service_accepted", &self.service_accepted)
      .field("exchanging", &self.exchange.is_some())
      .finish()
  }
}

impl<E: Entropy> ServerTransport<E> {
  /// A transport for a new connection: its banner, identification and first KEXINIT are in the output at once.
  pub fn new(
    config: TransportConfig,
    host_key: HostKey,
    entropy: E,
    now_ns: u64,
  ) -> Result<Self, ConfigRefusal> {
    if config.packet_cap < MAX_PACKET_EVERY_PEER_ACCEPTS {
      return Err(ConfigRefusal::PacketCapBelowStandard {
        cap: to_u64(config.packet_cap),
        floor: to_u64(MAX_PACKET_EVERY_PEER_ACCEPTS),
      });
    }
    let identification = OwnIdentification::new(
      &software_version(),
      config.comments.as_deref(),
      &config.banner,
    )
    .map_err(ConfigRefusal::Identification)?;
    let mut transport = Self {
      entropy,
      host_key,
      output: identification.wire().to_vec(),
      identification,
      packet_cap: config.packet_cap,
      key_time_ns: config.key_time_ns,
      usage_cap: config.usage_cap,
      extensions: config.extensions,
      phase: Phase::Identification,
      reader: IdentificationReader::new(),
      client_identification: None,
      sealer: Sealer::clear(),
      opener: Opener::clear(),
      inbound: Vec::new(),
      inbound_size: None,
      pending: None,
      exchange: None,
      session_id: None,
      strict: false,
      established: false,
      service_accepted: false,
      seen_kexinit: false,
      packets_before_kexinit: 0,
      key_installed_ns: now_ns,
      last_exchange_ns: 0,
      disconnect_sealed: None,
      rekeyed: None,
    };
    transport
      .begin_exchange(true, now_ns)
      .map_err(|refusal| match refusal {
        Refusal::Message { refusal, .. } => ConfigRefusal::Encoding(refusal),
        _ => ConfigRefusal::Entropy,
      })?;
    Ok(transport)
  }

  /// Reads the client's bytes from `input`: how many were consumed. Fewer than all means an event waits or the output is
  /// full; the caller acts on it and passes the rest again. A refusal closes the connection, with its disconnect queued.
  pub fn receive(&mut self, input: &[u8], now_ns: u64) -> Result<usize, Refusal> {
    let mut consumed = 0usize;
    while !self.blocked() {
      let rest = input.get(consumed..).unwrap_or(&[]);
      if rest.is_empty() {
        break;
      }
      let step = match self.phase {
        Phase::Identification => self.read_identification(rest),
        Phase::Packets => self.read_packet(rest, now_ns),
        Phase::Closed => Ok(0),
      };
      let used = step.inspect_err(|refusal| self.fail(*refusal))?;
      if used == 0 {
        break;
      }
      consumed = consumed.saturating_add(used);
    }
    Ok(consumed)
  }

  /// The event waiting, if any.
  pub fn poll_event(&mut self) -> Option<Event> {
    self.pending.take()
  }

  /// Bytes to write to the socket.
  pub fn pending_output(&self) -> &[u8] {
    &self.output
  }

  /// Gives back `count` bytes of [`ServerTransport::pending_output`] as written.
  pub fn consume_output(&mut self, count: usize) {
    let count = count.min(self.output.len());
    self.output.drain(..count);
  }

  /// Whether an exchange holds the upper layer's sends back ([`SendRefusal::Rekeying`]): the server's KEXINIT out and
  /// its NEWKEYS not. Only reading the client's half of the exchange ends it (A-745).
  pub fn is_rekeying(&self) -> bool {
    self
      .exchange
      .as_ref()
      .is_some_and(|exchange| exchange.next_opener.is_none())
  }

  /// The last exchange after the first that began since this was last asked, with its trigger (A-745).
  pub fn take_rekey(&mut self) -> Option<Rekeyed> {
    self.rekeyed.take()
  }

  /// Notes an exchange after the first beginning now for `trigger`.
  fn note_rekey(&mut self, trigger: RekeyTrigger, now_ns: u64) {
    self.rekeyed = Some(Rekeyed {
      trigger,
      served_ns: now_ns.saturating_sub(self.key_installed_ns),
      sealed: self.sealer.usage(),
      opened: self.opener.usage(),
      key_time_ns: self.key_time_ns,
    });
  }

  /// Whether the connection is closed.
  pub fn is_closed(&self) -> bool {
    self.phase == Phase::Closed
  }

  /// Whether the server's closing disconnect was sealed into the output: none until the server closes, and false when
  /// its key could no longer seal one (the connection closes without it).
  pub fn disconnect_sealed(&self) -> Option<bool> {
    self.disconnect_sealed
  }

  /// Whether the first exchange has ended: the client's `SSH_MSG_NEWKEYS` taken (A-651: an edge's handshake ends here,
  /// a round trip after the session id is known, which a client's host-key confirmation lies between).
  pub fn is_established(&self) -> bool {
    self.established
  }

  /// The session id, once the first exchange hash is known.
  pub fn session_id(&self) -> Option<&[u8; HASH_LEN]> {
    self.session_id.as_ref()
  }

  /// The client's identification line, once read.
  pub fn client_identification(&self) -> Option<&[u8]> {
    self.client_identification.as_deref()
  }

  /// What each direction's current key has processed.
  pub fn key_usage(&self) -> KeyUsage {
    KeyUsage {
      sent: self.sealer.usage(),
      received: self.opener.usage(),
    }
  }

  /// Sets how long a key serves, as the deployment's measured lifetimes move.
  pub fn set_key_time_ns(&mut self, key_time_ns: u64) {
    self.key_time_ns = key_time_ns;
  }

  /// When the key time runs out and [`ServerTransport::poll_timers`] would start an exchange, if one could start.
  pub fn next_timer_ns(&self) -> Option<u64> {
    self.may_start_rekey().then(|| self.rekey_deadline_ns())
  }

  /// Starts an exchange if the key time has run out.
  pub fn poll_timers(&mut self, now_ns: u64) -> Result<(), Refusal> {
    if !self.may_start_rekey() || now_ns < self.rekey_deadline_ns() {
      return Ok(());
    }
    self.note_rekey(RekeyTrigger::KeyTime, now_ns);
    self
      .begin_exchange(false, now_ns)
      .inspect_err(|refusal| self.fail(*refusal))
  }

  /// Sends one message of the layers above.
  pub fn send(&mut self, payload: &[u8], now_ns: u64) -> Result<(), SendRefusal> {
    self.check_send(payload)?;
    let ceiling = to_u64(MAX_PACKET_EVERY_PEER_ACCEPTS);
    let after = self
      .sealer
      .usage_after(payload.len())
      .ok_or(SendRefusal::TooLarge {
        payload: to_u64(payload.len()),
        ceiling,
      })?;
    if self
      .seal_limits()
      .is_some_and(|limits| limits.exceeded_by(after))
    {
      return Err(SendRefusal::Fatal(self.refuse(Refusal::KeyExhausted {
        direction: Direction::ServerToClient,
      })));
    }
    self.seal(payload).map_err(|refusal| match refusal {
      Sent::TooLarge { payload, ceiling } => SendRefusal::TooLarge { payload, ceiling },
      Sent::Fatal(refusal) => SendRefusal::Fatal(self.refuse(refusal)),
    })?;
    self
      .rekey_if_due(now_ns)
      .map_err(|refusal| SendRefusal::Fatal(self.refuse(refusal)))
  }

  /// Answers the upper-layer message that came under `sequence` with `SSH_MSG_UNIMPLEMENTED` (RFC 4253 §11.4), for
  /// the layers above, which alone know whether they recognize a number of theirs.
  pub fn unimplemented(&mut self, sequence: u32) -> Result<(), SendRefusal> {
    if self.phase == Phase::Closed {
      return Err(SendRefusal::Closed);
    }
    self
      .seal_transport(&unimplemented(sequence))
      .map_err(|refusal| SendRefusal::Fatal(self.refuse(refusal)))
  }

  /// Ends the connection with `reason` and `description`, sending the disconnect.
  pub fn close(&mut self, reason: DisconnectReason, description: &str) {
    if self.phase != Phase::Closed {
      self.send_disconnect(reason, description);
      self.phase = Phase::Closed;
    }
  }

  /// Whether `send` may go ahead for `payload`.
  fn check_send(&self, payload: &[u8]) -> Result<(), SendRefusal> {
    if self.phase == Phase::Closed {
      return Err(SendRefusal::Closed);
    }
    if !self.service_accepted {
      return Err(SendRefusal::NotEstablished);
    }
    if self.is_rekeying() {
      return Err(SendRefusal::Rekeying);
    }
    let number = payload.first().copied().ok_or(SendRefusal::Empty)?;
    if number < FIRST_UPPER_LAYER {
      return Err(SendRefusal::NotUpperLayer { number });
    }
    if self.output.len() >= self.packet_cap {
      return Err(SendRefusal::OutputFull);
    }
    Ok(())
  }

  /// Whether reading must wait for the caller.
  fn blocked(&self) -> bool {
    self.phase == Phase::Closed || self.pending.is_some() || self.output.len() >= self.packet_cap
  }

  /// Reads the client's identification line from `rest`.
  fn read_identification(&mut self, rest: &[u8]) -> Result<usize, Refusal> {
    let (used, line) = self.reader.read(rest).map_err(Refusal::Identification)?;
    if let Some(line) = line {
      self.client_identification = Some(line);
      self.phase = Phase::Packets;
    }
    Ok(used)
  }

  /// Reads from `rest` toward the next packet, and handles it once it is whole.
  fn read_packet(&mut self, rest: &[u8], now_ns: u64) -> Result<usize, Refusal> {
    let wanted = self.inbound_size.unwrap_or(LENGTH_LEN);
    let take = wanted.saturating_sub(self.inbound.len()).min(rest.len());
    self
      .inbound
      .extend_from_slice(rest.get(..take).unwrap_or(&[]));
    if self.inbound.len() < wanted {
      return Ok(take);
    }
    if self.inbound_size.is_none() {
      let head: [u8; LENGTH_LEN] = self
        .inbound
        .as_slice()
        .try_into()
        .map_err(|_| Refusal::Unexpected { number: 0 })?;
      self.inbound_size = Some(
        self
          .opener
          .frame(head, self.packet_cap)
          .map_err(Refusal::Packet)?,
      );
      return Ok(take);
    }
    let mut packet = core::mem::take(&mut self.inbound);
    self.inbound_size = None;
    let opened = self.opener.open(&mut packet).map_err(Refusal::Packet)?;
    if !self.established && opened.sequence == u32::MAX {
      return Err(Refusal::SequenceWrapBeforeKeys);
    }
    self.check_received_usage()?;
    let payload = packet.get(opened.payload.clone()).unwrap_or(&[]);
    self.handle(payload, opened.sequence, now_ns)?;
    self.rekey_if_due(now_ns)?;
    packet.clear();
    self.inbound = packet;
    Ok(take)
  }

  /// Ends the connection if the client's key has passed its limit.
  fn check_received_usage(&self) -> Result<(), Refusal> {
    let passed = self
      .open_limits()
      .is_some_and(|limits| limits.exceeded_by(self.opener.usage()));
    if passed {
      return Err(Refusal::KeyExhausted {
        direction: Direction::ClientToServer,
      });
    }
    Ok(())
  }

  /// Handles one message, opened under `sequence`.
  fn handle(&mut self, payload: &[u8], sequence: u32, now_ns: u64) -> Result<(), Refusal> {
    let number = payload.first().copied().unwrap_or(0);
    if !self.seen_kexinit && number != KEXINIT {
      self.packets_before_kexinit = self.packets_before_kexinit.saturating_add(1);
    }
    if number == DISCONNECT {
      return self.on_disconnect(payload);
    }
    if self.strict && !self.established && !is_exchange_message(number) {
      return Err(Refusal::Unexpected { number });
    }
    if self
      .exchange
      .as_mut()
      .is_some_and(|exchange| core::mem::take(&mut exchange.skip_guess))
    {
      return Ok(());
    }
    self.dispatch(number, payload, sequence, now_ns)
  }

  /// Handles one message by its number.
  fn dispatch(
    &mut self,
    number: u8,
    payload: &[u8],
    sequence: u32,
    now_ns: u64,
  ) -> Result<(), Refusal> {
    match number {
      IGNORE | DEBUG | UNIMPLEMENTED => Ok(()),
      SERVICE_REQUEST => self.on_service_request(payload),
      KEXINIT => self.on_kexinit(payload, now_ns),
      NEWKEYS => self.on_newkeys(payload, now_ns),
      KEX_ECDH_INIT => self.on_ecdh_init(payload),
      FIRST_UPPER_LAYER..=u8::MAX => self.on_upper_layer(number, payload, sequence),
      _ if number == SERVICE_ACCEPT || is_negotiation_or_exchange(number) => {
        Err(Refusal::Unexpected { number })
      }
      _ => self.seal_transport(&unimplemented(sequence)),
    }
  }

  /// `SSH_MSG_DISCONNECT`: raises [`Event::Disconnected`] and closes.
  fn on_disconnect(&mut self, payload: &[u8]) -> Result<(), Refusal> {
    let message = |refusal| Refusal::Message {
      number: DISCONNECT,
      refusal,
    };
    let mut reader = Reader::new(payload);
    reader.byte().map_err(message)?;
    let reason = reader.uint32().map_err(message)?;
    let description = reader.string().map_err(message)?;
    reader.string().map_err(message)?;
    reader.finish().map_err(message)?;
    let description = String::from_utf8_lossy(description).into_owned();
    self.pending = Some(Event::Disconnected {
      reason,
      description,
    });
    self.phase = Phase::Closed;
    Ok(())
  }

  /// `SSH_MSG_SERVICE_REQUEST`: accepts `ssh-userauth`, once, after the first exchange and outside any other.
  fn on_service_request(&mut self, payload: &[u8]) -> Result<(), Refusal> {
    if !self.established || self.exchange.is_some() || self.service_accepted {
      return Err(Refusal::Unexpected {
        number: SERVICE_REQUEST,
      });
    }
    let message = |refusal| Refusal::Message {
      number: SERVICE_REQUEST,
      refusal,
    };
    let mut reader = Reader::new(payload);
    reader.byte().map_err(message)?;
    let service = reader.string().map_err(message)?;
    reader.finish().map_err(message)?;
    if service != USERAUTH_SERVICE.as_bytes() {
      return Err(Refusal::ServiceNotAvailable);
    }
    let mut accept = Vec::new();
    put_byte(&mut accept, SERVICE_ACCEPT);
    put_string(&mut accept, service).map_err(message)?;
    self.seal_transport(&accept)?;
    self.service_accepted = true;
    self.pending = Some(Event::ServiceAccepted);
    Ok(())
  }

  /// An upper-layer message: passed up once the service is accepted, except inside the client's exchange.
  fn on_upper_layer(&mut self, number: u8, payload: &[u8], sequence: u32) -> Result<(), Refusal> {
    let client_exchanging = self
      .exchange
      .as_ref()
      .is_some_and(|exchange| exchange.theirs.is_some());
    if !self.service_accepted || client_exchanging {
      return Err(Refusal::Unexpected { number });
    }
    self.pending = Some(Event::Message {
      payload: payload.to_vec(),
      sequence,
    });
    Ok(())
  }

  /// `SSH_MSG_KEXINIT`: the client's half of the negotiation, answered with the server's when the client began.
  fn on_kexinit(&mut self, payload: &[u8], now_ns: u64) -> Result<(), Refusal> {
    if self
      .exchange
      .as_ref()
      .is_some_and(|exchange| exchange.theirs.is_some())
    {
      return Err(Refusal::Unexpected { number: KEXINIT });
    }
    self.seen_kexinit = true;
    let initial = !self.established;
    let client = KexInit::read(payload).map_err(Refusal::KexInit)?;
    let negotiated = negotiate(&client, initial).map_err(Refusal::KexInit)?;
    if initial && negotiated.strict && self.packets_before_kexinit > 0 {
      return Err(Refusal::KexInitNotFirst);
    }
    if initial {
      self.strict = negotiated.strict;
    }
    if self.exchange.is_none() {
      if !initial {
        self.note_rekey(RekeyTrigger::Client, now_ns);
      }
      self.begin_exchange(false, now_ns)?;
    }
    let exchange = self
      .exchange
      .as_mut()
      .ok_or(Refusal::Unexpected { number: KEXINIT })?;
    exchange.theirs = Some(payload.to_vec());
    exchange.skip_guess = negotiated.skip_guessed_packet;
    exchange.negotiated = Some(negotiated);
    Ok(())
  }

  /// `SSH_MSG_KEX_ECDH_INIT`: computes the exchange, answers it, sends NEWKEYS and takes the server's new keys into use.
  fn on_ecdh_init(&mut self, payload: &[u8]) -> Result<(), Refusal> {
    let mut exchange = self.exchange.take().ok_or(Refusal::Unexpected {
      number: KEX_ECDH_INIT,
    })?;
    let ready = exchange.next_opener.is_none() && exchange.theirs.is_some();
    let negotiated = exchange
      .negotiated
      .filter(|_| ready)
      .ok_or(Refusal::Unexpected {
        number: KEX_ECDH_INIT,
      })?;
    let client_public = read_ecdh_init(payload).map_err(|refusal| Refusal::Message {
      number: KEX_ECDH_INIT,
      refusal,
    })?;
    let mut private = [0u8; X25519_LEN];
    self
      .entropy
      .fill(&mut private)
      .map_err(|_| Refusal::Entropy)?;
    let half = match negotiated.kex {
      KexMethod::Mlkem768X25519Sha256 => mlkem768x25519_server(&private, client_public),
      KexMethod::Curve25519Sha256 => curve25519_server(&private, client_public),
    };
    private.fill(0);
    let half = half.map_err(Refusal::Kex)?;
    let hash = self.exchange_hash_of(
      &exchange,
      client_public,
      &half.public,
      half.shared_secret.bytes(),
    )?;
    let session_id = *self.session_id.get_or_insert(hash);
    let signature = self
      .host_key
      .sign(&hash)
      .map_err(|refusal| Refusal::Kex(KexRefusal::Encoding(refusal)))?;
    let reply =
      ecdh_reply(self.host_key.public_blob(), &half.public, &signature).map_err(|refusal| {
        Refusal::Message {
          number: KEX_ECDH_REPLY,
          refusal,
        }
      })?;
    self.seal_transport(&reply)?;
    self.seal_transport(&[NEWKEYS])?;
    let keys = new_keys(
      half.shared_secret.bytes(),
      &hash,
      &session_id,
      negotiated.client_to_server,
      negotiated.server_to_client,
    );
    let mut sealer = Sealer::new(
      keys.server_to_client.cipher,
      keys.server_to_client.key.bytes(),
      keys.server_to_client.iv.bytes(),
    )
    .map_err(key_material)?;
    sealer.set_sequence(if self.strict {
      0
    } else {
      self.sealer.sequence()
    });
    self.sealer = sealer;
    if exchange.initial && negotiated.client_ext_info {
      self.send_extensions()?;
    }
    let keys_in = &keys.client_to_server;
    exchange.next_opener = Some(
      Opener::new(keys_in.cipher, keys_in.key.bytes(), keys_in.iv.bytes()).map_err(key_material)?,
    );
    self.exchange = Some(exchange);
    Ok(())
  }

  /// The exchange hash of `exchange` with the client's and server's public keys and the shared secret.
  fn exchange_hash_of(
    &self,
    exchange: &Exchange,
    client_public: &[u8],
    server_public: &[u8],
    shared_secret: &[u8],
  ) -> Result<[u8; HASH_LEN], Refusal> {
    let unexpected = Refusal::Unexpected {
      number: KEX_ECDH_INIT,
    };
    exchange_hash(&ExchangeInputs {
      client_identification: self.client_identification.as_deref().ok_or(unexpected)?,
      server_identification: self.identification.line(),
      client_kexinit: exchange.theirs.as_deref().ok_or(unexpected)?,
      server_kexinit: &exchange.ours,
      host_key: self.host_key.public_blob(),
      client_public,
      server_public,
      shared_secret,
    })
    .map_err(Refusal::Kex)
  }

  /// `SSH_MSG_NEWKEYS` from the client: its packets are opened under the new keys from here, and the exchange ends.
  fn on_newkeys(&mut self, payload: &[u8], now_ns: u64) -> Result<(), Refusal> {
    let unexpected = Refusal::Unexpected { number: NEWKEYS };
    let mut exchange = self.exchange.take().ok_or(unexpected)?;
    let mut opener = exchange.next_opener.take().ok_or(unexpected)?;
    let negotiated = exchange.negotiated.ok_or(unexpected)?;
    if payload.len() != 1 {
      return Err(Refusal::Message {
        number: NEWKEYS,
        refusal: WireRefusal::TrailingBytes { at: 1 },
      });
    }
    opener.set_sequence(if self.strict {
      0
    } else {
      self.opener.sequence()
    });
    self.opener = opener;
    self.key_installed_ns = now_ns;
    self.last_exchange_ns = now_ns.saturating_sub(exchange.started_ns);
    let algorithms = Algorithms {
      kex: negotiated.kex,
      client_to_server: negotiated.client_to_server,
      server_to_client: negotiated.server_to_client,
    };
    self.pending = Some(if exchange.initial {
      self.established = true;
      Event::Established(Established {
        session_id: self.session_id.ok_or(unexpected)?,
        strict: self.strict,
        client_ext_info: negotiated.client_ext_info,
        algorithms,
      })
    } else {
      Event::Rekeyed(algorithms)
    });
    Ok(())
  }

  /// Seals `SSH_MSG_EXT_INFO` with the configured extensions (RFC 8308 §2.3), if there are any.
  fn send_extensions(&mut self) -> Result<(), Refusal> {
    if self.extensions.is_empty() {
      return Ok(());
    }
    let message = |refusal| Refusal::Message {
      number: EXT_INFO,
      refusal,
    };
    let count = u32::try_from(self.extensions.len()).map_err(|_| {
      message(WireRefusal::TooLong {
        length: self.extensions.len(),
      })
    })?;
    let mut payload = Vec::new();
    put_byte(&mut payload, EXT_INFO);
    put_uint32(&mut payload, count);
    for extension in &self.extensions {
      put_string(&mut payload, extension.name.as_bytes()).map_err(message)?;
      put_string(&mut payload, &extension.value).map_err(message)?;
    }
    self.seal_transport(&payload)
  }

  /// Sends the server's KEXINIT and opens an exchange.
  fn begin_exchange(&mut self, initial: bool, now_ns: u64) -> Result<(), Refusal> {
    let mut cookie = [0u8; COOKIE_LEN];
    self
      .entropy
      .fill(&mut cookie)
      .map_err(|_| Refusal::Entropy)?;
    let ours = server_kexinit(&cookie, initial).map_err(|refusal| Refusal::Message {
      number: KEXINIT,
      refusal,
    })?;
    self.seal_transport(&ours)?;
    self.exchange = Some(Exchange {
      initial,
      ours,
      theirs: None,
      negotiated: None,
      skip_guess: false,
      next_opener: None,
      started_ns: now_ns,
    });
    Ok(())
  }

  /// Whether the server may start an exchange now: the service accepted, none under way, the connection open.
  fn may_start_rekey(&self) -> bool {
    self.service_accepted && self.exchange.is_none() && self.phase != Phase::Closed
  }

  /// When the key time runs out: the key time after the keys were taken into use, never sooner than the last exchange
  /// took.
  fn rekey_deadline_ns(&self) -> u64 {
    self
      .key_installed_ns
      .saturating_add(self.key_time_ns.max(self.last_exchange_ns))
  }

  /// Starts an exchange if either direction's key has reached its rekey point.
  fn rekey_if_due(&mut self, now_ns: u64) -> Result<(), Refusal> {
    let sent_due = self
      .seal_limits()
      .is_some_and(|limits| limits.due(self.sealer.usage()));
    let received_due = self
      .open_limits()
      .is_some_and(|limits| limits.due(self.opener.usage()));
    if self.may_start_rekey() && (sent_due || received_due) {
      self.note_rekey(RekeyTrigger::KeyUsage, now_ns);
      return self.begin_exchange(false, now_ns);
    }
    Ok(())
  }

  /// The limits of the server's key, once there is one.
  fn seal_limits(&self) -> Option<KeyLimits> {
    self
      .sealer
      .cipher()
      .map(|cipher| KeyLimits::for_cipher(cipher).capped(self.usage_cap))
  }

  /// The limits of the client's key, once there is one.
  fn open_limits(&self) -> Option<KeyLimits> {
    self
      .opener
      .cipher()
      .map(|cipher| KeyLimits::for_cipher(cipher).capped(self.usage_cap))
  }

  /// Seals one of the transport's own messages.
  fn seal_transport(&mut self, payload: &[u8]) -> Result<(), Refusal> {
    let number = payload.first().copied().unwrap_or(0);
    let too_long = Refusal::Message {
      number,
      refusal: WireRefusal::TooLong {
        length: payload.len(),
      },
    };
    let after = self.sealer.usage_after(payload.len()).ok_or(too_long)?;
    if self
      .seal_limits()
      .is_some_and(|limits| limits.exceeded_by(after))
    {
      return Err(Refusal::KeyExhausted {
        direction: Direction::ServerToClient,
      });
    }
    self.seal(payload).map_err(|refusal| match refusal {
      Sent::TooLarge { .. } => too_long,
      Sent::Fatal(refusal) => refusal,
    })
  }

  /// Seals `payload` into the output.
  fn seal(&mut self, payload: &[u8]) -> Result<(), Sent> {
    if !self.established && self.sealer.sequence() == u32::MAX {
      return Err(Sent::Fatal(Refusal::SequenceWrapBeforeKeys));
    }
    self
      .sealer
      .seal(payload, &mut self.entropy, &mut self.output)
      .map_err(|refusal| match refusal {
        SealRefusal::TooLarge { payload, ceiling } => Sent::TooLarge { payload, ceiling },
        SealRefusal::Entropy | SealRefusal::Cipher => Sent::Fatal(Refusal::Entropy),
      })
  }

  /// Closes on `refusal`, sending its disconnect, and gives it back.
  fn refuse(&mut self, refusal: Refusal) -> Refusal {
    self.fail(refusal);
    refusal
  }

  /// Closes on `refusal`, sending its disconnect.
  fn fail(&mut self, refusal: Refusal) {
    self.close(refusal.reason(), refusal.description());
  }

  /// Seals `SSH_MSG_DISCONNECT`, if the server's key can still seal it.
  fn send_disconnect(&mut self, reason: DisconnectReason, description: &str) {
    let mut payload = Vec::new();
    put_byte(&mut payload, DISCONNECT);
    put_uint32(&mut payload, reason.code());
    let encoded =
      put_string(&mut payload, description.as_bytes()).and_then(|()| put_string(&mut payload, b""));
    let after = self.sealer.usage_after(payload.len());
    let within = after.is_some_and(|after| {
      self
        .seal_limits()
        .is_none_or(|limits| !limits.exceeded_by(after))
    });
    let sealed = encoded.is_ok() && within && self.seal(&payload).is_ok();
    self.disconnect_sealed = Some(sealed);
  }
}

/// A seal that did not go through.
enum Sent {
  /// The packet would be too large; the connection goes on.
  TooLarge {
    /// The message's bytes.
    payload: u64,
    /// The largest packet sent.
    ceiling: u64,
  },
  /// The connection ends.
  Fatal(Refusal),
}

/// Whether `number` is one of the algorithm negotiation's or the key exchange methods' (RFC 4250 §4.1.2: 20 to 49), all
/// of which the server either handles in their place or refuses out of it.
fn is_negotiation_or_exchange(number: u8) -> bool {
  (KEXINIT..=LAST_KEX_METHOD).contains(&number)
}

/// Whether `number` is allowed during the first exchange under strict key exchange: KEXINIT, NEWKEYS and the exchange
/// methods' own messages (strict-kex draft §3.2).
fn is_exchange_message(number: u8) -> bool {
  matches!(
    number,
    KEXINIT | NEWKEYS | FIRST_KEX_METHOD..=LAST_KEX_METHOD
  )
}

/// `SSH_MSG_UNIMPLEMENTED` for the packet opened under `sequence` (RFC 4253 §11.4).
fn unimplemented(sequence: u32) -> Vec<u8> {
  let mut payload = Vec::new();
  put_byte(&mut payload, UNIMPLEMENTED);
  put_uint32(&mut payload, sequence);
  payload
}

/// A key the cipher refused, as the exchange's refusal.
fn key_material(_refusal: KeyMaterialRefusal) -> Refusal {
  Refusal::Kex(KexRefusal::PrivateKeyRejected)
}
