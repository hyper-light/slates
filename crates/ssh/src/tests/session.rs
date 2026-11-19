//! Whole connections through [`Session`] (A-633; RFC 4252, RFC 4254, RFC 8308): the test client of `client.rs`, its
//! authentication and channel messages built here from the RFCs' layouts.

use crate::channel::max_data;
use crate::client::{Client, ClientOptions, put_string, read_string};
use crate::command::Command;
use crate::hostkey::HostKey;
use crate::keylife::{RECOMMENDED_KEY_TIME_NS, UsageCap};
use crate::packet::MAX_PACKET_EVERY_PEER_ACCEPTS;
use crate::session::{Session, SessionConfig, SessionEvent, SessionRefusal, SessionUse};
use crate::support::{Seeded, hex_array};
use crate::transport::TransportConfig;
use crate::userauth::{AuthRefusal, Authenticated};
use crate::userauth_tests::{Holders, ed_signature, first_key, query, request, signed};

/// The session channel's window in these tests, unless a test sets its own.
const WINDOW: u32 = 1 << 18;
/// The client's channel number.
const CLIENT_CHANNEL: u32 = 41;
/// `SSH_MSG_CHANNEL_SUCCESS`, `SSH_MSG_CHANNEL_FAILURE` and `SSH_MSG_CHANNEL_OPEN_FAILURE`.
const SUCCESS: u8 = 99;
const FAILURE: u8 = 100;
const OPEN_FAILURE: u8 = 92;

fn host_key() -> HostKey {
  HostKey::ed25519_from_seed(&hex_array(
    "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
  ))
  .unwrap()
}

fn config() -> SessionConfig {
  SessionConfig {
    transport: TransportConfig {
      packet_cap: MAX_PACKET_EVERY_PEER_ACCEPTS,
      banner: Vec::new(),
      comments: None,
      key_time_ns: RECOMMENDED_KEY_TIME_NS,
      usage_cap: UsageCap::default(),
      extensions: Vec::new(),
    },
    banner: None,
    failure_limit: 5,
    channel_window: WINDOW,
  }
}

/// The tests' store: user `git` holds the first test key as principal 7.
fn store() -> Holders {
  Holders {
    entries: vec![(b"git".to_vec(), first_key().blob, 7, 1)],
  }
}

/// A server and the test client, and what passed between them.
struct Pair {
  server: Session<Seeded, Holders>,
  client: Client,
  events: Vec<SessionEvent<u32>>,
  now: u64,
}

impl Pair {
  fn new(config: SessionConfig, options: ClientOptions) -> Self {
    let server = Session::new(config, host_key(), Seeded::new(1), store(), 0).unwrap();
    Self {
      server,
      client: Client::new(options),
      events: Vec::new(),
      now: 0,
    }
  }

  /// Moves the server's output to the client; whether there was any.
  fn to_client(&mut self) -> bool {
    let out = self.server.pending_output().to_vec();
    self.server.consume_output(out.len()).unwrap();
    self.client.feed(&out);
    !out.is_empty()
  }

  /// Takes the server's waiting events.
  fn take_events(&mut self) -> bool {
    let mut any = false;
    while let Some(event) = self.server.poll_event() {
      self.events.push(event);
      any = true;
    }
    any
  }

  /// Passes `input` to the server, taking its events and moving its output whenever it waits on them.
  fn to_server(&mut self, input: &[u8]) -> Result<(), SessionRefusal> {
    let mut rest = input;
    let mut stalls = 0u32;
    while !rest.is_empty() && !self.server.is_closed() {
      let used = self.server.receive(rest, self.now)?;
      rest = rest.get(used..).unwrap_or(&[]);
      let moved = self.take_events() || used > 0;
      if !moved && (!self.to_client() || stalls > 10_000) {
        break;
      }
      stalls += u32::from(!moved);
    }
    Ok(())
  }

