//! The server's transport over whole connections with the test client (A-594; RFC 4253; the strict-kex draft §3).

use proptest::prelude::*;

use crate::client::{Client, ClientOptions, put_string};
use crate::hostkey::HostKey;
use crate::ident::{IdentificationRefusal, software_version};
use crate::kex::KexRefusal;
use crate::kexinit::{KexInit, KexInitRefusal, List};
use crate::keylife::{RECOMMENDED_KEY_TIME_NS, UsageCap};
use crate::packet::{Cipher, MAX_PACKET_EVERY_PEER_ACCEPTS, PacketRefusal, Sealer};
use crate::refusal::{Direction, Refusal};
use crate::support::{FailingAfter, Seeded, hex_array};
use crate::transport::{Algorithms, ConfigRefusal, Event, SendRefusal, ServerTransport, TransportConfig};

const CHACHA: &str = "chacha20-poly1305@openssh.com";
const GCM: &str = "aes256-gcm@openssh.com";

/// RFC 8032 §7.1 test 1's key as the host key.
fn host_key() -> HostKey {
  HostKey::ed25519_from_seed(&hex_array("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60")).unwrap()
}

fn config() -> TransportConfig {
  TransportConfig {
    packet_cap: MAX_PACKET_EVERY_PEER_ACCEPTS,
    banner: Vec::new(),
    comments: None,
    key_time_ns: RECOMMENDED_KEY_TIME_NS,
    usage_cap: UsageCap::default(),
  }
}

/// A server and a client, and what passed between them.
struct Session {
  server: ServerTransport<Seeded>,
  client: Client,
  events: Vec<Event>,
  transcript: Vec<u8>,
  now: u64,
}

impl Session {
  fn new(config: TransportConfig, options: ClientOptions) -> Self {
    let server = ServerTransport::new(config, host_key(), Seeded::new(1), 0).unwrap();
    Self { server, client: Client::new(options), events: Vec::new(), transcript: Vec::new(), now: 0 }
  }

  /// Moves the server's output to the client; whether there was any.
  fn to_client(&mut self) -> bool {
    let out = self.server.pending_output().to_vec();
    self.server.consume_output(out.len());
    self.transcript.extend_from_slice(&out);
    self.client.feed(&out);
    !out.is_empty()
  }

  /// Passes `input` to the server, taking its events and moving its output whenever it waits on them.
  fn to_server(&mut self, input: &[u8]) -> Result<(), Refusal> {
    let mut rest = input;
    let mut stalls = 0;
    while !rest.is_empty() && !self.server.is_closed() {
      let used = self.server.receive(rest, self.now)?;
      rest = &rest[used..];
      let mut moved = used > 0;
      while let Some(event) = self.server.poll_event() {
        self.events.push(event);
        moved = true;
      }
      if !moved {
        assert!(self.to_client(), "the server took nothing, raised nothing and wrote nothing");
        stalls += 1;
        assert!(stalls < 10_000);
      }
    }
    Ok(())
  }

  /// Moves bytes both ways until neither side sends anything.
  fn pump(&mut self) -> Result<(), Refusal> {
    for _ in 0..1_000 {
      let sent = self.to_client();
      let input = self.client.take_output();
      if !sent && input.is_empty() {
        return Ok(());
      }
      self.to_server(&input)?;
    }
    panic!("the session never went quiet");
  }

  /// A session that has finished its first exchange and had `ssh-userauth` accepted.
  fn serving(config: TransportConfig, options: ClientOptions) -> Self {
    let mut session = Self::new(config, options);
    session.pump().unwrap();
    session.client.send(&service_request("ssh-userauth"));
    session.pump().unwrap();
    assert!(session.events.contains(&Event::ServiceAccepted));
    session
  }

  /// The last message the client opened.
  fn last_received(&self) -> &[u8] {
    self.client.received.last().expect("the client received a message")
  }
}

fn service_request(service: &str) -> Vec<u8> {
  let mut payload = vec![5];
  put_string(&mut payload, service.as_bytes());
  payload
}

/// The reason a disconnect payload carries.
fn disconnect_reason(payload: &[u8]) -> u32 {
  assert_eq!(payload[0], 1, "a disconnect: {payload:?}");
  u32::from_be_bytes(payload[1..5].try_into().unwrap())
}

