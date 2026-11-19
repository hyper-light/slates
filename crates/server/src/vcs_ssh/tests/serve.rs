use std::io::{Read, Write as _};
use std::net::TcpStream as ClientStream;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair};
use slates_db::catalog::Principal;
use slates_rt::control::Control;
use slates_rt::runtime::{LocalRuntime, RuntimeConfig};
use slates_rt::tcp::{Ipv4Addr, SocketAddrV4, TcpListener};
use slates_ssh::command::Command as Asked;
use slates_ssh::keylife::RECOMMENDED_KEY_TIME_NS;
use slates_ssh::userauth::{Holding, KeyHolders, UserKey};

use super::site::{Command, Dispatched, Next, Refused, SshSite, Stepped};
use super::ssh_client::{Client, ClientOptions, put_string, read_string};
use super::waits::PHASES;
use super::{Bounds, Counts, EdgeState, serve};
use crate::daemon::LIVENESS_BUDGET_NS;

/// Shape: one seat: the edge of a shard that seats one client.
const ONE_SEAT: usize = 1;
/// Shape: two seats: a connection, and the one that needs the other's seat.
const TWO_SEATS: usize = 2;
/// Shape: the bytes one client region holds in these tests: room for everything they send.
const ROOM_BYTES: usize = 1 << 20;
/// Shape: the tests' runtime's timer tick, 100 µs: the least clock granularity their waits are floored by.
const TICK_NS: u64 = 100_000;
/// Shape: how long a client waits for an answer before it gives up: four liveness windows, past every bound these tests
/// reach.
const PATIENCE_NS: u64 = 4 * LIVENESS_BUDGET_NS;
/// Shape: a client's pause past every first value these tests reach: two liveness windows (a first value is a window
/// and about three handshakes).
const PAUSE_NS: u64 = 2 * LIVENESS_BUDGET_NS;
/// Shape: the host key's seed, RFC 8032 §7.1 test 1's.
const HOST_SEED: [u8; 32] = [
  0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c, 0xc4,
  0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae, 0x7f, 0x60,
];
/// Shape: the held user key's seed, RFC 8032 §7.1 test 2's.
const HELD_SEED: [u8; 32] = [
  0x4c, 0xcd, 0x08, 0x9b, 0x28, 0xff, 0x96, 0xda, 0x9d, 0xb6, 0xc3, 0x46, 0xec, 0x11, 0x4e, 0x0f,
  0x5b, 0x8a, 0x31, 0x9f, 0x35, 0xab, 0xa6, 0x24, 0xda, 0x8c, 0xf6, 0xed, 0x4f, 0xb8, 0xa6, 0xfb,
];
/// Shape: a key no principal holds, RFC 8032 §7.1 test 3's.
const STRANGER_SEED: [u8; 32] = [
  0xc5, 0xaa, 0x8d, 0xf4, 0x3f, 0x9f, 0x83, 0x7b, 0xed, 0xb7, 0x44, 0x2f, 0x31, 0xdc, 0xb7, 0xb1,
  0x66, 0xd3, 0x85, 0x35, 0x07, 0x6f, 0x09, 0x4b, 0x85, 0xce, 0x3a, 0x2e, 0x0b, 0x44, 0x58, 0xf7,
];
/// Shape: the principal the held key proves.
const HOLDER: Principal = Principal::Certificate { hash: [7; 32] };
/// Shape: the user name the client gives, as git's does.
const USER: &[u8] = b"git";
/// Shape: the client's channel number.
const CLIENT_CHANNEL: u32 = 7;
/// Shape: the client's window: room for everything these tests' commands write.
const CLIENT_WINDOW: u32 = 1 << 20;
/// Shape: the client's packet, OpenSSH's session default (32 KiB).
const CLIENT_PACKET: u32 = 1 << 15;
/// Shape: what the tests' command writes first, before it waits for its client.
const READY: &[u8] = b"ready\n";
/// Shape: what the tests' client sends the command, as a push sends its pack.
const PACK: &[u8] = b"pack";