  /// Moves bytes both ways until neither side sends anything.
  fn pump(&mut self) -> Result<(), SessionRefusal> {
    for _ in 0..1_000 {
      let sent = self.to_client();
      let input = self.client.take_output();
      if !sent && input.is_empty() {
        self.take_events();
        return Ok(());
      }
      self.to_server(&input)?;
    }
    Ok(())
  }

  /// Sends `payload` from the client and pumps.
  fn exchange(&mut self, payload: &[u8]) -> Result<(), SessionRefusal> {
    self.client.send(payload);
    self.pump()
  }

  /// The last payload the client opened.
  fn last(&self) -> Vec<u8> {
    self.client.received.last().cloned().unwrap_or_default()
  }
}

/// A session whose client finished its first exchange and had `ssh-userauth` accepted.
fn serving(config: SessionConfig, options: ClientOptions) -> Pair {
  let mut pair = Pair::new(config, options);
  pair.pump().unwrap();
  let mut service = vec![5];
  put_string(&mut service, b"ssh-userauth");
  pair.exchange(&service).unwrap();
  pair
}

/// A session whose client authenticated as `git` with the first test key: the probe, the query and the signature.
fn authenticated(config: SessionConfig) -> Pair {
  let mut pair = serving(
    config,
    ClientOptions::openssh_like("chacha20-poly1305@openssh.com", true),
  );
  let key = first_key();
  pair.client.send(&request(b"git", "none"));
  pair.client.send(&query(b"git", "ssh-ed25519", &key.blob));
  let session_id = pair.client.session_id.unwrap();
  pair
    .exchange(&signed(
      b"git",
      "ssh-ed25519",
      &key.blob,
      &ed_signature(&key, &session_id, b"git"),
    ))
    .unwrap();
  pair
}

/// `SSH_MSG_CHANNEL_OPEN` of `kind` from the client's channel, with its window and maximum packet.
fn open(kind: &str, window: u32, max_packet: u32) -> Vec<u8> {
  let mut payload = vec![90];
  put_string(&mut payload, kind.as_bytes());
  payload.extend_from_slice(&CLIENT_CHANNEL.to_be_bytes());
  payload.extend_from_slice(&window.to_be_bytes());
  payload.extend_from_slice(&max_packet.to_be_bytes());
  payload
}

/// Opens a session and gives back the server's channel number and the window and packet it offered.
fn open_session(pair: &mut Pair, window: u32, max_packet: u32) -> (u32, u32, u32) {
  pair.exchange(&open("session", window, max_packet)).unwrap();
  let confirmation = pair.last();
  assert_eq!(
    confirmation[..5],
    [&[91][..], &CLIENT_CHANNEL.to_be_bytes()].concat()
  );
  let word = |at: usize| u32::from_be_bytes(confirmation[at..at + 4].try_into().unwrap());
  (word(5), word(9), word(13))
}

/// `SSH_MSG_CHANNEL_REQUEST` of `kind` to `channel`, its type-specific strings after.
fn channel_request(channel: u32, kind: &str, want_reply: bool, fields: &[&[u8]]) -> Vec<u8> {
  let mut payload = vec![98];
  payload.extend_from_slice(&channel.to_be_bytes());
  put_string(&mut payload, kind.as_bytes());
  payload.push(u8::from(want_reply));
  for field in fields {
    put_string(&mut payload, field);
  }
  payload
}

/// A message of `number` to `channel` with `tail` after it.
fn to_channel(number: u8, channel: u32, tail: &[u8]) -> Vec<u8> {
  let mut payload = vec![number];
  payload.extend_from_slice(&channel.to_be_bytes());
  payload.extend_from_slice(tail);
  payload
}

/// `SSH_MSG_CHANNEL_DATA` to `channel`.
fn data(channel: u32, bytes: &[u8]) -> Vec<u8> {
  let mut tail = Vec::new();
  put_string(&mut tail, bytes);
  to_channel(94, channel, &tail)
}

/// A reply naming the client's channel.
fn reply(number: u8) -> Vec<u8> {
  to_channel(number, CLIENT_CHANNEL, &[])
}