/// A clear packet of `payload`, as a client sends before the first NEWKEYS.
fn clear_packet(payload: &[u8]) -> Vec<u8> {
  let mut out = Vec::new();
  Sealer::clear().seal(payload, &mut Seeded::new(5), &mut out).unwrap();
  out
}

/// A-594: do connect a client like OpenSSH 10's under each cipher, with and without strict key exchange, ask for
/// `ssh-userauth`, and pass a message each way. Expect the first exchange to end with the session id the client computed
/// and verified the host key's signature over, the cipher and strictness agreed, the service accepted, the client's
/// message raised, and the server's opened by the client.
#[test]
fn a_stock_client_connects_under_each_cipher_and_strictness() {
  for (cipher_name, cipher, strict) in [
    (CHACHA, Cipher::ChaCha20Poly1305, true),
    (CHACHA, Cipher::ChaCha20Poly1305, false),
    (GCM, Cipher::Aes256Gcm, true),
    (GCM, Cipher::Aes256Gcm, false),
  ] {
    let mut session = Session::new(config(), ClientOptions::openssh_like(cipher_name, strict));
    session.pump().unwrap();
    let session_id = session.client.session_id.expect("the client verified the exchange");
    let algorithms = Algorithms { client_to_server: cipher, server_to_client: cipher };
    assert_eq!(
      session.events,
      [Event::Established(crate::transport::Established { session_id, strict, client_ext_info: true, algorithms })]
    );
    assert_eq!(session.server.session_id(), Some(&session_id));
    assert_eq!(session.client.host_key.as_deref(), Some(host_key().public_blob()));
    assert_eq!(session.client.strict, strict);
    assert_eq!(session.server.client_identification(), Some(&b"SSH-2.0-OpenSSH_10.2"[..]));
    session.client.send(&service_request("ssh-userauth"));
    session.pump().unwrap();
    assert_eq!(session.events.last(), Some(&Event::ServiceAccepted));
    let mut accept = vec![6];
    put_string(&mut accept, b"ssh-userauth");
    assert_eq!(session.last_received(), &accept[..]);
    session.client.send(&[50, 1, 2, 3]);
    session.pump().unwrap();
    assert_eq!(session.events.last(), Some(&Event::Message(vec![50, 1, 2, 3])));
    session.server.send(&[52], session.now).unwrap();
    session.pump().unwrap();
    assert_eq!(session.last_received(), &[52]);
  }
}

/// A-594: do connect a client that prefers AES-GCM for what it sends and ChaCha20-Poly1305 for what it receives. Expect
/// each direction under its own cipher, and messages passing both ways.
#[test]
fn each_direction_runs_its_own_cipher() {
  let mut options = ClientOptions::openssh_like(GCM, true);
  options.ciphers_server_to_client = vec![CHACHA.to_string(), GCM.to_string()];
  let mut session = Session::serving(config(), options);
  let Event::Established(established) = &session.events[0] else { panic!("first event {:?}", session.events[0]) };
  assert_eq!(established.algorithms, Algorithms { client_to_server: Cipher::Aes256Gcm, server_to_client: Cipher::ChaCha20Poly1305 });
  session.client.send(&[94, 9]);
  session.pump().unwrap();
  assert_eq!(session.events.last(), Some(&Event::Message(vec![94, 9])));
  session.server.send(&[94, 8], 0).unwrap();
  session.pump().unwrap();
  assert_eq!(session.last_received(), &[94, 8]);
}

/// A-594 (RFC 4253 §4.2): do connect with a banner line and comments set. Expect the client to read the banner line
/// first, then `SSH-2.0-slates_<version> git`.
#[test]
fn the_banner_comes_before_the_identification() {
  let mut config = config();
  config.banner = vec!["slates: git, Mercurial and Subversion".to_string()];
  config.comments = Some("git".to_string());
  let session = Session::serving(config, ClientOptions::openssh_like(CHACHA, true));
  assert_eq!(session.client.banner_lines, [b"slates: git, Mercurial and Subversion".to_vec()]);
  let line = format!("SSH-2.0-{} git", software_version());
  assert_eq!(session.client.server_identification.as_deref(), Some(line.as_bytes()));
}