/// The blob of the Ed25519 key of `seed` (RFC 8709 §4).
fn blob(seed: &[u8; 32]) -> Vec<u8> {
  let key = Ed25519KeyPair::from_seed_unchecked(seed).expect("an Ed25519 key");
  let mut blob = Vec::new();
  put_string(&mut blob, b"ssh-ed25519");
  put_string(&mut blob, key.public_key().as_ref());
  blob
}

/// The held key proves [`HOLDER`]; no other key proves anyone.
#[derive(Clone)]
struct Keys;

impl KeyHolders for Keys {
  type Principal = Principal;

  fn holding(&mut self, _user: &[u8], _key: &UserKey, key_blob: &[u8]) -> Holding<Principal> {
    if key_blob == blob(&HELD_SEED) {
      Holding::Holds {
        principal: HOLDER,
        keys_required: 1,
      }
    } else {
      Holding::Nobody
    }
  }
}

/// A site that runs git's two services as [`Scripted`] commands and refuses everything else as git refuses.
struct Site;

impl SshSite for Site {
  type Keys = Keys;
  type Command = Scripted;

  fn keys(&mut self) -> Keys {
    Keys
  }

  async fn lookup(&mut self, user: &[u8], key: &UserKey, key_blob: &[u8]) -> Holding<Principal> {
    Keys.holding(user, key, key_blob)
  }

  fn dispatch(
    &mut self,
    _principal: &Principal,
    command: &Asked,
    _git_protocol: Option<&[u8]>,
  ) -> Dispatched<Scripted> {
    match command {
      Asked::GitUploadPack { .. } | Asked::GitReceivePack { .. } => {
        Dispatched::Run(Scripted::default())
      }
      _ => Dispatched::Refuse(Refused {
        stderr: b"fatal: refused\n".to_vec(),
        exit_status: super::site::GIT_DIE_STATUS,
      }),
    }
  }
}

/// A connection lookup that cannot say of any key: every key is looked up where its record lives.
#[derive(Clone)]
struct Asking;

impl KeyHolders for Asking {
  type Principal = Principal;

  fn holding(&mut self, _user: &[u8], _key: &UserKey, _key_blob: &[u8]) -> Holding<Principal> {
    Holding::Unknown
  }
}

/// [`Site`], its keys looked up away from the session (A-651 part 3): the held key proves [`HOLDER`] once the lookup
/// answers.
struct LooksUp;

impl SshSite for LooksUp {
  type Keys = Asking;
  type Command = Scripted;

  fn keys(&mut self) -> Asking {
    Asking
  }

  async fn lookup(&mut self, user: &[u8], key: &UserKey, key_blob: &[u8]) -> Holding<Principal> {
    Keys.holding(user, key, key_blob)
  }

  fn dispatch(
    &mut self,
    principal: &Principal,
    command: &Asked,
    git_protocol: Option<&[u8]>,
  ) -> Dispatched<Scripted> {
    Site.dispatch(principal, command, git_protocol)
  }
}

/// A command shaped as a push: it writes [`READY`], waits for its client's input, consumes it, answers `got:` and the
/// input, and exits with status 0.
#[derive(Default)]
struct Scripted {
  /// Whether it wrote [`READY`].
  greeted: bool,
  /// The input held, not yet consumed.
  input: Vec<u8>,
  /// What it wrote and the channel has not taken.
  out: Vec<u8>,
}

impl Command for Scripted {
  fn input(&mut self, run: &[u8]) -> Result<(), Refused> {
    self.input.extend_from_slice(run);
    Ok(())
  }

  fn input_end(&mut self) {}

  async fn step(&mut self) -> Result<Stepped, Refused> {
    if !self.greeted {
      self.greeted = true;
      self.out.extend_from_slice(READY);
      return Ok(Stepped {
        consumed: 0,
        next: Next::Input,
      });
    }
    if self.input.is_empty() {
      return Ok(Stepped {
        consumed: 0,
        next: Next::Input,
      });
    }
    let consumed = self.input.len();
    self.out.extend_from_slice(b"got:");
    self.out.append(&mut self.input);
    Ok(Stepped {
      consumed,
      next: Next::Exited(0),
    })
  }

