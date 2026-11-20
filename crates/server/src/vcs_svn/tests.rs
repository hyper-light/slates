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
use slates_ipc::protocol::{CredentialMaterial, ObjectFormat, ReplyBody, RequestBody, VcsTool};
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

/// A Subversion repository's name beginning `prefix` that routes to partition `partition`.
fn name_on(prefix: &str, partition: usize) -> String {
  (0u32..)
    .map(|serial| format!("{prefix}{serial}"))
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

/// The token the tests' owner enrolls, the password a stock client answers CRAM-MD5's challenge with.
const TOKEN: &[u8] = b"slates-svn-listener-token";

/// Enrolls [`TOKEN`] for [`owner`] on the partition its id routes to, as the CLI's issuer sends it (its id and its
/// CRAM-MD5 contexts, never the token).
fn enrolled(daemon: &Daemon) {
  let material = CredentialMaterial::HttpToken {
    id: crate::credentials::token_id(TOKEN),
    cram_md5_contexts: crate::credentials::cram_md5_contexts(TOKEN),
  };
  let id = crate::credentials::id_of(&material);
  let partition = usize::from(crate::credentials::owner_of(&id, usize::from(SHARDS)));
  let reply = crate::daemon::observe_at(daemon, partition, move |state| {
    let principal = slates_ipc::protocol::Principal::Uid {
      host: 0,
      uid: OWNER,
    };
    let proof = crate::credentials::enroll_proof(&state.issuer_secret, &principal, &material, None);
    let body = RequestBody::EnrollCredential {
      principal,
      material,
      expires_ns: None,
      proof,
    };
    crate::verbs::dispatch(state, 0, &owner(), body)
  });
  assert_eq!(reply, ReplyBody::CredentialEnrolled { id });
}

/// What `stream` reads until the listener has sent `items` whole items, or ends, or is silent past its patience.
fn read_items(stream: &mut ClientStream, items: usize) -> String {
  let mut read = Vec::new();
  let (mut depth, mut whole) = (0usize, 0usize);
  let mut byte = [0u8; 1];
  while whole < items {
    match stream.read(&mut byte) {
      Ok(1) => {}
      _ => break,
    }
    read.push(byte[0]);
    match byte[0] {
      b'(' => depth += 1,
      b')' => {
        depth = depth.saturating_sub(1);
        whole += usize::from(depth == 0);
      }
      _ => {}
    }
  }
  // The space after the last item's parenthesis.
  if stream.read(&mut byte).is_ok_and(|taken| taken == 1) {
    read.push(byte[0]);
  }
  text(&read)
}

/// The string inside the one `( step ( N:string ) )` item `item`.
fn challenge_of(item: &str) -> &str {
  let length_end = item.find(':').expect("the challenge's length");
  let start = item
    .get(..length_end)
    .and_then(|head| head.rfind(' '))
    .expect("a space")
    + 1;
  let length: usize = item.get(start..length_end).unwrap().parse().unwrap();
  item.get(length_end + 1..length_end + 1 + length).unwrap()
}

/// A client signed in: its stream, what the listener wrote after its greeting (the authentication request, the
/// challenge, the success and `repos-info`), the challenge's item and the repository's UUID.
struct SignedIn {
  /// The stream.
  stream: ClientStream,
  /// What the listener wrote after its greeting.
  written: String,
  /// The challenge's item.
  step: String,
  /// The repository's UUID, as its realm named it.
  uuid: String,
}

/// A stock client's CRAM-MD5 sign-in with the listener at `port` for `url` (the greeting checked against `greeting`):
/// it answers as stock `svn` 1.15.0 answered `svnserve`, chooses CRAM-MD5, and answers the challenge as the token's id
/// with [`TOKEN`] as its password.
fn sign_in(port: u16, greeting: &[u8], url: &str) -> SignedIn {
  let mut stream = ClientStream::connect(("127.0.0.1", port)).expect("the listener takes it");
  stream
    .set_read_timeout(Some(Duration::from_nanos(PATIENCE_NS)))
    .expect("a read timeout");
  let greeted = read_up_to(&mut stream, greeting.len());
  assert_eq!(text(&greeted), text(greeting), "the greeting");
  let answer = replaced(NO_ACCESS_CLIENT, &string_item(NONE_URL), &string_item(url));
  stream.write_all(&answer).expect("the answer is sent");
  let asked = read_items(&mut stream, 1);
  stream
    .write_all(b"( CRAM-MD5 ( ) ) ")
    .expect("the mechanism");
  let step = read_items(&mut stream, 1);
  let challenge = challenge_of(&step).to_owned();
  let user: String = crate::credentials::token_id(TOKEN)
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect();
  let digest: String = slates_svn::handshake::CramKey::from_secret(TOKEN)
    .digest(challenge.as_bytes())
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect();
  let response = format!("{user} {digest}");
  stream
    .write_all(format!("{}:{response} ", response.len()).as_bytes())
    .expect("the response");
  let established = read_items(&mut stream, 2);
  let uuid = asked
    .split_once("36:")
    .and_then(|(_, rest)| rest.get(..36))
    .unwrap_or_default()
    .to_owned();
  SignedIn {
    stream,
    written: format!("{asked}{step}{established}"),
    step,
    uuid,
  }
}

/// Writes `command` to `stream` and reads the `items` whole items the listener answers it with.
fn asked(stream: &mut ClientStream, command: &[u8], items: usize) -> String {
  stream.write_all(command).expect("the command");
  read_items(stream, items)
}

/// What stock `svnserve` 1.15.0 wrote in its recorded `cram-md5` session after its greeting, for a repository of
/// `uuid` at root URL `root` and the listener's challenge item `step`.
fn signed_in_as_svnserve(uuid: &str, step: &str, root: &str) -> String {
  format!(
    "( success ( ( CRAM-MD5 ) 36:{uuid} ) ) {step}( success ( ) ) ( success ( 36:{uuid} {} ( mergeinfo ) ) ) ",
    string_item(root)
  )
}

/// A stock client's CRAM-MD5 session with the listener at `port` for `url`, whose root URL is `root`, asking
/// `get-latest-rev` once signed in: what the listener wrote after its greeting, and what stock `svnserve` 1.15.0 wrote
/// in its own recorded `cram-md5` session (the realm and the UUID the repository's, the challenge the listener's).
fn cram_session(port: u16, greeting: &[u8], url: &str, root: &str) -> (String, String) {
  let mut signed = sign_in(port, greeting, url);
  let latest = asked(&mut signed.stream, b"( get-latest-rev ( ) ) ", 2);
  let expected = format!(
    "{}( success ( ( ) 0: ) ) ( success ( 0 ) ) ",
    signed_in_as_svnserve(&signed.uuid, &signed.step, root)
  );
  (format!("{}{latest}", signed.written), expected)
}

/// A-692's svn:// half (§4.17.4 item 10; A-695's lookup). Do: on a daemon of two shards, enroll a token for the
/// repositories' owner; create a Subversion repository whose name routes to the listener's own partition, one whose
/// name routes to the other shard's, and one there whose name holds a `/`; for each, connect as stock `svn` 1.15.0
/// does (the last naming a path beneath it), choose CRAM-MD5, answer the challenge with the token's id as the user and
/// the token as the password, then ask `get-latest-rev`. Expect: each is greeted as `svnserve` greets, `svndiff1`
/// aside, asked for CRAM-MD5 with the repository's UUID as its realm, challenged, authenticated, told the repository's
/// UUID and root URL, and answered revision 0, as stock `svnserve` answered its recorded `cram-md5` session: each found
/// on the shard that owns its name, the other's after the connection moved there, its sources lent there. Before this,
/// each was refused "No access allowed to this repository" (170001), as w-interop's stock `svn checkout` found.
#[test]
fn a_stock_client_signs_in_with_an_enrolled_token_on_either_shard_and_reads() {
  let daemon = daemon(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
  let port = daemon.vcs_svn_port().expect("the listener started");
  enrolled(&daemon);
  let refusal_at = NO_ACCESS_SERVER
    .windows(REFUSAL_START.len())
    .position(|window| window == REFUSAL_START)
    .expect("the recorded server refused");
  let greeting = replaced(&NO_ACCESS_SERVER[..refusal_at], SVNDIFF1, "");
  let cases = [
    ("svn-", LISTENER, ""),
    ("svn-", OTHER, ""),
    ("team/svn-", OTHER, "/trunk/a"),
  ];
  for (prefix, partition, beneath) in cases {
    let name = name_on(prefix, partition);
    create_svn(&daemon, partition, &name);
    let root = format!("svn://127.0.0.1:{port}/{name}");
    let (session, expected) = cram_session(port, &greeting, &format!("{root}{beneath}"), &root);
    assert_eq!(session, expected, "{root}");
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

/// Format: stock `svn` 1.15.0's `mkdir` commit as it wrote it to `svnserve` (A-655's recording), from its `commit` to
/// its `close-edit`.
const MKDIR_COMMIT: &[u8] = include_bytes!("../../../svn/fixtures/commit/mkdir.1.client.bin");

/// Format: where the recorded client's commit starts, after its handshake.
const COMMIT_START: &[u8] = b"( commit ";

/// A-698's svn:// half (§4.17.4 piece 7; after A-692 continued (1)). Do: on a daemon of two shards, enroll a token for
/// the repositories' owner and create a Subversion repository on the other shard's partition; sign in over the
/// listener with the token, send stock `svn` 1.15.0's recorded `mkdir` commit of `/trunk`, then ask `get-latest-rev`.
/// Expect: the commit is answered as `svnserve` answered it (the trivial authentication, the commit's success,
/// `close-edit`'s, the trivial authentication again, then `commit-info` naming revision 1), and the youngest revision
/// is then 1: the work landed through the owner's commit step on the shard the connection moved to. Before this, the
/// listener ended the connection at the commit.
#[test]
fn a_stock_commit_over_svn_lands_through_the_owners_step() {
  let daemon = daemon(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
  let port = daemon.vcs_svn_port().expect("the listener started");
  enrolled(&daemon);
  let refusal_at = NO_ACCESS_SERVER
    .windows(REFUSAL_START.len())
    .position(|window| window == REFUSAL_START)
    .expect("the recorded server refused");
  let greeting = replaced(&NO_ACCESS_SERVER[..refusal_at], SVNDIFF1, "");
  let name = name_on("svn-", OTHER);
  create_svn(&daemon, OTHER, &name);
  let root = format!("svn://127.0.0.1:{port}/{name}");
  let mut signed = sign_in(port, &greeting, &root);
  let commit_at = MKDIR_COMMIT
    .windows(COMMIT_START.len())
    .position(|window| window == COMMIT_START)
    .expect("the recorded commit");
  let committed = asked(&mut signed.stream, &MKDIR_COMMIT[commit_at..], 5);
  let latest = asked(&mut signed.stream, b"( get-latest-rev ( ) ) ", 2);
  let made =
    "( success ( ( ) 0: ) ) ( success ( ) ) ( success ( ) ) ( success ( ( ) 0: ) ) ( 1 ( 27:";
  assert!(committed.starts_with(made), "{committed}");
  assert!(committed.ends_with(" ) ( ) ) "), "{committed}");
  assert_eq!(latest, "( success ( ( ) 0: ) ) ( success ( 1 ) ) ");
}

/// A-693 part 2 over the listener (`serve.c` `lock_many`). Do: on a daemon of two shards, sign in to a Subversion
/// repository on the other shard's partition, commit stock `svn`'s recorded `mkdir` of `/trunk`, then ask `lock-many`
/// for `trunk`, a directory, and `get-lock` of it. Expect: the verb answered as `svnserve` answers one path refused: the
/// trivial authentication request, the path's failure in FSFS's words ("is not a file"), `done`, then success; and no
/// lock held. Before A-693 part 2 the listener answered `lock-many` "Unknown editor command" (210001), which stock `svn
/// lock` reports as E170003.
#[test]
fn a_lock_over_svn_is_judged_and_answered_as_svnserve_answers_it() {
  let daemon = daemon(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
  let port = daemon.vcs_svn_port().expect("the listener started");
  enrolled(&daemon);
  let refusal_at = NO_ACCESS_SERVER
    .windows(REFUSAL_START.len())
    .position(|window| window == REFUSAL_START)
    .expect("the recorded server refused");
  let greeting = replaced(&NO_ACCESS_SERVER[..refusal_at], SVNDIFF1, "");
  let name = name_on("svn-", OTHER);
  create_svn(&daemon, OTHER, &name);
  let root = format!("svn://127.0.0.1:{port}/{name}");
  let mut signed = sign_in(port, &greeting, &root);
  let commit_at = MKDIR_COMMIT
    .windows(COMMIT_START.len())
    .position(|window| window == COMMIT_START)
    .expect("the recorded commit");
  let committed = asked(&mut signed.stream, &MKDIR_COMMIT[commit_at..], 5);
  assert!(committed.ends_with(" ) ( ) ) "), "{committed}");
  let locked = asked(
    &mut signed.stream,
    b"( lock-many ( ( 4:held ) false ( ( 5:trunk ( ) ) ) ) ) ",
    2,
  );
  let done = read_items_or_word(&mut signed.stream);
  let held = asked(&mut signed.stream, b"( get-lock ( 5:trunk ) ) ", 2);
  assert!(
    locked.starts_with("( success ( ( ) 0: ) ) ( failure ( ( ") && locked.contains("is not a file"),
    "{locked}"
  );
  assert_eq!(done, "done ( success ( ) ) ");
  assert_eq!(held, "( success ( ( ) 0: ) ) ( success ( ( ) ) ) ");
}

/// What `stream` reads through `lock-many`'s end: the bare word `done`, then one item.
fn read_items_or_word(stream: &mut ClientStream) -> String {
  let mut word = Vec::new();
  let mut byte = [0u8; 1];
  while word.len() < "done ".len() {
    match stream.read(&mut byte) {
      Ok(1) => word.push(byte[0]),
      _ => break,
    }
  }
  format!("{}{}", text(&word), read_items(stream, 1))
}

/// A hook program written as a shell script into `dir`, named for its point, made executable: its path. A hook's
/// environment is its repository's hooks-env alone (A-623), so the script names every program by its absolute path.
#[cfg(unix)]
#[allow(clippy::disallowed_methods)] // test code: its own scratch directory
pub(crate) fn hook_script(dir: &std::path::Path, point: &str, body: &str) -> String {
  use std::os::unix::fs::PermissionsExt;
  let path = dir.join(point);
  std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("the hook written");
  std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("made executable");
  // Run once here, as a hook already in place has run before: macOS assesses an executable at its first run, 0.16 to
  // 0.29 s for a new script on the Mac Studio, and longer under parallel tests, which is no part of the hook's run.
  let _ = std::process::Command::new(&path)
    .env_clear()
    .current_dir(dir)
    .output();
  path.to_string_lossy().into_owned()
}

/// Configures Subversion repository `name`'s policy, from its first version, to run `hooks` (each a point and its
/// program), with no hooks-env.
#[cfg(unix)]
fn set_svn_hooks(
  daemon: &Daemon,
  partition: usize,
  name: &str,
  hooks: Vec<(slates_ipc::protocol::HookPointName, String)>,
) {
  let name = name.to_owned();
  let reply = crate::daemon::observe_at(daemon, partition, move |state| {
    crate::verbs::dispatch(
      state,
      1,
      &owner(),
      RequestBody::SetRepositoryPolicy {
        repository: name.clone(),
        read: 0,
        rules: Vec::new(),
        hooks: hooks
          .iter()
          .map(|(point, program)| slates_ipc::protocol::HookConfig {
            point: *point,
            name: point_name(*point).to_owned(),
            program: program.clone(),
          })
          .collect(),
        hooks_env: Vec::new(),
        signers: Vec::new(),
      },
    )
  });
  assert_eq!(reply, ReplyBody::Acknowledged);
}

/// The name Subversion gives a hook at `point` (its file's name under `hooks/`), which a hook of the point is named for.
#[cfg(unix)]
fn point_name(point: slates_ipc::protocol::HookPointName) -> &'static str {
  use slates_ipc::protocol::HookPointName as Point;
  match point {
    Point::SvnStartCommit => "start-commit",
    Point::SvnPreCommit => "pre-commit",
    Point::SvnPostCommit => "post-commit",
    Point::SvnPreRevpropChange => "pre-revprop-change",
    Point::SvnPostRevpropChange => "post-revprop-change",
    Point::SvnPreLock => "pre-lock",
    Point::SvnPostLock => "post-lock",
    Point::SvnPreUnlock => "pre-unlock",
    Point::SvnPostUnlock => "post-unlock",
    _ => "other",
  }
}

/// What a hook script recorded in `dir` under `file`: its lines.
#[cfg(unix)]
#[allow(clippy::disallowed_methods)] // test code: its own scratch directory
fn recorded_lines(dir: &std::path::Path, file: &str) -> Vec<String> {
  std::fs::read_to_string(dir.join(file))
    .unwrap_or_default()
    .lines()
    .map(str::to_owned)
    .collect()
}

/// The root mount the hook tests publish (A-645 P3): a hook's REPOS is the repository's name beneath it.
#[cfg(unix)]
pub(crate) const TEST_ROOT: &str = "/slates-root";

/// Signs in to a fresh Subversion repository on the other shard's partition whose policy runs `hooks`, the daemon
/// having published `root` as its root mount where one is given, and sends stock
/// `svn` 1.15.0's recorded `mkdir` commit of `/trunk`: the repository's name, the commit's answer (`items` items: five
/// for a commit made, three for one refused at `open-root`) and the youngest revision then, asked on a second
/// connection, since a refused commit's client sends `abort-edit`, which the recording does not hold.
#[cfg(unix)]
fn commit_under_hooks(
  items: usize,
  root: Option<&std::path::Path>,
  hooks: impl FnOnce(&str) -> Vec<(slates_ipc::protocol::HookPointName, String)>,
) -> (String, String, String) {
  let daemon = daemon(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
  if let Some(root) = root {
    daemon
      .record_root_mount(root)
      .expect("the root mount recorded");
  }
  let port = daemon.vcs_svn_port().expect("the listener started");
  enrolled(&daemon);
  let refusal_at = NO_ACCESS_SERVER
    .windows(REFUSAL_START.len())
    .position(|window| window == REFUSAL_START)
    .expect("the recorded server refused");
  let greeting = replaced(&NO_ACCESS_SERVER[..refusal_at], SVNDIFF1, "");
  let name = name_on("svn-hooked-", OTHER);
  create_svn(&daemon, OTHER, &name);
  set_svn_hooks(&daemon, OTHER, &name, hooks(&name));
  let root = format!("svn://127.0.0.1:{port}/{name}");
  let mut signed = sign_in(port, &greeting, &root);
  // A hook's process is started and reaped within the answer: the client waits ten windows for it.
  signed
    .stream
    .set_read_timeout(Some(Duration::from_nanos(10 * PATIENCE_NS)))
    .expect("the client's wait");
  let commit_at = MKDIR_COMMIT
    .windows(COMMIT_START.len())
    .position(|window| window == COMMIT_START)
    .expect("the recorded commit");
  let committed = asked(&mut signed.stream, &MKDIR_COMMIT[commit_at..], items);
  let mut other = sign_in(port, &greeting, &root);
  let latest = asked(&mut other.stream, b"( get-latest-rev ( ) ) ", 2);
  (name, committed, latest)
}

/// A-730 (`svn_repos__hooks_start_commit`, `check_hook_result`). Do: configure a start-commit hook that writes to standard
/// error and exits 1, and send the recorded commit. Expect: the commit refused at `open-root` in `svnserve`'s words,
/// "Commit blocked by start-commit hook (exit code 1) with output:" and the hook's standard error (E165001), and the
/// youngest revision still 0. Before A-730 the listener refused any commit to a repository with a hook configured, in
/// its own words ("does not run this repository's hooks yet"), whatever the hook would have said.
#[cfg(unix)]
#[test]
fn a_start_commit_hook_that_refuses_refuses_the_commit_in_svnserves_words() {
  let dir = crate::git_hooks::tests::ScratchDir::new();
  let refusing = hook_script(&dir.path, "start-commit", "echo 'not today' >&2\nexit 1");
  let (_, committed, latest) = commit_under_hooks(3, Some(std::path::Path::new(TEST_ROOT)), |_| {
    vec![(
      slates_ipc::protocol::HookPointName::SvnStartCommit,
      refusing,
    )]
  });
  let words = "Commit blocked by start-commit hook (exit code 1) with output:\nnot today\n";
  assert!(
    committed.contains("165001") && committed.contains(&format!("{}:{words}", words.len())),
    "{committed}"
  );
  assert_eq!(latest, "( success ( ( ) 0: ) ) ( success ( 0 ) ) ");
}

/// A-645 P3 (main's ruling 1). Do: configure a start-commit hook that records that it ran and accepts, publish no root
/// mount, and send the recorded commit. Expect: the commit refused at `open-root`, typed, in words naming why (the
/// daemon's root mount is not yet published, so it has nowhere to show the hook its repository); the hook never run;
/// the youngest revision still 0. Before A-645 P3 the hook ran with REPOS `/<name>`, a path that holds nothing.
#[cfg(unix)]
#[test]
fn a_hook_before_the_root_mount_is_published_is_refused_typed() {
  let dir = crate::git_hooks::tests::ScratchDir::new();
  let at = dir.path.display().to_string();
  let accepting = hook_script(&dir.path, "start-commit", &format!("echo ran > {at}/ran"));
  // The warm-up run of `hook_script` wrote the mark; only a run of the commit's may write it again.
  std::fs::remove_file(dir.path.join("ran")).expect("the warm-up's mark removed");
  let (_, committed, latest) = commit_under_hooks(3, None, |_| {
    vec![(
      slates_ipc::protocol::HookPointName::SvnStartCommit,
      accepting,
    )]
  });
  assert!(
    committed.contains(&super::UNSUPPORTED_FEATURE.to_string())
      && committed.contains(super::ROOT_UNPUBLISHED_WORDS),
    "{committed}"
  );
  assert!(!dir.path.join("ran").exists(), "the hook ran");
  assert_eq!(latest, "( success ( ( ) 0: ) ) ( success ( 0 ) ) ");
}

/// A-730 (`hooks.c` and `commit.c`, as A-623 recorded stock `svnserve` 1.15.0's runs). Do: configure a start-commit hook
/// and a pre-commit hook that each record their arguments (and pre-commit its standard input) and accept, and a
/// post-commit hook that writes to standard error and exits 1; send the recorded commit. Expect: start-commit given the
/// repository, the user, `mergeinfo` (the client announced it) and the transaction; pre-commit the repository and the
/// same transaction, its standard input empty (no lock tokens); the commit made as revision 1, its `commit-info` carrying
/// post-commit's failure in `svnserve`'s words as `post-commit-err`.
#[cfg(unix)]
#[test]
fn a_commit_over_svn_runs_its_repositorys_commit_hooks_as_svnserve_runs_them() {
  use slates_ipc::protocol::HookPointName;
  let dir = crate::git_hooks::tests::ScratchDir::new();
  let at = dir.path.display().to_string();
  let start = hook_script(
    &dir.path,
    "start-commit",
    &format!("for arg in \"$@\"; do echo \"$arg\"; done > {at}/start.args"),
  );
  let pre = hook_script(
    &dir.path,
    "pre-commit",
    &format!(
      "for arg in \"$@\"; do echo \"$arg\"; done > {at}/pre.args\n/bin/cat > {at}/pre.stdin"
    ),
  );
  let post = hook_script(&dir.path, "post-commit", "echo 'post said no' >&2\nexit 1");
  let (name, committed, latest) =
    commit_under_hooks(5, Some(std::path::Path::new(TEST_ROOT)), |_| {
      vec![
        (HookPointName::SvnStartCommit, start),
        (HookPointName::SvnPreCommit, pre),
        (HookPointName::SvnPostCommit, post),
      ]
    });
  let started = recorded_lines(&dir.path, "start.args");
  let precommitted = recorded_lines(&dir.path, "pre.args");
  let repos = format!("{TEST_ROOT}/{name}");
  assert_eq!(
    (
      started.first(),
      started.get(2).map(String::as_str),
      started.len(),
      precommitted.first(),
      precommitted.get(1),
      precommitted.len(),
      recorded_lines(&dir.path, "pre.stdin").len(),
    ),
    (
      Some(&repos),
      Some("mergeinfo"),
      4,
      Some(&repos),
      started.get(3),
      2,
      0
    ),
    "start-commit {started:?}, pre-commit {precommitted:?}"
  );
  assert!(committed.contains("post said no"), "{committed}");
  assert_eq!(latest, "( success ( ( ) 0: ) ) ( success ( 1 ) ) ");
}

/// Stock `svn` 1.15.0's recorded commit of additions and deletions, whose `new.txt` the lock tests' file commit takes its
/// text delta and checksum from.
#[cfg(unix)]
const ADD_DELETE_COMMIT: &[u8] =
  include_bytes!("../../../svn/fixtures/commit/add-delete.1.client.bin");

/// A commit adding `new.txt` at the session's root ("a new file"), its text delta and checksum as stock `svn` sent them
/// for that file in [`ADD_DELETE_COMMIT`].
#[cfg(unix)]
fn new_file_commit() -> Vec<u8> {
  let start = b"( apply-textdelta ( 2:c1 ( ) ) ) ";
  let end = b"( close-file ( 2:c1 ( 32:aff8766b86bae76c1fc4a203ab1b1ec6 ) ) ) ";
  let from = ADD_DELETE_COMMIT
    .windows(start.len())
    .position(|window| window == start)
    .expect("the recorded file's delta");
  let to = ADD_DELETE_COMMIT
    .windows(end.len())
    .position(|window| window == end)
    .expect("the recorded file's close")
    + end.len();
  let mut commit =
    b"( commit ( 4:file ( ) false ( ( 7:svn:log 4:file ) ) ) ) ( open-root ( ( ) 2:d0 ) ) \
( add-file ( 7:new.txt 2:d0 2:c1 ( ) ) ) ( close-dir ( 2:d0 ) ) "
      .to_vec();
  commit.extend_from_slice(&ADD_DELETE_COMMIT[from..to]);
  commit.extend_from_slice(b"( close-edit ( ) ) ");
  commit
}

/// Signs in to a fresh Subversion repository on the other shard's partition whose policy runs `hooks`, commits
/// [`new_file_commit`], then asks `verb` (a lock verb's item): what it answered through its `done` and one item more.
#[cfg(unix)]
fn lock_under_hooks(
  hooks: Vec<(slates_ipc::protocol::HookPointName, String)>,
  verbs: &[&[u8]],
) -> Vec<String> {
  let verbs: Vec<(&[u8], bool)> = verbs.iter().map(|verb| (*verb, true)).collect();
  under_policy(Some(hooks), &verbs)
}

/// Signs in to a fresh repository whose policy is set to run `hooks`, or never set for none (as `slates repository
/// create` leaves it), commits a file, and sends each of `verbs`: its first two items, and for a lock verb the rest of
/// its answer.
#[cfg(unix)]
fn under_policy(
  hooks: Option<Vec<(slates_ipc::protocol::HookPointName, String)>>,
  verbs: &[(&[u8], bool)],
) -> Vec<String> {
  let daemon = daemon(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
  daemon
    .record_root_mount(std::path::Path::new(TEST_ROOT))
    .expect("the root mount recorded");
  let port = daemon.vcs_svn_port().expect("the listener started");
  enrolled(&daemon);
  let refusal_at = NO_ACCESS_SERVER
    .windows(REFUSAL_START.len())
    .position(|window| window == REFUSAL_START)
    .expect("the recorded server refused");
  let greeting = replaced(&NO_ACCESS_SERVER[..refusal_at], SVNDIFF1, "");
  let name = name_on("svn-locked-", OTHER);
  create_svn(&daemon, OTHER, &name);
  if let Some(hooks) = hooks {
    set_svn_hooks(&daemon, OTHER, &name, hooks);
  }
  let root = format!("svn://127.0.0.1:{port}/{name}");
  let mut signed = sign_in(port, &greeting, &root);
  signed
    .stream
    .set_read_timeout(Some(Duration::from_nanos(10 * PATIENCE_NS)))
    .expect("the client's wait");
  let committed = asked(&mut signed.stream, &new_file_commit(), 5);
  assert!(committed.ends_with(" ) ( ) ) "), "{committed}");
  verbs
    .iter()
    .map(|(verb, lock)| {
      let answered = asked(&mut signed.stream, verb, 2);
      if *lock {
        format!("{answered}{}", read_items_or_word(&mut signed.stream))
      } else {
        answered
      }
    })
    .collect()
}

/// A-730 (`hooks.c` `svn_repos__hooks_pre_lock`, `check_hook_result`; `fs-wrap.c` `svn_repos_fs_lock_many`). Do: commit
/// a file, configure a pre-lock hook that writes to standard error and exits 1, ask `lock-many` for the file, then
/// `get-lock` of it. Expect: the path refused in `svnserve`'s words, "Lock blocked by pre-lock hook (exit code 1) with
/// output:" and the hook's standard error (E165001), then `done` and success; and no lock held.
#[cfg(unix)]
#[test]
fn a_pre_lock_hook_that_refuses_refuses_its_path_in_svnserves_words() {
  use slates_ipc::protocol::HookPointName;
  let dir = crate::git_hooks::tests::ScratchDir::new();
  let refusing = hook_script(&dir.path, "pre-lock", "echo 'no locks here' >&2\nexit 1");
  let answers = lock_under_hooks(
    vec![(HookPointName::SvnPreLock, refusing)],
    &[b"( lock-many ( ( 4:held ) false ( ( 7:new.txt ( ) ) ) ) ) "],
  );
  let words = "Lock blocked by pre-lock hook (exit code 1) with output:\nno locks here\n";
  let locked = answers.first().expect("the lock's answer");
  assert!(
    locked.contains("165001") && locked.contains(&format!("{}:{words}", words.len())),
    "{locked}"
  );
  assert!(locked.ends_with("done ( success ( ) ) "), "{locked}");
}

/// A-730 (`hooks.c`, as A-623 recorded stock `svnserve` 1.15.0's runs). Do: commit a file; configure a pre-lock hook that
/// records its arguments and writes a token to standard output, and a post-lock hook that records its arguments and
/// standard input, writes to standard error and exits 1; ask `lock-many` for the file. Expect: pre-lock given the
/// repository, the path, the user, the comment and `0` (not stolen); the file locked with the hook's token; then, after
/// `done`, post-lock's failure (E165008 "Locking succeeded, but post-lock hook failed" over E165001), post-lock given the
/// repository and the user, the locked path on its standard input.
#[cfg(unix)]
#[test]
fn a_lock_runs_pre_lock_for_its_token_and_post_lock_after_it_as_svnserve_does() {
  use slates_ipc::protocol::HookPointName;
  let dir = crate::git_hooks::tests::ScratchDir::new();
  let at = dir.path.display().to_string();
  let pre = hook_script(
    &dir.path,
    "pre-lock",
    &format!(
      "for arg in \"$@\"; do echo \"$arg\"; done > {at}/pre.args\nprintf 'opaquelocktoken:from-the-hook'"
    ),
  );
  let post = hook_script(
    &dir.path,
    "post-lock",
    &format!(
      "for arg in \"$@\"; do echo \"$arg\"; done > {at}/post.args\n/bin/cat > {at}/post.stdin\n\
       echo 'post said no' >&2\nexit 1"
    ),
  );
  let answers = lock_under_hooks(
    vec![
      (HookPointName::SvnPreLock, pre),
      (HookPointName::SvnPostLock, post),
    ],
    &[b"( lock-many ( ( 4:held ) false ( ( 7:new.txt ( ) ) ) ) ) "],
  );
  let locked = answers.first().expect("the lock's answer");
  let pre_args = recorded_lines(&dir.path, "pre.args");
  let post_args = recorded_lines(&dir.path, "post.args");
  assert_eq!(
    (
      pre_args.get(1).map(String::as_str),
      pre_args.get(3).map(String::as_str),
      pre_args.get(4).map(String::as_str),
      pre_args.len(),
      post_args.len(),
      post_args.first() == pre_args.first(),
      post_args.get(1) == pre_args.get(2),
      recorded_lines(&dir.path, "post.stdin"),
    ),
    (
      Some("/new.txt"),
      Some("held"),
      Some("0"),
      5,
      2,
      true,
      true,
      vec!["/new.txt".to_owned()]
    ),
    "pre-lock {pre_args:?}, post-lock {post_args:?}"
  );
  assert!(locked.contains("opaquelocktoken:from-the-hook"), "{locked}");
  assert!(
    locked.contains("done ( failure ( ( 165008 ") && locked.contains("post said no"),
    "{locked}"
  );
}

/// §4.17.4 (a Subversion repository is one green; a branch is a directory in it, made by a copy): the lineage's branch
/// verbs are refused by kind on a Subversion repository, so none makes a green no svn client reaches or unbinds the one
/// it serves. Do: create a Subversion repository, then ask `CreateBranch` of a branch from its root, `BindBranch` of a
/// name, and `RetireBranch` of its root; then sign in over svn:// as a stock client does and ask `get-latest-rev`.
/// Expect: each verb refused `Unsupported` naming Subversion's directories; the repository still served, revision 0
/// its youngest. Before, `CreateBranch` made a second green, which no Subversion path serves.
#[test]
fn a_subversion_repositorys_branch_verbs_are_refused_by_kind() {
  use slates_ipc::protocol::Refusal;
  let daemon = daemon(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
  let port = daemon.vcs_svn_port().expect("the listener started");
  enrolled(&daemon);
  let name = name_on("svn-branched-", OTHER);
  create_svn(&daemon, OTHER, &name);
  let (root, green) = crate::daemon::observe_at(&daemon, OTHER, {
    let name = name.clone();
    move |state| {
      let repositories = state.db.partition().repositories();
      let record = repositories.repository_named(&name)?;
      let (root, green) = repositories.branches(record.id, "").next()?;
      Some((root.to_owned(), green))
    }
  })
  .expect("the repository's root is bound");
  let verbs = [
    RequestBody::CreateBranch {
      repository: name.clone(),
      name: "feature".to_owned(),
      from: root.clone(),
      version: None,
    },
    RequestBody::BindBranch {
      repository: name.clone(),
      name: "feature".to_owned(),
      green: crate::verbs::to_wire_volume(green),
    },
    RequestBody::RetireBranch {
      repository: name.clone(),
      name: root,
    },
  ];
  for verb in verbs {
    let asked = format!("{verb:?}");
    let reply = crate::daemon::observe_at(&daemon, OTHER, move |state| {
      crate::verbs::dispatch(state, 1, &owner(), verb.clone())
    });
    assert!(
      matches!(
        &reply,
        ReplyBody::Refused {
          refusal: Refusal::Unsupported { feature },
        } if feature.contains("Subversion")
      ),
      "{asked}: {reply:?}"
    );
  }
  let refusal_at = NO_ACCESS_SERVER
    .windows(REFUSAL_START.len())
    .position(|window| window == REFUSAL_START)
    .expect("the recorded server refused");
  let greeting = replaced(&NO_ACCESS_SERVER[..refusal_at], SVNDIFF1, "");
  let mut signed = sign_in(port, &greeting, &format!("svn://127.0.0.1:{port}/{name}"));
  let latest = asked(&mut signed.stream, b"( get-latest-rev ( ) ) ", 2);
  assert_eq!(latest, "( success ( ( ) 0: ) ) ( success ( 0 ) ) ");
}

/// The svn:// listener's turns yield (w-interop's git/git `svnrdump dump`, 2026-11-20: one `replay-range` held shard 0
/// 3596 ms without a step). Do: sign in, then send 64 `get-latest-rev` commands in one write, so every turn after the
/// first finds its command already read, and read their 128 answers as fast as they come. Expect: every answer, and no
/// turn begun in the same shard poll as the turn before it (`TURNS_UNYIELDED` 0 on every shard). Red before the yield
/// (a469a6b85's parent): 63 or so, one for each pipelined command after the first.
#[test]
fn pipelined_commands_are_answered_one_turn_a_shard_poll() {
  const COMMANDS: usize = 64;
  let daemon = daemon(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0));
  let port = daemon.vcs_svn_port().expect("the listener started");
  enrolled(&daemon);
  let refusal_at = NO_ACCESS_SERVER
    .windows(REFUSAL_START.len())
    .position(|window| window == REFUSAL_START)
    .expect("the recorded server refused");
  let greeting = replaced(&NO_ACCESS_SERVER[..refusal_at], SVNDIFF1, "");
  let name = name_on("svn-pipelined-", OTHER);
  create_svn(&daemon, OTHER, &name);
  let mut signed = sign_in(port, &greeting, &format!("svn://127.0.0.1:{port}/{name}"));
  let answers = asked(
    &mut signed.stream,
    &b"( get-latest-rev ( ) ) ".repeat(COMMANDS),
    2 * COMMANDS,
  );
  assert_eq!(
    answers,
    "( success ( ( ) 0: ) ) ( success ( 0 ) ) ".repeat(COMMANDS)
  );
  let unyielded: Vec<u64> = [LISTENER, OTHER]
    .into_iter()
    .map(|partition| {
      crate::daemon::observe_at(&daemon, partition, |state| {
        state
          .refusals
          .get(super::TURNS_UNYIELDED)
          .copied()
          .unwrap_or(0)
      })
    })
    .collect();
  assert_eq!(unyielded, vec![0, 0]);
}

/// A-702 part 2 (`fs-wrap.c` `svn_repos_fs_change_rev_prop4`; `hooks.c` `svn_repos__hooks_pre_revprop_change`,
/// `_post_revprop_change`). Do: commit a file with log `file`; configure a pre-revprop-change hook that records its
/// arguments and standard input and allows, and a post-revprop-change hook that records its standard input; change
/// revision 1's `svn:log` to `new`, then ask `rev-proplist` of revision 1. Expect: the change's success; pre-revprop-change
/// given the repository, `1`, the user, `svn:log` and `M`, the new value on its standard input; post-revprop-change the
/// old value `file` on its; and the revision's `svn:log` now `new`. Before A-702 part 2 every change was refused 165006.
#[cfg(unix)]
#[test]
fn a_revision_property_change_runs_its_hooks_and_lands_as_svnserve_does() {
  use slates_ipc::protocol::HookPointName;
  let dir = crate::git_hooks::tests::ScratchDir::new();
  let at = dir.path.display().to_string();
  let pre = hook_script(
    &dir.path,
    "pre-revprop-change",
    &format!(
      "for arg in \"$@\"; do echo \"$arg\"; done > {at}/pre.args\n/bin/cat > {at}/pre.stdin"
    ),
  );
  let post = hook_script(
    &dir.path,
    "post-revprop-change",
    &format!("/bin/cat > {at}/post.stdin"),
  );
  let answers = under_policy(
    Some(vec![
      (HookPointName::SvnPreRevpropChange, pre),
      (HookPointName::SvnPostRevpropChange, post),
    ]),
    &[
      (b"( change-rev-prop ( 1 7:svn:log 3:new ) ) ", false),
      (b"( rev-proplist ( 1 ) ) ", false),
    ],
  );
  let pre_args = recorded_lines(&dir.path, "pre.args");
  assert_eq!(
    (
      answers.first().map(String::as_str) == Some("( success ( ( ) 0: ) ) ( success ( ) ) "),
      pre_args.get(1).map(String::as_str),
      pre_args.get(3).map(String::as_str),
      pre_args.get(4).map(String::as_str),
      recorded_lines(&dir.path, "pre.stdin"),
      recorded_lines(&dir.path, "post.stdin"),
    ),
    (
      true,
      Some("1"),
      Some("svn:log"),
      Some("M"),
      vec!["new".to_owned()],
      vec!["file".to_owned()],
    ),
    "pre-revprop-change {pre_args:?}"
  );
  let listed = answers.get(1).expect("the list's answer");
  assert!(listed.contains("7:svn:log 3:new"), "{listed}");
}

/// A-702 part 2 (`hooks.c` `svn_repos__hooks_pre_revprop_change`). Do: commit a file with no hook configured, change
/// revision 1's `svn:log` with `change-rev-prop`, then with `change-rev-prop2` as stock svn 1.15.0 sends it, then list
/// the revision's properties; with the repository's policy set with no hook, and never set. Expect: each change answered
/// one trivial authentication request, then 165006 "Repository has not been enabled to accept revision propchanges", as
/// `svnserve` answers a repository with no pre-revprop-change hook; and `svn:log` still `file`. Red before the listener
/// cleared a turn's written output: the request written twice, which stock svn read as the change's success.
#[cfg(unix)]
#[test]
fn a_revision_property_change_without_a_hook_is_refused_as_svnserve_refuses_it() {
  // Stock svn 1.15.0 sends `change-rev-prop2` with `dont-care` to a server announcing `atomic-revprops` (w-interop's run,
  // 2026-11-20: `svn propset --revprop` was answered "property set"), older clients `change-rev-prop`.
  let verbs: [(&[u8], bool); 3] = [
    (b"( change-rev-prop ( 1 7:svn:log 3:new ) ) ", false),
    (
      b"( change-rev-prop2 ( 1 7:svn:log ( 7:changed ) ( true ) ) ) ",
      false,
    ),
    (b"( rev-proplist ( 1 ) ) ", false),
  ];
  for hooks in [Some(Vec::new()), None] {
    let answered = under_policy(hooks.clone(), &verbs).concat();
    // One trivial authentication request a change: stock svn reads a second as the change's answer, a success
    // (w-interop's run: "property 'svn:log' set on repository revision 1", exit 0).
    let doubled = "( success ( ( ) 0: ) ) ( success ( ( ) 0: ) ) ";
    assert_eq!(
      (
        answered.matches("( failure ( ( 165006 ").count(),
        answered.contains(doubled),
        answered.contains("7:svn:log 4:file"),
      ),
      (2, false, true),
      "policy set: {}; {answered}",
      hooks.is_some()
    );
  }
}

/// A-730 (`hooks.c` `svn_repos__hooks_pre_unlock`, `_post_unlock`). Do: commit a file; configure a pre-lock hook that
/// gives the lock's token, and pre-unlock and post-unlock hooks that record their arguments and standard input and
/// accept; lock the file, then unlock it with the token. Expect: the unlock answered as `svnserve` answers one path
/// unlocked (the path, `done`, success); pre-unlock given the repository, the path, the user, the token and `0` (not
/// broken); post-unlock the repository and the user, the unlocked path on its standard input.
#[cfg(unix)]
#[test]
fn an_unlock_runs_pre_unlock_and_post_unlock_as_svnserve_does() {
  use slates_ipc::protocol::HookPointName;
  let dir = crate::git_hooks::tests::ScratchDir::new();
  let at = dir.path.display().to_string();
  let token = hook_script(&dir.path, "pre-lock", "printf 'opaquelocktoken:to-unlock'");
  let pre = hook_script(
    &dir.path,
    "pre-unlock",
    &format!("for arg in \"$@\"; do echo \"$arg\"; done > {at}/pre.args"),
  );
  let post = hook_script(
    &dir.path,
    "post-unlock",
    &format!(
      "for arg in \"$@\"; do echo \"$arg\"; done > {at}/post.args\n/bin/cat > {at}/post.stdin"
    ),
  );
  let answers = lock_under_hooks(
    vec![
      (HookPointName::SvnPreLock, token),
      (HookPointName::SvnPreUnlock, pre),
      (HookPointName::SvnPostUnlock, post),
    ],
    &[
      b"( lock-many ( ( 4:held ) false ( ( 7:new.txt ( ) ) ) ) ) ",
      b"( unlock-many ( false ( ( 7:new.txt ( 25:opaquelocktoken:to-unlock ) ) ) ) ) ",
    ],
  );
  let unlocked = answers.get(1).expect("the unlock's answer");
  let pre_args = recorded_lines(&dir.path, "pre.args");
  assert_eq!(
    (
      pre_args.get(1).map(String::as_str),
      pre_args.get(3).map(String::as_str),
      pre_args.get(4).map(String::as_str),
      pre_args.len(),
      recorded_lines(&dir.path, "post.args").len(),
      recorded_lines(&dir.path, "post.stdin"),
    ),
    (
      Some("/new.txt"),
      Some("opaquelocktoken:to-unlock"),
      Some("0"),
      5,
      2,
      vec!["/new.txt".to_owned()]
    ),
    "pre-unlock {pre_args:?}"
  );
  assert!(
    unlocked.contains("( success ( 7:new.txt ) )") && unlocked.ends_with("done ( success ( ) ) "),
    "{unlocked}"
  );
}