/// A session with a git fetch accepted: `GIT_PROTOCOL` passed and `git-upload-pack` asked for and accepted.
fn fetching(config: SessionConfig, window: u32, max_packet: u32) -> (Pair, u32) {
  let mut pair = authenticated(config);
  let (channel, _, _) = open_session(&mut pair, window, max_packet);
  pair
    .exchange(&channel_request(
      channel,
      "env",
      true,
      &[b"GIT_PROTOCOL", b"version=2"],
    ))
    .unwrap();
  pair
    .exchange(&channel_request(
      channel,
      "exec",
      true,
      &[b"git-upload-pack '/srv/repo.git'"],
    ))
    .unwrap();
  pair.server.answer_exec(true).unwrap();
  pair.pump().unwrap();
  (pair, channel)
}

/// A-633 (RFC 8308 §2.4, §3.1): do connect a client that lists `ext-info-c` and one that does not. Expect the first to
/// receive `SSH_MSG_EXT_INFO` naming `server-sig-algs` as the packet after the server's first NEWKEYS, listing every
/// signature algorithm the server takes, and the second none.
#[test]
fn ext_info_names_the_signature_algorithms_to_a_client_that_asks() {
  let mut asking = Pair::new(
    config(),
    ClientOptions::openssh_like("aes256-gcm@openssh.com", true),
  );
  asking.pump().unwrap();
  let mut expected = vec![7, 0, 0, 0, 1];
  put_string(&mut expected, b"server-sig-algs");
  put_string(&mut expected, b"ssh-ed25519,rsa-sha2-512,rsa-sha2-256");
  let mut options = ClientOptions::openssh_like("aes256-gcm@openssh.com", true);
  options.kex.retain(|name| name != "ext-info-c");
  let mut silent = Pair::new(config(), options);
  silent.pump().unwrap();
  let observed = (
    asking.client.received.first().cloned(),
    asking.client.received_epochs.first().copied(),
  );
  assert_eq!(
    (observed, silent.client.received.len()),
    ((Some(expected), Some(1)), 0)
  );
}

/// A-633 (RFC 4252 §5, §7): do authenticate a client the way OpenSSH does: the probe, a query, then the signature.
/// Expect a failure naming `publickey`, `SSH_MSG_USERAUTH_PK_OK`, then success, raised as the principal the store gave.
#[test]
fn a_client_authenticates_as_openssh_does() {
  let pair = authenticated(config());
  let numbers: Vec<u8> = pair
    .client
    .received
    .iter()
    .map(|payload| payload[0])
    .collect();
  assert_eq!(numbers, [7, 6, 51, 60, 52]);
  let principal = Authenticated {
    user: b"git".to_vec(),
    principal: 7,
  };
  assert_eq!(pair.events, [SessionEvent::Authenticated(principal)]);
}

/// A-633 (§4.17.2; RFC 4254 §5.1, §6.4, §6.5): do open a session, pass `GIT_PROTOCOL` and ask for `git-upload-pack`.
/// Expect the server's window and packet offered, the variable taken, and the command raised with it, unrun until the
/// caller accepts, then answered `SSH_MSG_CHANNEL_SUCCESS`.
#[test]
fn a_git_fetch_is_raised_with_its_protocol_and_accepted() {
  let mut pair = authenticated(config());
  let (channel, window, max_packet) = open_session(&mut pair, WINDOW, 1 << 15);
  assert_eq!(
    (window, max_packet),
    (WINDOW, max_data(MAX_PACKET_EVERY_PEER_ACCEPTS))
  );
  pair
    .exchange(&channel_request(
      channel,
      "env",
      true,
      &[b"GIT_PROTOCOL", b"version=2"],
    ))
    .unwrap();
  pair
    .exchange(&channel_request(
      channel,
      "exec",
      true,
      &[b"git-upload-pack '/srv/repo.git'"],
    ))
    .unwrap();
  let exec = SessionEvent::Exec {
    command: Command::GitUploadPack {
      path: b"/srv/repo.git".to_vec(),
    },
    git_protocol: Some(b"version=2".to_vec()),
  };
  assert_eq!(
    (pair.events.last().cloned(), pair.last()),
    (Some(exec), reply(SUCCESS))
  );
  pair.server.answer_exec(true).unwrap();
  pair.pump().unwrap();
  assert_eq!(pair.last(), reply(SUCCESS));
}