  fn output(&self) -> (&[u8], &[u8]) {
    (&[], &self.out)
  }

  fn sent(&mut self, _stderr: usize, stdout: usize) {
    self.out.drain(..stdout.min(self.out.len()));
  }
}

/// What a test reads of an edge once its clients have finished.
#[derive(Debug)]
struct Seen {
  /// The counts.
  counts: Counts,
  /// The seats still taken.
  serving: usize,
  /// Each phase's completed waits and reaped connections, in [`PHASES`]' order.
  learned: [(u64, u64); 5],
  /// Each phase's back-offs, in [`PHASES`]' order.
  backoffs: [u64; 5],
  /// The key time a new session would take.
  key_time_ns: u64,
}

/// What `state` holds now.
fn seen(state: &EdgeState) -> Seen {
  Seen {
    counts: state.counts(),
    serving: state.serving(),
    learned: PHASES.map(|phase| state.learned(phase)),
    backoffs: PHASES.map(|phase| state.backoffs(phase)),
    key_time_ns: state.key_time_ns(),
  }
}

/// Shape: a runtime for one edge: one shard, room for its listener, its connections and their timers.
fn runtime() -> LocalRuntime {
  LocalRuntime::new(&RuntimeConfig {
    shards: 1,
    tasks_per_shard: 32,
    timers_per_shard: 32,
    ring_entries: 8,
    step_budget_ns: 1_000_000,
    timer_tick_ns: TICK_NS,
    batch: 8,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
    wake_tracking: None,
  })
  .expect("a local runtime")
}

/// Stops shard `0` when dropped, however its client's thread ends, so a test never hangs on a shard nobody stops.
struct StopsShard(u16);

impl Drop for StopsShard {
  fn drop(&mut self) {
    let _ = slates_rt::registry::send_control(self.0, Control::Shutdown);
  }
}

/// Serves an edge of `seats` with [`Site`], runs `client` against it on a thread of its own, and stops the shard once the
/// client returns: what the client returned, and what the edge then held.
fn served<C, R>(seats: usize, client: C) -> (R, Seen)
where
  C: FnOnce(SocketAddrV4) -> R + Send + 'static,
  R: Send + 'static,
{
  served_by(seats, || Site, client)
}

/// As [`served`], with sites `make` makes.
fn served_by<S, M, C, R>(seats: usize, make: M, client: C) -> (R, Seen)
where
  S: SshSite + 'static,
  M: FnMut() -> S + 'static,
  C: FnOnce(SocketAddrV4) -> R + Send + 'static,
  R: Send + 'static,
{
  let runtime = runtime();
  let bounds = Bounds::of_shard(seats, ROOM_BYTES, TICK_NS);
  let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), bounds.backlog())
    .expect("a loopback listener");
  let address = listener.local_addr().expect("the listener's address");
  let state = runtime
    .context()
    .keep(EdgeState::new(bounds, HOST_SEED))
    .expect("the edge's state kept on its shard");
  runtime
    .spawn(async move {
      let _ = serve(listener, state, make).await;
    })
    .expect("the listener's task");
  let shard = runtime.shard_id().0;
  let client = std::thread::spawn(move || {
    let _stops = StopsShard(shard);
    client(address)
  });
  runtime.context().run();
  let result = client.join().expect("the client's thread");
  let seen = state
    .with_in(runtime.context(), seen)
    .expect("the edge's state");
  (result, seen)
}

/// One SSH client over a loopback socket: slates-ssh's client written from the RFCs, its messages read in order.
struct Peer {
  /// The socket.
  stream: ClientStream,
  /// The client.
  client: Client,
  /// The client's messages already read.
  seen: usize,
  /// The server's channel number, once a session is open.
  channel: u32,
}

