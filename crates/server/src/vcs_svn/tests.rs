//! Tests of the daemon's svn:// listener (A-689, A-695; §4.17.4 piece 7): a daemon in-process starts it from its
//! configuration, and a client over loopback writes the bytes stock `svn` 1.15.0 wrote to stock `svnserve` 1.15.0 for a
//! URL no repository holds and for a repository that allows no one (`crates/svn/fixtures/handshake/no-repository.*.bin`
//! and `no-access.*.bin`).

use std::io::{Read, Write};
use std::net::TcpStream as ClientStream;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use slates_db::HostId;
use slates_db::catalog::Principal;
use slates_ipc::protocol::{ObjectFormat, ReplyBody, RequestBody, VcsTool};
use slates_rt::tcp::{Ipv4Addr, SocketAddrV4};

use crate::daemon::{Daemon, SegmentSource};

/// Shape: the daemon's shards: two, so the listener's control shard is one of several, as a daemon runs.
const SHARDS: u16 = 2;
/// Shape: how long the client waits for each read: the daemon's liveness budget.
const PATIENCE_NS: u64 = crate::daemon::LIVENESS_BUDGET_NS;
/// Format: what stock `svnserve` 1.15.0 wrote to stock `svn` 1.15.0 asking for [`RECORDED_URL`] under a root holding no
/// repository: its greeting, then its refusal (recorded on the Mac Studio, 2026-11-19).
const SERVER_BYTES: &[u8] =
  include_bytes!("../../../svn/fixtures/handshake/no-repository.server.bin");
/// Format: what that client wrote: its answer to the greeting, naming [`RECORDED_URL`].
const CLIENT_BYTES: &[u8] =
  include_bytes!("../../../svn/fixtures/handshake/no-repository.client.bin");
/// Format: the URL the recorded client named, at `svnserve`'s default port; a client of the listener names its port.
const RECORDED_URL: &str = "svn://127.0.0.1/missing";
/// Format: where the recorded server's refusal starts, after its greeting.
const REFUSAL_START: &[u8] = b"( failure ";
/// Format: the capability `svnserve`'s greeting announces and slates' does not, its word and the space after it: a
/// sender uses zlib only with a peer that announced `svndiff1` (`marshal.c`), so slates never receives zlib (§4.17.4
/// "5. The wire"; `slates_svn::handshake`).
const SVNDIFF1: &str = "svndiff1 ";
/// Format: what stock `svnserve` 1.15.0 wrote to stock `svn` 1.15.0 asking for [`NONE_URL`], a repository whose
/// configuration allows no one: its greeting, then "No access allowed to this repository" (recorded on the Mac Studio,
/// 2026-11-19).
const NO_ACCESS_SERVER: &[u8] =
  include_bytes!("../../../svn/fixtures/handshake/no-access.server.bin");
/// Format: what that client wrote: its answer to the greeting, naming [`NONE_URL`].
const NO_ACCESS_CLIENT: &[u8] =
  include_bytes!("../../../svn/fixtures/handshake/no-access.client.bin");
/// Format: the URL the recorded client named for the repository that allows no one.
const NONE_URL: &str = "svn://127.0.0.1/none";
/// Shape: the partition of the listener's shard, the control shard.
const LISTENER: usize = 0;
/// Shape: the partition of the other shard, which a connection moves to.
const OTHER: usize = 1;
/// Shape: the uid of the repositories' owner.
const OWNER: u32 = 1234;

/// A daemon of [`SHARDS`] shards whose configuration asks for the svn:// listener at `address`.
fn daemon(address: SocketAddrV4) -> Daemon {
  static NEXT: AtomicU32 = AtomicU32::new(0);
  let profile = crate::daemon::test_profile();
  let instance = format!(
    "vcs-svn-{}-{}",
    std::process::id(),
    NEXT.fetch_add(1, Ordering::Relaxed)
  );
  let mut config = crate::DaemonConfig::derive(&profile, &instance, Some(SHARDS));
  config.vcs_svn = Some(address);
  let daemon = Daemon::start(&profile, config, SegmentSource::Create { name: instance })
    .expect("the daemon starts");
  daemon
    .bootstrap(true)
    .expect("the daemon creates its initial group");
  daemon
}