/// A-594 hostile, Terrapin (the strict-kex draft §3.2: "MUST verify that the SSH_MSG_KEXINIT was the first message
/// received"): do send `SSH_MSG_IGNORE` before a strict client's KEXINIT. Expect the connection refused once the
/// KEXINIT shows strict key exchange, and a disconnect, reason 2, the client can read.
#[test]
fn strict_refuses_a_message_before_the_kexinit() {
  let mut session = Session::new(config(), ClientOptions::openssh_like(CHACHA, true));
  let first = session.client.take_output();
  let line_end = first.windows(2).position(|pair| pair == b"\r\n").unwrap() + 2;
  let mut input = first[..line_end].to_vec();
  input.extend_from_slice(&clear_packet(&[2, 0, 0, 0, 0]));
  input.extend_from_slice(&first[line_end..]);
  assert_eq!(session.to_server(&input), Err(Refusal::KexInitNotFirst));
  assert!(session.server.is_closed());
  assert_eq!(session.server.disconnect_sealed(), Some(true));
  session.to_client();
  assert_eq!(disconnect_reason(session.last_received()), 2);
}

/// A-594 hostile, Terrapin (the strict-kex draft §3.2: "MUST terminate the connection if they receive a non-KEX message
/// during the initial key exchange"): do send `SSH_MSG_IGNORE` between the KEXINITs and the exchange, from a strict
/// client and from one that is not. Expect the strict connection refused at the IGNORE, and the other to finish its
/// exchange, as RFC 4253 lets it.
#[test]
fn strict_refuses_a_message_inside_the_first_exchange() {
  for strict in [true, false] {
    let mut session = Session::new(config(), ClientOptions::openssh_like(CHACHA, strict));
    let first = session.client.take_output();
    session.to_server(&first).unwrap();
    session.client.send(&[2, 0, 0, 0, 0]);
    let outcome = session.pump();
    if strict {
      assert_eq!(outcome, Err(Refusal::Unexpected { number: 2 }));
    } else {
      outcome.unwrap();
      assert!(matches!(session.events[0], Event::Established(_)));
    }
  }
}

/// A-594 hostile (RFC 4253 §7.1; the strict-kex draft §3.2: "A server implementing this extension MUST only accept it
/// once"): do send `SSH_MSG_KEX_ECDH_INIT` twice, and `SSH_MSG_NEWKEYS` before any. Expect each refused as unexpected.
#[test]
fn exchange_messages_out_of_place_are_refused() {
  let mut session = Session::new(config(), ClientOptions::openssh_like(CHACHA, true));
  let first = session.client.take_output();
  session.to_server(&first).unwrap();
  session.to_client();
  let init = session.client.take_output();
  let mut twice = init.clone();
  twice.extend_from_slice(&init);
  assert_eq!(session.to_server(&twice), Err(Refusal::Unexpected { number: 30 }));

  let mut session = Session::new(config(), ClientOptions::openssh_like(GCM, false));
  let first = session.client.take_output();
  session.to_server(&first).unwrap();
  session.client.send(&[21]);
  let early = session.client.take_output();
  assert_eq!(session.to_server(&early), Err(Refusal::Unexpected { number: 21 }));
}

/// A-594 (RFC 4253 §10): do ask for `ssh-connection` before authentication, ask for `ssh-userauth` twice, and send an
/// upper-layer message before any service. Expect the first refused as no such service, with disconnect reason 7 the
/// client opens under the new keys, and the others refused as unexpected.
#[test]
fn services_are_accepted_once_and_only_ssh_userauth() {
  let mut session = Session::new(config(), ClientOptions::openssh_like(CHACHA, true));
  session.pump().unwrap();
  session.client.send(&service_request("ssh-connection"));
  assert_eq!(session.pump(), Err(Refusal::ServiceNotAvailable));
  session.to_client();
  assert_eq!(disconnect_reason(session.last_received()), 7);

  let mut session = Session::serving(config(), ClientOptions::openssh_like(CHACHA, true));
  session.client.send(&service_request("ssh-userauth"));
  assert_eq!(session.pump(), Err(Refusal::Unexpected { number: 5 }));

  let mut session = Session::new(config(), ClientOptions::openssh_like(GCM, true));
  session.pump().unwrap();
  session.client.send(&[50, 0]);
  assert_eq!(session.pump(), Err(Refusal::Unexpected { number: 50 }));
}