impl Peer {
  /// A client connected to `address` that has finished its first key exchange; none if it could not.
  fn connect(address: SocketAddrV4) -> Option<Peer> {
    let stream = ClientStream::connect(address).ok()?;
    stream
      .set_read_timeout(Some(Duration::from_nanos(PATIENCE_NS)))
      .ok()?;
    let mut peer = Peer {
      stream,
      client: Client::new(ClientOptions::openssh_like(
        "chacha20-poly1305@openssh.com",
        true,
      )),
      seen: 0,
      channel: 0,
    };
    while peer.client.session_id.is_none() || peer.client.server_newkeys == 0 {
      if !peer.pump() {
        return None;
      }
    }
    Some(peer)
  }

  /// Sends what the client has to send, then reads once and feeds it: whether the connection is still open.
  fn pump(&mut self) -> bool {
    let out = self.client.take_output();
    if !out.is_empty() && self.stream.write_all(&out).is_err() {
      return false;
    }
    let mut chunk = [0u8; 4096];
    match self.stream.read(&mut chunk) {
      Ok(0) | Err(_) => false,
      Ok(count) => {
        self.client.feed(&chunk[..count]);
        true
      }
    }
  }

  /// Sends `payload` as the client's next message.
  fn send(&mut self, payload: &[u8]) {
    self.client.send(payload);
    let out = self.client.take_output();
    let _ = self.stream.write_all(&out);
  }

  /// The next message the server sent whose number is one of `numbers`, the others passed over; none once the
  /// connection ends.
  fn next(&mut self, numbers: &[u8]) -> Option<Vec<u8>> {
    loop {
      while let Some(payload) = self.client.received.get(self.seen) {
        self.seen += 1;
        if payload
          .first()
          .is_some_and(|number| numbers.contains(number))
        {
          return Some(payload.clone());
        }
      }
      if !self.pump() {
        return None;
      }
    }
  }

  /// Waits until the connection ends: whether it ended before the client's patience ran out.
  fn ended(&mut self) -> bool {
    let started = Instant::now();
    while self.pump() {}
    started.elapsed() < Duration::from_nanos(PATIENCE_NS)
  }
}

/// RFC 4252 §7's signed data for the key `key_blob` of `user`, under `session_id`.
fn signed_data(session_id: &[u8], user: &[u8], key_blob: &[u8]) -> Vec<u8> {
  let mut data = Vec::new();
  put_string(&mut data, session_id);
  data.push(50);
  put_string(&mut data, user);
  put_string(&mut data, b"ssh-connection");
  put_string(&mut data, b"publickey");
  data.push(1);
  put_string(&mut data, b"ssh-ed25519");
  put_string(&mut data, key_blob);
  data
}

/// A publickey request for the key of `seed`: a query, or signed under `session_id`.
fn userauth(seed: &[u8; 32], session_id: Option<&[u8]>) -> Vec<u8> {
  let key_blob = blob(seed);
  let mut request = vec![50];
  put_string(&mut request, USER);
  put_string(&mut request, b"ssh-connection");
  put_string(&mut request, b"publickey");
  request.push(u8::from(session_id.is_some()));
  put_string(&mut request, b"ssh-ed25519");
  put_string(&mut request, &key_blob);
  if let Some(session_id) = session_id {
    let key = Ed25519KeyPair::from_seed_unchecked(seed).expect("an Ed25519 key");
    let signature = key.sign(&signed_data(session_id, USER, &key_blob));
    let mut signature_blob = Vec::new();
    put_string(&mut signature_blob, b"ssh-ed25519");
    put_string(&mut signature_blob, signature.as_ref());
    put_string(&mut request, &signature_blob);
  }
  request
}

/// Asks for `ssh-userauth` and queries the key of `seed`: the server's answer's number (60, `PK_OK`, for a held key).
fn query(peer: &mut Peer, seed: &[u8; 32]) -> Option<u8> {
  let mut service = vec![5];
  put_string(&mut service, b"ssh-userauth");
  peer.send(&service);
  peer.next(&[6])?;
  peer.send(&userauth(seed, None));
  peer.next(&[51, 60])?.first().copied()
}