/// A-633 (RFC 4254 §5.2, §6.10, §5.3): do pass data each way in an accepted fetch, then finish with status 0 and close
/// from the client. Expect the client's data raised, the server's data received, then `exit-status`, EOF and close in
/// that order, and the channel closed both ways.
#[test]
fn a_fetch_ends_with_exit_status_eof_and_close_in_order() {
  let (mut pair, channel) = fetching(config(), WINDOW, 1 << 15);
  pair
    .exchange(&data(channel, b"0014command=ls-refs\n0000"))
    .unwrap();
  assert_eq!(
    pair.events.last(),
    Some(&SessionEvent::Data(b"0014command=ls-refs\n0000".to_vec()))
  );
  pair.server.consumed(24).unwrap();
  assert_eq!(pair.server.send_data(b"0000"), Ok(4));
  pair.server.finish(0).unwrap();
  pair.pump().unwrap();
  let tail = &pair.client.received[pair.client.received.len() - 4..];
  let mut status = to_channel(98, CLIENT_CHANNEL, &[]);
  put_string(&mut status, b"exit-status");
  status.extend_from_slice(&[0, 0, 0, 0, 0]);
  let mut sent = to_channel(94, CLIENT_CHANNEL, &[]);
  put_string(&mut sent, b"0000");
  assert_eq!(tail, [sent, status, reply(96), reply(97)]);
  pair.exchange(&to_channel(97, channel, &[])).unwrap();
  assert_eq!(pair.events.last(), Some(&SessionEvent::ChannelClosed));
}

/// A-633 (§4.17.3, §4.17.4): do ask for Mercurial's and Subversion's commands. Expect each raised as its command.
#[test]
fn mercurial_and_subversion_commands_reach_the_caller() {
  let cases: [(&[u8], Command); 2] = [
    (
      b"hg -R 'team repo' serve --stdio",
      Command::Mercurial {
        repository: b"team repo".to_vec(),
        hidden: false,
      },
    ),
    (b"svnserve -t", Command::Subversion),
  ];
  for (line, command) in cases {
    let mut pair = authenticated(config());
    let (channel, _, _) = open_session(&mut pair, WINDOW, 1 << 15);
    pair
      .exchange(&channel_request(channel, "exec", true, &[line]))
      .unwrap();
    assert_eq!(
      pair.events.last(),
      Some(&SessionEvent::Exec {
        command,
        git_protocol: None
      })
    );
  }
}

/// A-633 (§4.17.2, "There is no shell, no subsystem and no forwarding"; RFC 4254 §5.4): do ask an open session for a
/// pty, a shell, sftp, another variable, an unserved command and OpenSSH's keepalive, each wanting a reply, and for a
/// signal wanting none. Expect `SSH_MSG_CHANNEL_FAILURE` for each that wants a reply, nothing for the signal, and no
/// event.
#[test]
fn channel_requests_without_a_server_are_refused() {
  let mut pair = authenticated(config());
  let (channel, _, _) = open_session(&mut pair, WINDOW, 1 << 15);
  let refused: [(&str, Vec<&[u8]>); 6] = [
    ("pty-req", vec![&b"xterm"[..]]),
    ("shell", Vec::new()),
    ("subsystem", vec![&b"sftp"[..]]),
    ("env", vec![&b"LANG"[..], &b"C"[..]]),
    ("exec", vec![&b"rm -rf /"[..]]),
    ("keepalive@openssh.com", Vec::new()),
  ];
  let received = pair.client.received.len();
  for (kind, fields) in &refused {
    pair
      .client
      .send(&channel_request(channel, kind, true, fields));
  }
  pair
    .client
    .send(&channel_request(channel, "signal", false, &[b"TERM"]));
  pair.pump().unwrap();
  let answers = pair.client.received[received..].to_vec();
  assert_eq!((answers, pair.events.len()), (vec![reply(FAILURE); 6], 1));
}