/// The URL a client of the listener at loopback `port` names for the repository `missing`, as [`RECORDED_URL`] did.
fn url_at(port: u16) -> String {
  format!("svn://127.0.0.1:{port}/missing")
}

/// ra_svn's string item holding `text`: its length, a colon, its bytes (`marshal.c`).
fn string_item(text: &str) -> String {
  format!("{}:{text}", text.len())
}

/// `svnserve`'s words refusing `url`, under a root that holds no repository (`serve.c` `find_repos`).
fn no_repository(url: &str) -> String {
  format!("No repository found in '{url}'")
}

/// `recorded` with its one item `item` written as `replacement`: the recording must hold `item` exactly once, so the
/// replacement keeps every other byte as stock Subversion wrote it.
fn replaced(recorded: &[u8], item: &str, replacement: &str) -> Vec<u8> {
  let item = item.as_bytes();
  let places: Vec<usize> = recorded
    .windows(item.len())
    .enumerate()
    .filter(|(_, window)| *window == item)
    .map(|(place, _)| place)
    .collect();
  assert_eq!(places.len(), 1, "the recording holds the item once");
  let place = places[0];
  let mut bytes = recorded[..place].to_vec();
  bytes.extend_from_slice(replacement.as_bytes());
  bytes.extend_from_slice(&recorded[place + item.len()..]);
  bytes
}

/// The recorded client's answer, naming the listener at `port`.
fn answer_naming(port: u16) -> Vec<u8> {
  replaced(
    CLIENT_BYTES,
    &string_item(RECORDED_URL),
    &string_item(&url_at(port)),
  )
}

/// The recorded greeting, its `svndiff1` taken out ([`SVNDIFF1`]), and the recorded refusal naming the listener at
/// `port`.
fn svnserves_answers(port: u16) -> (Vec<u8>, Vec<u8>) {
  let refusal = SERVER_BYTES
    .windows(REFUSAL_START.len())
    .position(|window| window == REFUSAL_START)
    .expect("the recorded server refused");
  (
    replaced(&SERVER_BYTES[..refusal], SVNDIFF1, ""),
    replaced(
      &SERVER_BYTES[refusal..],
      &string_item(&no_repository(RECORDED_URL)),
      &string_item(&no_repository(&url_at(port))),
    ),
  )
}

/// What `stream` reads before the listener has sent `wanted` bytes, ends the connection, or is silent past the
/// stream's patience: as many bytes as came.
fn read_up_to(stream: &mut ClientStream, wanted: usize) -> Vec<u8> {
  let mut read = Vec::new();
  let mut chunk = [0u8; 4096];
  while read.len() < wanted {
    match stream.read(&mut chunk) {
      Ok(0) | Err(_) => break,
      Ok(taken) => read.extend_from_slice(&chunk[..taken]),
    }
  }
  read
}

/// Connects to the listener at loopback `port`, reads as many bytes as a greeting of `greeting_bytes` holds, writes
/// `answer`, and reads until the listener ends the connection: what came before the answer, and after, as text.
fn exchange(port: u16, greeting_bytes: usize, answer: &[u8]) -> (String, String) {
  let mut stream = ClientStream::connect(("127.0.0.1", port)).expect("the listener takes it");
  stream
    .set_read_timeout(Some(Duration::from_nanos(PATIENCE_NS)))
    .expect("a read timeout");
  let greeted = read_up_to(&mut stream, greeting_bytes);
  stream.write_all(answer).expect("the answer is sent");
  let mut rest = Vec::new();
  stream
    .read_to_end(&mut rest)
    .expect("the listener ends the connection");
  (text(&greeted), text(&rest))
}

/// `bytes` as text, for a comparison that prints both sides.
fn text(bytes: &[u8]) -> String {
  String::from_utf8_lossy(bytes).into_owned()
}