/// Signs with the key of `seed`: the server's answer's number (52, success).
fn sign(peer: &mut Peer, seed: &[u8; 32]) -> Option<u8> {
  let session_id = peer.client.session_id?;
  peer.send(&userauth(seed, Some(&session_id)));
  peer.next(&[51, 52])?.first().copied()
}

/// Opens a session and asks it to run `command`: whether the server accepted.
fn exec(peer: &mut Peer, command: &[u8]) -> bool {
  let mut open = vec![90];
  put_string(&mut open, b"session");
  for word in [CLIENT_CHANNEL, CLIENT_WINDOW, CLIENT_PACKET] {
    open.extend_from_slice(&word.to_be_bytes());
  }
  peer.send(&open);
  let Some(confirmation) = peer.next(&[91, 92]) else {
    return false;
  };
  if confirmation.first() != Some(&91) {
    return false;
  }
  peer.channel = u32::from_be_bytes(confirmation[5..9].try_into().expect("a channel number"));
  let mut request = vec![98];
  request.extend_from_slice(&peer.channel.to_be_bytes());
  put_string(&mut request, b"exec");
  request.push(1);
  put_string(&mut request, command);
  peer.send(&request);
  peer
    .next(&[99, 100])
    .and_then(|reply| reply.first().copied())
    == Some(99)
}

/// Sends `data` on the session.
fn send_data(peer: &mut Peer, data: &[u8]) {
  let mut message = vec![94];
  message.extend_from_slice(&peer.channel.to_be_bytes());
  put_string(&mut message, data);
  peer.send(&message);
}

/// What a session gave its client.
#[derive(Debug, Default, PartialEq, Eq)]
struct Transcript {
  /// Standard output.
  out: Vec<u8>,
  /// Standard error.
  err: Vec<u8>,
  /// The exit status.
  status: Option<u32>,
  /// The channel messages' numbers, in order (each data message once in a row).
  order: Vec<u8>,
}

/// Reads the session until the server closes it, then closes it too: everything it gave.
fn transcript(peer: &mut Peer) -> Transcript {
  let mut transcript = Transcript::default();
  while let Some(message) = peer.next(&[94, 95, 96, 97, 98]) {
    let number = message[0];
    if transcript.order.last() != Some(&number) {
      transcript.order.push(number);
    }
    let mut at = 5;
    match number {
      94 => transcript.out.extend(read_string(&message, &mut at)),
      95 => {
        at += 4;
        transcript.err.extend(read_string(&message, &mut at));
      }
      98 if read_string(&message, &mut at) == b"exit-status" => {
        let status = message.get(at + 1..at + 5).expect("the status");
        transcript.status = Some(u32::from_be_bytes(status.try_into().expect("four bytes")));
      }
      97 => {
        let mut close = vec![97];
        close.extend_from_slice(&peer.channel.to_be_bytes());
        peer.send(&close);
        break;
      }
      _ => {}
    }
  }
  transcript
}

/// The transcript of a command that took [`PACK`] and exited 0: [`READY`], `got:` and the pack, then exit-status, EOF and
/// close, in that order.
fn pushed() -> Transcript {
  Transcript {
    out: [READY, b"got:", PACK].concat(),
    err: Vec::new(),
    status: Some(0),
    order: vec![94, 98, 96, 97],
  }
}

/// A whole session with the held key: authenticate (pausing `auth_pause_ns` between the server's `PK_OK` and the
/// signature), run `git-receive-pack`, read [`READY`], pause `push_pause_ns`, send [`PACK`], and read to the end.
fn session(address: SocketAddrV4, auth_pause_ns: u64, push_pause_ns: u64) -> Option<Transcript> {
  let mut peer = Peer::connect(address)?;
  (query(&mut peer, &HELD_SEED)? == 60).then_some(())?;
  std::thread::sleep(Duration::from_nanos(auth_pause_ns));
  (sign(&mut peer, &HELD_SEED)? == 52).then_some(())?;
  exec(&mut peer, b"git-receive-pack '/r.git'").then_some(())?;
  let ready = peer.next(&[94])?;
  let mut at = 5;
  (read_string(&ready, &mut at) == READY).then_some(())?;
  std::thread::sleep(Duration::from_nanos(push_pause_ns));
  send_data(&mut peer, PACK);
  let mut transcript = transcript(&mut peer);
  transcript.out.splice(0..0, READY.iter().copied());
  Some(transcript)
}