/// A-633 (RFC 4254 §4, §5.1): do send a global request wanting a reply and one wanting none, and open forwarding, an
/// agent channel, and a second session. Expect `SSH_MSG_REQUEST_FAILURE` for the first global request alone, then
/// open failures: administratively prohibited (1) for RFC 4254's own type, unknown type (3) for OpenSSH's, and resource
/// shortage (4) for the second session.
#[test]
fn globals_forwarding_and_a_second_session_are_refused() {
  let mut pair = authenticated(config());
  open_session(&mut pair, WINDOW, 1 << 15);
  let received = pair.client.received.len();
  for (name, want_reply) in [
    ("tcpip-forward", true),
    ("no-more-sessions@openssh.com", false),
  ] {
    let mut global = vec![80];
    put_string(&mut global, name.as_bytes());
    global.push(u8::from(want_reply));
    pair.client.send(&global);
  }
  for kind in ["direct-tcpip", "auth-agent@openssh.com", "session"] {
    pair.client.send(&open(kind, WINDOW, 1 << 15));
  }
  pair.pump().unwrap();
  let reasons: Vec<Option<u8>> = pair.client.received[received + 1..]
    .iter()
    .map(|payload| (payload[0] == OPEN_FAILURE).then(|| payload[8]))
    .collect();
  assert_eq!(
    (pair.client.received[received].clone(), reasons),
    (vec![82], vec![Some(1), Some(3), Some(4)])
  );
}

/// A-633 (RFC 4254 §6.5, "Only one of these requests can succeed per channel"): do ask for a second command and pass a
/// variable after the first was accepted. Expect both refused.
#[test]
fn a_command_is_taken_once() {
  let (mut pair, channel) = fetching(config(), WINDOW, 1 << 15);
  pair
    .exchange(&channel_request(
      channel,
      "exec",
      true,
      &[b"git-receive-pack '/srv/repo.git'"],
    ))
    .unwrap();
  let second = pair.last();
  pair
    .exchange(&channel_request(
      channel,
      "env",
      true,
      &[b"GIT_PROTOCOL", b"version=1"],
    ))
    .unwrap();
  assert_eq!((second, pair.last()), (reply(FAILURE), reply(FAILURE)));
}

/// A-633 (RFC 4254 §5.2): do send a window's worth of data under a window of three packets, consuming less than a
/// packet, then a packet more. Expect no window given back until a whole packet was consumed, then exactly what was.
#[test]
fn the_window_comes_back_a_packet_at_a_time() {
  let max_packet = max_data(MAX_PACKET_EVERY_PEER_ACCEPTS);
  let mut configured = config();
  configured.channel_window = max_packet * 3;
  let (mut pair, channel) = fetching(configured, WINDOW, 1 << 15);
  let chunk = vec![b'x'; max_packet as usize];
  pair.exchange(&data(channel, &chunk)).unwrap();
  let before = pair.client.received.len();
  pair.server.consumed(max_packet as usize - 1).unwrap();
  pair.pump().unwrap();
  let early = pair.client.received.len() - before;
  pair.server.consumed(1).unwrap();
  pair.pump().unwrap();
  let mut adjust = to_channel(93, CLIENT_CHANNEL, &[]);
  adjust.extend_from_slice(&max_packet.to_be_bytes());
  assert_eq!((early, pair.last()), (0, adjust));
}