/// A-594 (RFC 4253 §11.4; the strict-kex draft §3.3): do send an unrecognized message number after the service is
/// accepted, under strict key exchange and without it. Expect `SSH_MSG_UNIMPLEMENTED` naming that packet's sequence
/// number: counted from zero after the client's NEWKEYS when strict, carried on from the first packet when not.
#[test]
fn unrecognized_messages_are_answered_with_their_sequence_number() {
  for strict in [true, false] {
    let mut session = Session::serving(config(), ClientOptions::openssh_like(CHACHA, strict));
    let sequence = session.client.send_sequence();
    assert_eq!(sequence, if strict { 1 } else { 4 });
    session.client.send(&[15, 1]);
    session.pump().unwrap();
    let mut expected = vec![3];
    expected.extend_from_slice(&sequence.to_be_bytes());
    assert_eq!(session.last_received(), &expected[..]);
  }
}

/// A-594 (RFC 4253 §11.1): do send the client's disconnect. Expect it raised with its reason and description, the
/// connection closed, and nothing read after.
#[test]
fn a_client_disconnect_is_raised_and_closes() {
  let mut session = Session::serving(config(), ClientOptions::openssh_like(CHACHA, true));
  let mut payload = vec![1];
  payload.extend_from_slice(&11u32.to_be_bytes());
  put_string(&mut payload, b"bye");
  put_string(&mut payload, b"");
  session.client.send(&payload);
  session.pump().unwrap();
  assert_eq!(session.events.last(), Some(&Event::Disconnected { reason: 11, description: "bye".to_string() }));
  assert!(session.server.is_closed());
  assert_eq!(session.server.receive(&[0; 64], 0), Ok(0));
  assert_eq!(session.server.send(&[94], 0), Err(SendRefusal::Closed));
}

/// A-594 hostile (RFC 4253 §7.1, "If there is no such algorithm, both sides MUST disconnect"): do connect a client
/// offering only AES-CTR. Expect the KEXINIT refused for the client-to-server ciphers, with disconnect reason 3 in the
/// clear.
#[test]
fn a_client_sharing_no_cipher_is_refused() {
  let mut options = ClientOptions::openssh_like(CHACHA, true);
  options.ciphers_client_to_server = vec!["aes128-ctr".to_string()];
  let mut session = Session::new(config(), options);
  assert_eq!(session.pump(), Err(Refusal::KexInit(KexInitRefusal::NoCommonAlgorithm(List::CipherClientToServer))));
  session.to_client();
  assert_eq!(disconnect_reason(session.last_received()), 3);
}

/// A-594 hostile (RFC 8731 §3): do send an exchange whose public key is the all-zero point. Expect the exchange refused
/// and disconnect reason 3.
#[test]
fn a_small_order_client_key_is_refused() {
  let mut session = Session::new(config(), ClientOptions::openssh_like(CHACHA, true));
  let first = session.client.take_output();
  session.to_server(&first).unwrap();
  let mut init = vec![30];
  put_string(&mut init, &[0; 32]);
  session.client.send(&init);
  let bad = session.client.take_output();
  assert_eq!(session.to_server(&bad), Err(Refusal::Kex(KexRefusal::SharedSecretRejected)));
  session.to_client();
  assert_eq!(disconnect_reason(session.last_received()), 3);
}

/// A-594 hostile: do flip one bit of a client's packet under each cipher after the service is accepted. Expect the
/// packet refused as failing its tag, and disconnect reason 5 the client opens.
#[test]
fn a_forged_packet_is_refused_with_mac_error() {
  for cipher in [CHACHA, GCM] {
    let mut session = Session::serving(config(), ClientOptions::openssh_like(cipher, true));
    session.client.send(&[50, 7, 7, 7, 7, 7, 7, 7]);
    let mut forged = session.client.take_output();
    let middle = forged.len() / 2;
    forged[middle] ^= 0x04;
    assert_eq!(session.to_server(&forged), Err(Refusal::Packet(PacketRefusal::TagMismatch)));
    session.to_client();
    assert_eq!(disconnect_reason(session.last_received()), 5);
  }
}

/// A-594 hostile: do send, after a valid identification, a length of 1 MiB and one of `u32::MAX`, four bytes and
/// nothing after. Expect each refused from those four bytes, before any body.
#[test]
fn a_packet_past_the_cap_is_refused_from_its_length() {
  for head in [0x0010_0004u32, u32::MAX] {
    let mut server = ServerTransport::new(config(), host_key(), Seeded::new(1), 0).unwrap();
    let mut input = b"SSH-2.0-OpenSSH_10.2\r\n".to_vec();
    input.extend_from_slice(&head.to_be_bytes());
    let refusal = server.receive(&input, 0).unwrap_err();
    assert!(
      matches!(refusal, Refusal::Packet(PacketRefusal::LengthPastCap { .. } | PacketRefusal::LengthUnaligned { .. })),
      "{refusal:?}"
    );
    assert!(server.is_closed());
  }
}