/// A-689 (§4.17.4 piece 7). Do: start a daemon asked for the svn:// listener on loopback at a port the system picks;
/// connect to the port it reports, read its first bytes, answer them as stock `svn` 1.15.0 answered `svnserve`, naming
/// the repository `missing` at the listener's port, and read until the connection ends. Expect: the greeting is
/// `svnserve`'s byte for byte but for `svndiff1` (§4.17.4 "5. The wire"), and the answer is refused as `svnserve`
/// refused it, "No repository found in" the URL the client named (210005), after which the listener closes the
/// connection, as `svnserve` does; since A-695 the name was looked up on the shard its partition is, and none held it.
#[test]
fn a_connection_is_greeted_as_svnserve_greets_and_its_url_refused_in_svnserves_words() {
  let daemon = daemon(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
  let port = daemon.vcs_svn_port().expect("the listener started");
  let (greeting, refusal) = svnserves_answers(port);
  assert_eq!(
    exchange(port, greeting.len(), &answer_naming(port)),
    (text(&greeting), text(&refusal)),
    "the listener greets as svnserve does, svndiff1 aside, and refuses the URL as svnserve refuses it"
  );
}

/// The owner of the tests' repositories, the principal of the human's channel that creates them.
fn owner() -> Principal {
  Principal::Uid {
    host: HostId(0),
    uid: OWNER,
  }
}

/// A Subversion repository's name that routes to partition `partition`.
fn name_on(partition: usize) -> String {
  (0u32..)
    .map(|serial| format!("svn-{serial}"))
    .find(|name| usize::from(crate::verbs::owner_of_name(name, usize::from(SHARDS))) == partition)
    .expect("a name routes there")
}

/// Creates the Subversion repository `name` on partition `partition`, owned by [`owner`], with the defaults `slates
/// repository create NAME --tool svn` sends (A-685).
fn create_svn(daemon: &Daemon, partition: usize, name: &str) {
  let name = name.to_owned();
  let created = crate::daemon::observe_at(daemon, partition, move |state| {
    crate::verbs::dispatch(
      state,
      1,
      &owner(),
      RequestBody::CreateRepository {
        name: name.clone(),
        tool: VcsTool::Subversion,
        format: ObjectFormat::Sha1,
        default_branch: "trunk".to_owned(),
        green: None,
      },
    )
  });
  assert!(!matches!(created, ReplyBody::Refused { .. }), "{created:?}");
}

/// A-695 (§4.17.4 piece 7). Do: on a daemon of two shards, create a Subversion repository whose name routes to the
/// listener's own partition and one whose name routes to the other shard's; for each, connect and answer the greeting
/// as stock `svn` 1.15.0 answered `svnserve` for a repository that allows no one, naming the repository at the
/// listener's port, and read until the connection ends. Expect: both are greeted as `svnserve` greets, `svndiff1` aside,
/// and answered as `svnserve` answered, "No access allowed to this repository" (170001): each was found on the shard
/// that owns its name, the other's after the connection moved there, and opened with the mechanisms it allows, none
/// yet (no anonymous principal, §4.13; CRAM-MD5's keys, A-692). Before A-695 both were "No repository found".
#[test]
fn a_repository_either_shard_owns_is_opened_there_and_refused_as_svnserve_refuses_one_allowing_no_one() {
  let daemon = daemon(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
  let port = daemon.vcs_svn_port().expect("the listener started");
  let refusal_at = NO_ACCESS_SERVER
    .windows(REFUSAL_START.len())
    .position(|window| window == REFUSAL_START)
    .expect("the recorded server refused");
  let greeting = replaced(&NO_ACCESS_SERVER[..refusal_at], SVNDIFF1, "");
  let refusal = &NO_ACCESS_SERVER[refusal_at..];
  for partition in [LISTENER, OTHER] {
    let name = name_on(partition);
    create_svn(&daemon, partition, &name);
    let url = format!("svn://127.0.0.1:{port}/{name}");
    let answer = replaced(NO_ACCESS_CLIENT, &string_item(NONE_URL), &string_item(&url));
    assert_eq!(
      exchange(port, greeting.len(), &answer),
      (text(&greeting), text(refusal)),
      "the repository on partition {partition}"
    );
  }
}

/// A-689 (§4.17.4 piece 7). Do: start a daemon that is no fleet node asked for the svn:// listener on every interface,
/// which the flag's reading refuses before any start (`slates_api::role::vcs_address`), as a configuration made some
/// other way could still ask. Expect: the daemon serves without the listener and reports no port: ra_svn is cleartext,
/// served on a loopback address alone where no fleet node's operator publishes it.
#[test]
fn a_daemon_that_is_no_fleet_node_starts_no_listener_off_loopback() {
  let daemon = daemon(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0));
  assert_eq!(daemon.vcs_svn_port(), None);
}