/// A-633 hostile (RFC 4254 §5.2): do send past the window under a window of 100 bytes, and send more than one packet's
/// worth in a message. Expect each refused by its kind with disconnect reason 2.
#[test]
fn data_past_the_window_or_the_packet_is_refused() {
  let mut small = config();
  small.channel_window = 100;
  let (mut pair, channel) = fetching(small, WINDOW, 1 << 15);
  let past_window = pair.exchange(&data(channel, &[b'x'; 101]));
  let (mut pair_two, channel_two) = fetching(config(), WINDOW, 1 << 15);
  let oversized = vec![b'x'; max_data(MAX_PACKET_EVERY_PEER_ACCEPTS) as usize + 1];
  let past_packet = pair_two.exchange(&data(channel_two, &oversized));
  assert_eq!(
    (past_window, past_packet),
    (
      Err(SessionRefusal::Channel(
        crate::channel::ChannelRefusal::WindowExceeded
      )),
      Err(SessionRefusal::Channel(
        crate::channel::ChannelRefusal::PacketTooLarge
      ))
    )
  );
  pair.to_client();
  assert_eq!(&pair.last()[..5], &[1, 0, 0, 0, 2]);
}

/// A-633 (RFC 4254 §5.2, "The maximum amount of data allowed is determined by the maximum packet size for the channel,
/// and the current window size, whichever is smaller"): do send 250 bytes to a client whose window is 100 and packet 40,
/// then let it open 60 more. Expect 100 taken in packets of 40, 40 and 20, then nothing until the window opens, then
/// `Writable` and 60 more.
#[test]
fn sends_stay_within_the_clients_window_and_packet() {
  let (mut pair, channel) = fetching(config(), 100, 40);
  let before = pair.client.received.len();
  let first = pair.server.send_data(&[b'y'; 250]);
  let shut = pair.server.send_data(&[b'y'; 150]);
  pair.pump().unwrap();
  let lengths: Vec<usize> = pair.client.received[before..]
    .iter()
    .map(|payload| payload.len() - 9)
    .collect();
  pair
    .exchange(&to_channel(93, channel, &60u32.to_be_bytes()))
    .unwrap();
  let writable = pair.events.last() == Some(&SessionEvent::Writable);
  let reopened = pair.server.send_data(&[b'y'; 150]);
  assert_eq!(
    (first, shut, lengths, writable, reopened),
    (Ok(100), Ok(0), vec![40, 40, 20], true, Ok(60))
  );
}

/// A-633 hostile (RFC 4252 §5.1; RFC 4253 §11.4): do send a channel open before authenticating, a success message only
/// the server sends, and, once authenticated, a number no layer has. Expect the first two refused as unexpected and
/// the last answered `SSH_MSG_UNIMPLEMENTED` with its sequence number.
#[test]
fn messages_out_of_their_layer_are_refused_or_unimplemented() {
  let mut early = serving(
    config(),
    ClientOptions::openssh_like("chacha20-poly1305@openssh.com", true),
  );
  let before_auth = early.exchange(&open("session", WINDOW, 1 << 15));
  let mut forged = serving(
    config(),
    ClientOptions::openssh_like("chacha20-poly1305@openssh.com", true),
  );
  let server_only = forged.exchange(&[52]);
  let mut pair = authenticated(config());
  let sequence = pair.client.send_sequence();
  pair.exchange(&[192, 0, 0, 0, 0]).unwrap();
  let mut unimplemented = vec![3];
  unimplemented.extend_from_slice(&sequence.to_be_bytes());
  let refusals = (before_auth, server_only);
  let expected = (
    Err(SessionRefusal::Unexpected { number: 90 }),
    Err(SessionRefusal::Unexpected { number: 52 }),
  );
  assert_eq!((refusals, pair.last()), (expected, unimplemented));
}