/// A-651 (§4.17.2 "Transports" item 2; RFC 4252, RFC 4254). Do: with one seat, connect, authenticate with a held key, run
/// a command that writes, waits for the client's pack and answers it, and read to the end. Expect: the command's output,
/// exit-status, EOF and close in order; one command run and nothing reaped, refused or failed; every phase taught by the
/// proven connection (one sample each but the command's, whose silences are several); and the edge's key time now the
/// lifetime's tail, below the hour.
#[test]
fn a_held_key_runs_its_command_and_its_connection_teaches_the_edge() {
  let (transcript, seen) = served(ONE_SEAT, |address| session(address, 0, 0));
  assert_eq!(transcript, Some(pushed()));
  assert_eq!(
    (
      seen.counts.accepted,
      seen.counts.commands_run,
      seen.counts.reaped,
      seen.counts.refused,
      seen.counts.failed,
      seen.serving
    ),
    (1, 1, 0, 0, 0, 0)
  );
  let samples = seen.learned.map(|(samples, _)| samples);
  assert_eq!(
    (
      samples[0],
      samples[1],
      samples[2],
      samples[4],
      seen.counts.reaped
    ),
    (1, 1, 1, 1, 0)
  );
  assert!(samples[3] >= 1, "{samples:?}");
  assert!(
    seen.key_time_ns < RECOMMENDED_KEY_TIME_NS,
    "{}",
    seen.key_time_ns
  );
}

/// A-651 (a refused `exec`, as the tool's program refuses). Do: authenticate and ask the site for a command it refuses.
/// Expect: the refusal on standard error, then its exit status, EOF and close, in order; counted as refused, not run.
#[test]
fn a_refused_exec_is_answered_on_standard_error_with_its_exit_status() {
  let (transcript, seen) = served(ONE_SEAT, |address| {
    let mut peer = Peer::connect(address)?;
    (query(&mut peer, &HELD_SEED)? == 60).then_some(())?;
    (sign(&mut peer, &HELD_SEED)? == 52).then_some(())?;
    exec(&mut peer, b"svnserve -t").then_some(())?;
    Some(transcript(&mut peer))
  });
  assert_eq!(
    transcript,
    Some(Transcript {
      out: Vec::new(),
      err: b"fatal: refused\n".to_vec(),
      status: Some(super::site::GIT_DIE_STATUS),
      order: vec![95, 98, 96, 97],
    })
  );
  assert_eq!(
    (seen.counts.commands_refused, seen.counts.commands_run),
    (1, 0)
  );
}

/// A-651 (§4.13: a key proves a principal only through the site's lookup). Do: query a key no principal holds, then
/// leave. Expect: `USERAUTH_FAILURE`, not `PK_OK`; and nothing taught, since no key proved anyone.
#[test]
fn a_key_no_principal_holds_is_refused_and_teaches_nothing() {
  let (answer, seen) = served(ONE_SEAT, |address| {
    let mut peer = Peer::connect(address)?;
    query(&mut peer, &STRANGER_SEED)
  });
  assert_eq!(answer, Some(51));
  assert_eq!(seen.learned, [(0, 0); 5]);
}

/// A-651 (main's ruling: an overdue connection is not cut while no new connection needs its seat). Do: with two seats and
/// one client, pause past the connection's bound between the server's `PK_OK` and the signature, as OpenSSH does while a
/// person types a key's passphrase; then run the command. Expect: the connection served to its end, marked overdue and
/// not reaped.
#[test]
fn a_passphrase_pause_is_not_cut_while_a_seat_is_free() {
  let (transcript, seen) = served(TWO_SEATS, |address| session(address, PAUSE_NS, 0));
  assert_eq!(transcript, Some(pushed()));
  assert_eq!((seen.counts.reaped, seen.counts.overdue >= 1), (0, true));
}