/// A-594 hostile (RFC 4253 §4.2): do send an HTTP request, and an identification line that never ends. Expect the first
/// refused as a line before the identification and the second as too long, each with the server's disconnect queued
/// after its own first KEXINIT.
#[test]
fn bad_identifications_are_refused() {
  let mut server = ServerTransport::new(config(), host_key(), Seeded::new(1), 0).unwrap();
  let before = server.pending_output().len();
  assert_eq!(
    server.receive(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n", 0),
    Err(Refusal::Identification(IdentificationRefusal::LineBeforeIdentification))
  );
  assert!(server.pending_output().len() > before);
  let mut server = ServerTransport::new(config(), host_key(), Seeded::new(1), 0).unwrap();
  let mut endless = b"SSH-2.0-".to_vec();
  endless.extend_from_slice(&[b'x'; 400]);
  assert_eq!(server.receive(&endless, 0), Err(Refusal::Identification(IdentificationRefusal::TooLong)));
}

/// A-594 (RFC 4253 §9): do start a key exchange from the client once the service is accepted, then pass messages. Expect
/// the server's KEXINIT without the strict markers, the exchange raised as a rekey, a new exchange hash with the session
/// id unchanged, and messages passing both ways under the new keys.
#[test]
fn a_client_rekey_keeps_the_session_id() {
  for cipher in [CHACHA, GCM] {
    let mut session = Session::serving(config(), ClientOptions::openssh_like(cipher, true));
    let session_id = *session.server.session_id().unwrap();
    session.client.start_rekey();
    session.pump().unwrap();
    assert!(session.events.iter().any(|event| matches!(event, Event::Rekeyed(_))));
    assert_eq!(session.client.server_newkeys, 2);
    assert_eq!(session.client.exchange_hashes.len(), 2);
    assert_ne!(session.client.exchange_hashes[0], session.client.exchange_hashes[1]);
    assert_eq!(session.server.session_id(), Some(&session_id));
    let server_kexinit = session.client.server_kexinit.clone().unwrap();
    let read = KexInit::read(&server_kexinit).unwrap();
    assert!(!read.list(List::Kex).contains("kex-strict-s-v00@openssh.com"));
    session.client.send(&[94, 1]);
    session.server.send(&[94, 2], 0).unwrap();
    session.pump().unwrap();
    assert_eq!(session.events.last(), Some(&Event::Message(vec![94, 1])));
    assert_eq!(session.last_received(), &[94, 2]);
  }
}

/// A-594 (RFC 4253 §9; `keylife`): do serve with a key time of one microsecond, poll before and at the deadline, send
/// while the exchange runs, and finish it. Expect no exchange before the deadline, the server's KEXINIT at it, sends
/// refused as rekeying until the exchange ends, and sends accepted after.
#[test]
fn the_server_rekeys_when_the_key_time_runs_out() {
  let mut config = config();
  config.key_time_ns = 1_000;
  let mut session = Session::serving(config, ClientOptions::openssh_like(CHACHA, true));
  assert_eq!(session.server.next_timer_ns(), Some(1_000));
  session.server.poll_timers(999).unwrap();
  assert!(session.server.pending_output().is_empty());
  session.server.poll_timers(1_000).unwrap();
  assert!(!session.server.pending_output().is_empty());
  assert_eq!(session.server.next_timer_ns(), None);
  assert_eq!(session.server.send(&[94], 1_000), Err(SendRefusal::Rekeying));
  session.pump().unwrap();
  assert!(matches!(session.events.last(), Some(Event::Rekeyed(_))));
  session.server.send(&[94], 1_000).unwrap();
  session.pump().unwrap();
  assert_eq!(session.last_received(), &[94]);
}

/// A-594 (`keylife`: never rekey sooner than the last exchange took): do serve with a key time of one microsecond where
/// the first exchange took five. Expect the next deadline five microseconds after the keys were taken into use.
#[test]
fn the_key_time_is_never_shorter_than_the_last_exchange() {
  let mut config = config();
  config.key_time_ns = 1_000;
  let mut session = Session::new(config, ClientOptions::openssh_like(GCM, true));
  session.now = 5_000;
  session.pump().unwrap();
  session.client.send(&service_request("ssh-userauth"));
  session.pump().unwrap();
  assert_eq!(session.server.next_timer_ns(), Some(10_000));
}

/// A-594 (RFC 4344 §3.1; `keylife`): do serve under a cap of 8 packets a key and send until the server's key reaches
/// half of it. Expect the server's KEXINIT then, the next send refused as rekeying, and the exchange finished with the
/// key's usage started afresh.
#[test]
fn the_server_rekeys_at_half_its_keys_limit() {
  let mut config = config();
  config.usage_cap = UsageCap { packets: Some(8), blocks: None };
  let mut session = Session::serving(config, ClientOptions::openssh_like(GCM, true));
  assert_eq!(session.server.key_usage().sent.packets, 1);
  session.server.send(&[94, 1], 0).unwrap();
  session.server.send(&[94, 2], 0).unwrap();
  session.server.send(&[94, 3], 0).unwrap();
  assert_eq!(session.server.send(&[94, 4], 0), Err(SendRefusal::Rekeying));
  session.pump().unwrap();
  assert!(matches!(session.events.last(), Some(Event::Rekeyed(_))));
  assert_eq!(session.server.key_usage().sent.packets, 0);
  session.server.send(&[94, 4], 0).unwrap();
  session.pump().unwrap();
  assert_eq!(session.last_received(), &[94, 4]);
}

/// A-594 hostile (RFC 4344 §3.1): do serve under a cap of 8 packets a key to a client that ignores the server's KEXINIT
/// and keeps sending. Expect its messages taken up to the cap, and the ninth refused as the client's key exhausted.
#[test]
fn a_client_that_never_rekeys_exhausts_its_key() {
  let mut config = config();
  config.usage_cap = UsageCap { packets: Some(8), blocks: None };
  let mut options = ClientOptions::openssh_like(CHACHA, true);
  options.answer_kexinit = false;
  let mut session = Session::serving(config, options);
  assert_eq!(session.server.key_usage().received.packets, 1);
  for index in 0..7u8 {
    session.client.send(&[94, index]);
    session.pump().unwrap();
  }
  assert_eq!(session.server.key_usage().received.packets, 8);
  session.client.send(&[94, 8]);
  assert_eq!(session.pump(), Err(Refusal::KeyExhausted { direction: Direction::ClientToServer }));
}

/// A-594: do pass the client's NEWKEYS and its service request in one read. Expect the server to stop after the NEWKEYS
/// while the established event waits, and take the request once it is read.
#[test]
fn input_waits_while_an_event_does() {
  let mut session = Session::new(config(), ClientOptions::openssh_like(CHACHA, true));
  let first = session.client.take_output();
  session.to_server(&first).unwrap();
  session.to_client();
  let init = session.client.take_output();
  session.to_server(&init).unwrap();
  session.to_client();
  session.client.send(&service_request("ssh-userauth"));
  let both = session.client.take_output();
  let used = session.server.receive(&both, 0).unwrap();
  assert!(used < both.len());
  assert!(matches!(session.server.poll_event(), Some(Event::Established(_))));
  assert_eq!(session.server.receive(&both[used..], 0), Ok(both.len() - used));
  assert_eq!(session.server.poll_event(), Some(Event::ServiceAccepted));
}

/// A-594: do send 1200 unrecognized messages at once without reading the server's answers. Expect the server to stop
/// reading once a packet cap of answers is unwritten, and to take the rest once they are written.
#[test]
fn input_waits_while_the_output_is_full() {
  let mut session = Session::serving(config(), ClientOptions::openssh_like(CHACHA, true));
  for _ in 0..1_200 {
    session.client.send(&[15]);
  }
  let flood = session.client.take_output();
  let used = session.server.receive(&flood, 0).unwrap();
  assert!(used < flood.len());
  assert!(session.server.pending_output().len() >= MAX_PACKET_EVERY_PEER_ACCEPTS);
  assert_eq!(session.server.send(&[94], 0), Err(SendRefusal::OutputFull));
  session.to_server(&flood[used..]).unwrap();
  session.to_client();
  assert_eq!(session.client.received.iter().filter(|payload| payload[0] == 3).count(), 1_200);
}

/// A-594 determinism: do run the same connection twice from the same seeds. Expect the server's bytes identical.
#[test]
fn a_connection_from_the_same_seeds_is_byte_identical() {
  let run = || {
    let mut session = Session::serving(config(), ClientOptions::openssh_like(GCM, true));
    session.server.send(&[94, 1, 2], 0).unwrap();
    session.pump().unwrap();
    session.transcript
  };
  assert_eq!(run(), run());
}

/// A-594 (RFC 4253 §7): do connect a client that guesses right (its first exchange packet sent with its KEXINIT) and one
/// that guesses wrong (a guessed packet for a method the server does not run). Expect both to finish the exchange: the
/// right guess used, the wrong one ignored.
#[test]
fn guesses_are_used_or_ignored() {
  let mut right = ClientOptions::openssh_like(CHACHA, true);
  right.first_kex_packet_follows = true;
  let mut session = Session::new(config(), right);
  session.pump().unwrap();
  assert!(matches!(session.events[0], Event::Established(_)));

  let mut wrong = ClientOptions::openssh_like(CHACHA, true);
  wrong.kex.insert(0, "mlkem768x25519-sha256".to_string());
  wrong.first_kex_packet_follows = true;
  let mut guess = vec![30];
  put_string(&mut guess, &[0x77; 1216]);
  wrong.guess = Some(guess);
  let mut session = Session::new(config(), wrong);
  session.pump().unwrap();
  assert!(matches!(session.events[0], Event::Established(_)));
}

/// A-594: do set up a transport with a packet cap below RFC 4253 §6.1's 35000 bytes, and with entropy that fails.
/// Expect each refused by its kind.
#[test]
fn bad_configurations_are_refused() {
  let mut small = config();
  small.packet_cap = MAX_PACKET_EVERY_PEER_ACCEPTS - 1;
  assert_eq!(
    ServerTransport::new(small, host_key(), Seeded::new(1), 0).unwrap_err(),
    ConfigRefusal::PacketCapBelowStandard { cap: 34_999, floor: 35_000 }
  );
  assert_eq!(ServerTransport::new(config(), host_key(), FailingAfter::new(0), 0).unwrap_err(), ConfigRefusal::Entropy);
}

/// A-594: do send from the layers above before the service, a transport number, an empty message and one too large.
/// Expect each refused by its kind, the connection going on.
#[test]
fn sends_the_transport_cannot_take_are_refused() {
  let mut session = Session::new(config(), ClientOptions::openssh_like(CHACHA, true));
  session.pump().unwrap();
  assert_eq!(session.server.send(&[94], 0), Err(SendRefusal::NotEstablished));
  for cipher in [CHACHA, GCM] {
    let mut session = Session::serving(config(), ClientOptions::openssh_like(cipher, true));
    assert_eq!(session.server.send(&[5], 0), Err(SendRefusal::NotUpperLayer { number: 5 }));
    assert_eq!(session.server.send(&[], 0), Err(SendRefusal::Empty));
    let large = vec![94; MAX_PACKET_EVERY_PEER_ACCEPTS];
    assert_eq!(
      session.server.send(&large, 0),
      Err(SendRefusal::TooLarge { payload: MAX_PACKET_EVERY_PEER_ACCEPTS as u64, ceiling: MAX_PACKET_EVERY_PEER_ACCEPTS as u64 })
    );
    session.server.send(&[94], 0).unwrap();
    session.pump().unwrap();
    assert_eq!(session.last_received(), &[94]);
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A-594 hostile: do send any bytes after a valid identification, whole and in pieces. Expect the transport to take
  /// them or refuse them, typed, never panicking, and to close on a refusal.
  #[test]
  fn any_bytes_are_taken_or_refused(bytes in proptest::collection::vec(any::<u8>(), 0..2_000), cut in 1usize..64) {
    let mut server = ServerTransport::new(config(), host_key(), Seeded::new(3), 0).unwrap();
    let mut input = b"SSH-2.0-OpenSSH_10.2\r\n".to_vec();
    input.extend_from_slice(&bytes);
    for piece in input.chunks(cut) {
      let mut rest = piece;
      while !rest.is_empty() && !server.is_closed() {
        match server.receive(rest, 0) {
          Ok(used) => rest = &rest[used..],
          Err(_) => prop_assert!(server.is_closed()),
        }
        while server.poll_event().is_some() {}
        let written = server.pending_output().len();
        server.consume_output(written);
      }
    }
  }
}