/// A-633 hostile (RFC 4252 §4): do query two keys nobody holds under a failure limit of one. Expect the second refused,
/// with disconnect reason 14, no more authentication methods.
#[test]
fn too_many_failures_end_with_no_more_methods() {
  let mut limited = config();
  limited.failure_limit = 1;
  let mut pair = serving(
    limited,
    ClientOptions::openssh_like("chacha20-poly1305@openssh.com", true),
  );
  let stranger = crate::userauth_tests::ed_key(
    "c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7",
  );
  pair
    .client
    .send(&query(b"git", "ssh-ed25519", &stranger.blob));
  let outcome = pair.exchange(&query(b"git", "ssh-ed25519", &stranger.blob));
  pair.to_client();
  let reason = pair.last()[1..5].to_vec();
  assert_eq!(
    (outcome, reason),
    (
      Err(SessionRefusal::Auth(AuthRefusal::TooManyFailures {
        failures: 2
      })),
      vec![0, 0, 0, 14]
    )
  );
}

/// A-633 (RFC 4253 §7.1, "it MUST NOT send any messages other than" the exchange's): do start the server's key exchange,
/// then, before the client sees it, have the client ask for a reply and the server send data. Expect both held until
/// the exchange ends, received under the new keys, and `Writable` raised once sending may go on.
#[test]
fn replies_and_data_wait_out_the_servers_own_rekey() {
  let mut configured = config();
  configured.transport.key_time_ns = 1_000;
  let (mut pair, channel) = fetching(configured, WINDOW, 1 << 15);
  pair.server.poll_timers(1_000).unwrap();
  let kexinit = pair.server.pending_output().to_vec();
  pair.client.send(&channel_request(
    channel,
    "keepalive@openssh.com",
    true,
    &[],
  ));
  let request = pair.client.take_output();
  pair.to_server(&request).unwrap();
  let queued = pair.server.send_data(b"after the rekey");
  pair.server.consume_output(kexinit.len()).unwrap();
  pair.client.feed(&kexinit);
  pair.pump().unwrap();
  let epochs = &pair.client.received_epochs[pair.client.received_epochs.len() - 2..];
  let writable = pair.events.contains(&SessionEvent::Writable);
  assert_eq!(
    (
      queued,
      epochs.to_vec(),
      writable,
      pair.client.server_newkeys
    ),
    (Ok(15), vec![2, 2], true, 2)
  );
}

/// A-633: do misuse the session: answer a command never asked, give back more than was given, send before a command
/// runs. Expect each refused by its kind, the connection going on.
#[test]
fn misuse_is_refused_and_ends_nothing() {
  let mut pair = authenticated(config());
  open_session(&mut pair, WINDOW, 1 << 15);
  let misuses = (
    pair.server.answer_exec(true),
    pair.server.consumed(1),
    pair.server.send_data(b"x"),
  );
  let expected = (
    Err(SessionUse::Channel(crate::channel::ChannelUse::NoCommand)),
    Err(SessionUse::Channel(
      crate::channel::ChannelUse::Overconsumed,
    )),
    Err(SessionUse::Channel(crate::channel::ChannelUse::NotRunning)),
  );
  assert_eq!((misuses, pair.server.is_closed()), (expected, false));
}

/// A-633 (RFC 4253 §11.1): do disconnect from the client. Expect it raised with its reason and description.
#[test]
fn a_client_disconnect_is_raised() {
  let mut pair = authenticated(config());
  let mut disconnect = vec![1, 0, 0, 0, 11];
  put_string(&mut disconnect, b"done");
  put_string(&mut disconnect, b"");
  pair.exchange(&disconnect).unwrap();
  let expected = SessionEvent::Disconnected {
    reason: 11,
    description: "done".to_string(),
  };
  assert_eq!(
    (pair.events.last().cloned(), pair.server.is_closed()),
    (Some(expected), true)
  );
}

/// A-633: do read the description of a refused channel open. Expect the reason's text, in English, after the reason.
#[test]
fn an_open_failure_says_why() {
  let mut pair = authenticated(config());
  pair.exchange(&open("x11", WINDOW, 1 << 15)).unwrap();
  let failure = pair.last();
  let mut at = 9;
  assert_eq!(
    (failure[0], read_string(&failure, &mut at)),
    (OPEN_FAILURE, b"no forwarding".to_vec())
  );
}