/// A-651 (main's ruling: the most overdue is reaped when a new connection needs its seat). Do: with one seat, pause a
/// client after the server's `PK_OK` until its connection ends; meanwhile connect a second client, which needs the seat,
/// and run its command. Expect: the paused connection reaped well before its patience ran out and the second served to
/// its end; the reap counted in authentication, which backs nothing off, since the reaped connection had proved no one.
#[test]
fn a_passphrase_pause_is_reaped_when_a_new_connection_needs_its_seat() {
  let ((first_cut, second), seen) = served(ONE_SEAT, |address| {
    let (paused, go) = mpsc::channel();
    let first = std::thread::spawn(move || {
      let mut peer = Peer::connect(address)?;
      (query(&mut peer, &HELD_SEED)? == 60).then_some(())?;
      paused.send(()).ok()?;
      Some(peer.ended())
    });
    let second = go
      .recv_timeout(Duration::from_nanos(PATIENCE_NS))
      .ok()
      .and_then(|()| session(address, 0, 0));
    (first.join().ok().flatten(), second)
  });
  assert_eq!((first_cut, second), (Some(true), Some(pushed())));
  assert_eq!(
    (seen.counts.reaped, seen.learned[1].1, seen.backoffs[1]),
    (1, 1, 0)
  );
}

/// A-651 (main's ruling; a push's client compresses before it sends). Do: with two seats and one client, run the command
/// and pause past the bound before sending the pack. Expect: the push served to its end, marked overdue and not reaped.
#[test]
fn a_slow_push_is_not_cut_while_a_seat_is_free() {
  let (transcript, seen) = served(TWO_SEATS, |address| session(address, 0, PAUSE_NS));
  assert_eq!(transcript, Some(pushed()));
  assert_eq!((seen.counts.reaped, seen.counts.overdue >= 1), (0, true));
}

/// A-651 (main's ruling). Do: with one seat, run a command and pause before the pack until the connection ends; meanwhile
/// connect a second client, which needs the seat, and run its command. Expect: the paused push reaped and the second
/// served; the reap counted in the running phase, which, since the reaped connection was proven, backs off once.
#[test]
fn a_slow_push_is_reaped_when_a_new_connection_needs_its_seat() {
  let ((first_cut, second), seen) = served(ONE_SEAT, |address| {
    let (paused, go) = mpsc::channel();
    let first = std::thread::spawn(move || {
      let mut peer = Peer::connect(address)?;
      (query(&mut peer, &HELD_SEED)? == 60).then_some(())?;
      (sign(&mut peer, &HELD_SEED)? == 52).then_some(())?;
      exec(&mut peer, b"git-receive-pack '/r.git'").then_some(())?;
      peer.next(&[94])?;
      paused.send(()).ok()?;
      Some(peer.ended())
    });
    let second = go
      .recv_timeout(Duration::from_nanos(PATIENCE_NS))
      .ok()
      .and_then(|()| session(address, 0, 0));
    (first.join().ok().flatten(), second)
  });
  assert_eq!((first_cut, second), (Some(true), Some(pushed())));
  assert_eq!(
    (
      seen.counts.reaped,
      seen.counts.full >= 1,
      seen.learned[3].1,
      seen.backoffs[3]
    ),
    (1, true, 1, 1)
  );
}

/// A-651 part 3 (a key whose record lives elsewhere). Do: with a site whose connection lookup cannot say of any key and
/// whose record lookup holds the held key, authenticate and run the command. Expect: the session waits for the lookup,
/// then proves the principal, and the command runs to its end as with a lookup that answers at once.
#[test]
fn a_key_looked_up_where_its_record_lives_proves_its_principal() {
  let (transcript, seen) = served_by(ONE_SEAT, || LooksUp, |address| session(address, 0, 0));
  assert_eq!(transcript, Some(pushed()));
  assert_eq!(seen.counts.commands_run, 1);
}
