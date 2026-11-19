//! Tests of the daemon's HTTP edge (A-625 part 3): a daemon in-process starts it from its configuration, and a rustls
//! client over loopback TLS asks for a repository another shard's partition holds, with each kind of credential; and
//! pushes to it the bytes stock git sent (A-654 part 3).

use std::io::{Read, Write};
use std::net::TcpStream as ClientStream;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use slates_db::HostId;
use slates_db::catalog::Principal;
use slates_git::http::{self, Service, Version};
use slates_git::object_name::{ObjectFormat as GitFormat, ObjectId};
use slates_git::pktline::{self, Packet};
use slates_git::receive::{Advertised, write_advertisement};
use slates_ipc::protocol::{
  CredentialMaterial, ObjectFormat, PolicyEffect, PolicyNames, PolicyRight, PolicyRule, PolicyWho,
  ReplyBody, RequestBody, VcsTool,
};
use slates_rt::tcp::{Ipv4Addr, SocketAddrV4};

use crate::config::{EdgeIdentity, VcsHttpConfig};
use crate::daemon::{Daemon, SegmentSource};
use crate::vcs_http::{Counts, EdgeState, here};

/// Shape: the daemon's shards: two, so a repository can live on a shard other than the listener's.
const SHARDS: u16 = 2;
/// Shape: the partition of the listener's shard, the control shard.
const LISTENER: usize = 0;
/// Shape: the partition the tests' repository lives on.
const REPOSITORY: usize = 1;
/// Shape: the uid of the repository's owner.
const OWNER: u32 = 1234;
/// Shape: the uid of a principal no rule of the repository names.
const STRANGER: u32 = 4321;
/// Shape: the owner's token.
const OWNER_TOKEN: &str = "slates_owner";
/// Shape: the stranger's token.
const STRANGER_TOKEN: &str = "slates_stranger";
/// Shape: an owner's token that expired a nanosecond after the Unix epoch.
const EXPIRED_TOKEN: &str = "slates_expired";
/// Shape: when [`EXPIRED_TOKEN`] expired, in nanoseconds since the Unix epoch.
const LONG_AGO_NS: u64 = 1;
/// Shape: the owner's token as Basic's password for the user `git`: `git:slates_owner` in base64 (RFC 7617 §2).
const OWNER_BASIC: &str = "Authorization: Basic Z2l0OnNsYXRlc19vd25lcg==";
/// Shape: how long a client waits for an answer: the daemon's liveness budget.
const PATIENCE_NS: u64 = crate::daemon::LIVENESS_BUDGET_NS;

/// The repository's owner, the principal of the human's channel that created it.
fn owner() -> Principal {
  Principal::Uid {
    host: HostId(0),
    uid: OWNER,
  }
}

/// A self-signed identity for `localhost`, as the operator's command hands one in, and its certificate for a client to
/// trust.
fn identity() -> (EdgeIdentity, CertificateDer<'static>) {
  let key = rcgen::KeyPair::generate().expect("a key");
  let certificate = rcgen::CertificateParams::new(vec!["localhost".to_owned()])
    .expect("parameters")
    .self_signed(&key)
    .expect("a self-signed certificate");
  let der = certificate.der().clone();
  (
    EdgeIdentity::new(vec![der.to_vec()], key.serialize_der()),
    der,
  )
}

/// A daemon of [`SHARDS`] shards whose configuration asks for an edge on loopback presenting `identity`.
fn daemon(identity: EdgeIdentity) -> Daemon {
  static NEXT: AtomicU32 = AtomicU32::new(0);
  let profile = crate::daemon::test_profile();
  let instance = format!(
    "vcs-http-{}-{}",
    std::process::id(),
    NEXT.fetch_add(1, Ordering::Relaxed)
  );
  let mut config = crate::DaemonConfig::derive(&profile, &instance, Some(SHARDS));
  config.vcs_http = Some(VcsHttpConfig {
    address: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
    identity: Some(identity),
  });
  let daemon = Daemon::start(&profile, config, SegmentSource::Create { name: instance })
    .expect("the daemon starts");
  daemon
    .bootstrap(true)
    .expect("the daemon creates its initial group");
  daemon
}

/// A git repository's name that routes to [`REPOSITORY`]'s partition, not the listener's.
fn name_on_the_repositorys_partition() -> String {
  (0u32..)
    .map(|serial| format!("moved-{serial}.git"))
    .find(|name| usize::from(crate::verbs::owner_of_name(name, usize::from(SHARDS))) == REPOSITORY)
    .expect("a name routes there")
}

/// Creates the git repository `name` on its partition, owned by [`owner`].
fn create(daemon: &Daemon, name: &str) {
  let name = name.to_owned();
  let created = crate::daemon::observe_at(daemon, REPOSITORY, move |state| {
    crate::verbs::dispatch(
      state,
      1,
      &owner(),
      RequestBody::CreateRepository {
        name: name.clone(),
        tool: VcsTool::Git,
        format: ObjectFormat::Sha1,
        default_branch: "main".to_owned(),
        green: None,
      },
    )
  });
  assert!(!matches!(created, ReplyBody::Refused { .. }), "{created:?}");
}

/// Enrolls `token` for uid `uid` until `expires_ns`, on the partition its hash routes to, as the human surface does.
fn enroll(daemon: &Daemon, token: &str, uid: u32, expires_ns: Option<u64>) {
  let id = crate::credentials::token_id(token.as_bytes());
  let partition = usize::from(crate::credentials::owner_of(&id, usize::from(SHARDS)));
  let reply = crate::daemon::observe_at(daemon, partition, move |state| {
    let principal = slates_ipc::protocol::Principal::Uid { host: 0, uid };
    let material = CredentialMaterial::HttpToken { id };
    let proof =
      crate::credentials::enroll_proof(&state.issuer_secret, &principal, &material, expires_ns);
    crate::verbs::dispatch(
      state,
      1,
      &owner(),
      RequestBody::EnrollCredential {
        principal,
        material,
        expires_ns,
        proof,
      },
    )
  });
  assert_eq!(reply, ReplyBody::CredentialEnrolled { id });
}

/// A rustls client's connection to the edge on `port`, trusting `certificate`.
#[allow(clippy::disallowed_types)] // rustls takes its configuration and provider as `Arc` by signature (D-8's test-harness exception).
fn tls_client(
  port: u16,
  certificate: &CertificateDer<'static>,
) -> StreamOwned<ClientConnection, ClientStream> {
  let mut roots = RootCertStore::empty();
  roots
    .add(certificate.clone())
    .expect("the certificate trusted");
  let provider = std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider());
  let config = ClientConfig::builder_with_provider(provider)
    .with_safe_default_protocol_versions()
    .expect("TLS's versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
  let connection = ClientConnection::new(
    std::sync::Arc::new(config),
    ServerName::try_from("localhost").expect("a server name"),
  )
  .expect("a client connection");
  let socket = ClientStream::connect(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
    .expect("the edge's listener takes the connection");
  socket
    .set_read_timeout(Some(Duration::from_nanos(PATIENCE_NS)))
    .expect("a read timeout");
  StreamOwned::new(connection, socket)
}

/// The status of the answer to `GET target` with `fields`, on a new TLS connection to the edge on `port`.
fn status_of(
  port: u16,
  certificate: &CertificateDer<'static>,
  target: &str,
  fields: &[&str],
) -> u16 {
  let mut stream = tls_client(port, certificate);
  let mut request = format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n");
  for field in fields {
    request.push_str(field);
    request.push_str("\r\n");
  }
  request.push_str("\r\n");
  stream
    .write_all(request.as_bytes())
    .expect("the request is sent");
  let mut head = Vec::new();
  let mut byte = [0u8; 1];
  while !head.ends_with(b"\r\n\r\n") {
    match stream.read(&mut byte) {
      Ok(1) => head.push(byte[0]),
      other => panic!("the answer's head ended early ({other:?}): {head:?}"),
    }
  }
  let line = String::from_utf8_lossy(&head).into_owned();
  line
    .split(' ')
    .nth(1)
    .and_then(|code| code.parse().ok())
    .unwrap_or_else(|| panic!("no status line: {line:?}"))
}

/// The edge's counts on partition `partition`'s shard.
fn counts_on(daemon: &Daemon, partition: usize) -> Counts {
  crate::daemon::observe_at(daemon, partition, |_state| {
    here().and_then(|edge| edge.with(EdgeState::counts))
  })
  .expect("the shard keeps an edge")
}

/// What partition `partition`'s shard counted under `kind`.
fn counted_on(daemon: &Daemon, partition: usize, kind: &'static str) -> u64 {
  crate::daemon::observe_at(daemon, partition, move |state| {
    state.refusals.get(kind).copied().unwrap_or(0)
  })
}

/// A-625 part 3 (main's rulings, 2026-11-19: the daemon starts the edge; a connection moves to its repository's shard;
/// credentials from the one record). Do: start a two-shard daemon whose configuration asks for an edge on loopback with
/// a self-signed identity; create a git repository on the second shard's partition; enroll tokens for its owner, for a
/// principal no rule names, and an owner's that expired long ago; then, through a rustls client over TLS, ask for the
/// repository's upload-pack advertisement, each on a new connection: with the owner's bearer token, with the owner's
/// token as Basic's password, with no credentials, with the expired token, with the stranger's. Expect: the owner's two
/// answered `503` (proven, moved to the repository's shard, found, admitted by its policy; upload-pack's store is not
/// held until A-662); no credentials and the expired token `401`, each counted under its kind on the listener's shard,
/// neither moved; the stranger moved and refused `403` there, counted; the listener's shard counting three moves and the
/// repository's three arrivals, so every proven request was decided where its repository lives.
#[test]
fn a_daemons_edge_moves_a_proven_request_to_its_repositorys_shard_and_refuses_the_rest_by_kind() {
  let (identity, certificate) = identity();
  let daemon = daemon(identity);
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = name_on_the_repositorys_partition();
  create(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  enroll(&daemon, STRANGER_TOKEN, STRANGER, None);
  enroll(&daemon, EXPIRED_TOKEN, OWNER, Some(LONG_AGO_NS));
  let target = format!("/{name}/info/refs?service=git-upload-pack");
  let owner_bearer = format!("Authorization: Bearer {OWNER_TOKEN}");
  let expired_bearer = format!("Authorization: Bearer {EXPIRED_TOKEN}");
  let stranger_bearer = format!("Authorization: Bearer {STRANGER_TOKEN}");
  let answers = [
    status_of(port, &certificate, &target, &[&owner_bearer]),
    status_of(port, &certificate, &target, &[OWNER_BASIC]),
    status_of(port, &certificate, &target, &[]),
    status_of(port, &certificate, &target, &[&expired_bearer]),
    status_of(port, &certificate, &target, &[&stranger_bearer]),
  ];
  let listener = (
    counts_on(&daemon, LISTENER).moved,
    counted_on(&daemon, LISTENER, "vcs_http.git.no_credentials"),
    counted_on(&daemon, LISTENER, "vcs_http.git.expired_credential"),
  );
  let repository = (
    counts_on(&daemon, REPOSITORY).arrived,
    counted_on(&daemon, REPOSITORY, "vcs_http.git.forbidden"),
    counted_on(&daemon, REPOSITORY, "vcs_http.git.unavailable"),
  );
  daemon.stop();
  assert_eq!(answers, [503, 503, 401, 401, 403]);
  assert_eq!(listener, (3, 1, 1), "moves, no credentials, expired");
  assert_eq!(repository, (3, 1, 2), "arrivals, forbidden, unavailable");
}

/// A-625 part 3 (§4.17.2 "Transports" item 1). Do: ask whether listeners an anchor might hand over listen where a
/// daemon's configuration says: at the same address and port; at the same address where the configuration asks for port
/// 0 (the system's pick, which the anchor made once for every daemon); at another port; at another address. Expect: the
/// first two taken and the last two refused, so a daemon adopts the anchor's listener only where it would have bound its
/// own, and never serves the edge somewhere the operator did not ask.
#[test]
fn a_held_listener_is_adopted_only_where_the_configuration_says() {
  let at = |octets: [u8; 4], port: u16| SocketAddrV4::new(Ipv4Addr::from(octets), port);
  assert!(super::listens_at(
    at([127, 0, 0, 1], 8080),
    at([127, 0, 0, 1], 8080)
  ));
  assert!(super::listens_at(
    at([127, 0, 0, 1], 51234),
    at([127, 0, 0, 1], 0)
  ));
  assert!(!super::listens_at(
    at([127, 0, 0, 1], 8081),
    at([127, 0, 0, 1], 8080)
  ));
  assert!(!super::listens_at(
    at([0, 0, 0, 0], 8080),
    at([127, 0, 0, 1], 8080)
  ));
}

/// One line of `stream`, its line feed included.
fn line_of(stream: &mut impl Read) -> Vec<u8> {
  let mut line = Vec::new();
  let mut byte = [0u8; 1];
  while !line.ends_with(b"\n") {
    match stream.read(&mut byte) {
      Ok(1) => line.push(byte[0]),
      other => panic!("the answer ended inside a line ({other:?}): {line:?}"),
    }
  }
  line
}

/// A chunked body read from `stream` to its last chunk, which has no trailer here (RFC 9112 §7.1).
fn chunked_body(stream: &mut impl Read) -> Vec<u8> {
  let mut body = Vec::new();
  loop {
    let size = String::from_utf8_lossy(&line_of(stream)).into_owned();
    let digits = size.split([';', '\r', '\n']).next().unwrap_or_default();
    let length = usize::from_str_radix(digits, 16).expect("a chunk's size");
    let mut chunk = vec![0u8; length];
    stream.read_exact(&mut chunk).expect("a chunk");
    body.extend(chunk);
    let end = line_of(stream);
    if end != b"\r\n" {
      panic!("a chunk ended in {end:?}");
    }
    if length == 0 {
      return body;
    }
  }
}

/// The body of an answer whose `head` was read from `stream`: chunked, framed by its length, or none.
fn body_of(stream: &mut impl Read, head: &str) -> Vec<u8> {
  let fields = head.to_ascii_lowercase();
  if fields.contains("transfer-encoding: chunked") {
    return chunked_body(stream);
  }
  let length = fields
    .lines()
    .find_map(|field| field.strip_prefix("content-length: "))
    .and_then(|value| value.trim().parse::<usize>().ok())
    .unwrap_or(0);
  let mut body = vec![0u8; length];
  stream.read_exact(&mut body).expect("the body");
  body
}

/// The status and body of the answer to `request`, sent whole on a new TLS connection to the edge on `port`.
fn exchange(port: u16, certificate: &CertificateDer<'static>, request: &[u8]) -> (u16, Vec<u8>) {
  let mut stream = tls_client(port, certificate);
  stream.write_all(request).expect("the request is sent");
  let mut head = Vec::new();
  while !head.ends_with(b"\r\n\r\n") {
    head.extend(line_of(&mut stream));
  }
  let head = String::from_utf8_lossy(&head).into_owned();
  let status = head
    .split(' ')
    .nth(1)
    .and_then(|code| code.parse().ok())
    .unwrap_or_else(|| panic!("no status line: {head:?}"));
  (status, body_of(&mut stream, &head))
}

/// Format: the content type of receive-pack's requests (gitprotocol-http), as stock git sends it.
const RECEIVE_REQUEST: &str = "Content-Type: application/x-git-receive-pack-request";
/// Format: stock git 2.50.1's push creating `main` in an empty repository, as `make-push-fixtures.sh` captured it
/// (`crates/git/fixtures/push`): its commands, then its pack.
const CREATE_REQUEST: &[u8] = include_bytes!("../../../../git/fixtures/push/create.request.bin");
/// Format: what `git receive-pack` answered that push: its advertisement of no ref, then its report.
const CREATE_RESPONSE: &[u8] = include_bytes!("../../../../git/fixtures/push/create.response.bin");
/// Format: the agent the captured server advertised.
const GITS_AGENT: &[u8] = b"git/2.50.1-Darwin";
/// Format: the commit that push made `main`.
const FIRST: &str = "b1fac94f2ed5e3cb5850e4b5f89d23fce635601e";
/// Format: the zero id, a delete's new value.
const ZERO: &str = "0000000000000000000000000000000000000000";
/// Format: the capabilities stock git 2.50.1 sends on a push's first command (the fixtures' requests).
const CAPABILITIES: &str =
  " report-status-v2 side-band-64k quiet object-format=sha1 agent=git/2.50.1-Darwin";
/// Format: the report line of a delete git refuses as `receive.denyDeletes` refuses one.
const DELETION_PROHIBITED: &[u8] = b"ng refs/heads/main deletion prohibited";

/// The object `hex` names, in SHA-1.
fn named(hex: &str) -> ObjectId {
  ObjectId::from_hex(GitFormat::Sha1, hex.as_bytes()).expect("an id")
}

/// receive-pack's advertisement of `refs` by a server advertising push options when `push_options`, under `agent`.
fn receive_advertisement(refs: &[(ObjectId, &[u8])], push_options: bool, agent: &[u8]) -> Vec<u8> {
  let advertised = Advertised {
    format: GitFormat::Sha1,
    atomic: true,
    ofs_delta: true,
    push_options,
    session_id: None,
    agent,
  };
  let mut out = Vec::new();
  write_advertisement(refs, &advertised, &mut out).expect("an advertisement");
  out
}

/// What the daemon's site answers receive-pack's advertisement with at protocol v0: the `# service=` prelude, then the
/// push driver's advertisement of `refs` (this server's agent, no push options).
fn served_advertisement(refs: &[(ObjectId, &[u8])]) -> Vec<u8> {
  let mut out = Vec::new();
  http::advertisement_prelude(Service::ReceivePack, Version::V0, &mut out).expect("a prelude");
  out.extend(receive_advertisement(
    refs,
    false,
    slates_git::upload::AGENT.as_bytes(),
  ));
  out
}

/// git's report to the `create` push: what `git receive-pack` wrote after its advertisement.
fn gits_create_report() -> &'static [u8] {
  let advertisement = receive_advertisement(&[], true, GITS_AGENT);
  CREATE_RESPONSE
    .strip_prefix(advertisement.as_slice())
    .expect("git's advertisement first")
}

/// A push request deleting `main` at [`FIRST`], as stock git sends one: its command, then a flush and no pack.
fn delete_of_main() -> Vec<u8> {
  let line = format!("{FIRST} {ZERO} refs/heads/main\0{CAPABILITIES}");
  let mut out = Vec::new();
  pktline::write(Packet::Data(line.as_bytes()), &mut out).expect("a command");
  pktline::write(Packet::Flush, &mut out).expect("a flush");
  out
}

/// The owner's bearer credential.
fn owner_bearer() -> String {
  format!("Authorization: Bearer {OWNER_TOKEN}")
}

/// receive-pack's advertisement of repository `name` as the edge answers the owner: its status and body.
fn advertised(edge: (u16, &CertificateDer<'static>), name: &str) -> (u16, Vec<u8>) {
  let (port, certificate) = edge;
  let request = format!(
    "GET /{name}/info/refs?service=git-receive-pack HTTP/1.1\r\nHost: localhost\r\n{}\r\n\r\n",
    owner_bearer()
  );
  exchange(port, certificate, request.as_bytes())
}

/// `request` pushed to repository `name` by its owner as stock git posts it, with its length: the answer's status and
/// body.
fn pushed(edge: (u16, &CertificateDer<'static>), name: &str, request: &[u8]) -> (u16, Vec<u8>) {
  let (port, certificate) = edge;
  let mut sent = format!(
    "POST /{name}/git-receive-pack HTTP/1.1\r\nHost: localhost\r\n{}\r\n{RECEIVE_REQUEST}\r\n\
     Content-Length: {}\r\n\r\n",
    owner_bearer(),
    request.len()
  )
  .into_bytes();
  sent.extend_from_slice(request);
  exchange(port, certificate, &sent)
}

/// Sets repository `name`'s policy, from its first version, to a rule denying everyone the deletion of `main`.
fn deny_deletion(daemon: &Daemon, name: &str) {
  let name = name.to_owned();
  let reply = crate::daemon::observe_at(daemon, REPOSITORY, move |state| {
    crate::verbs::dispatch(
      state,
      1,
      &owner(),
      RequestBody::SetRepositoryPolicy {
        repository: name.clone(),
        read: 0,
        rules: vec![PolicyRule {
          names: PolicyNames::Refs,
          pattern: "refs/heads/main".to_owned(),
          right: PolicyRight::Delete,
          effect: PolicyEffect::Deny,
          who: PolicyWho::Anyone,
        }],
        hooks: Vec::new(),
        hooks_env: Vec::new(),
        signers: Vec::new(),
      },
    )
  });
  assert_eq!(reply, ReplyBody::Acknowledged);
}

/// Asserts that `answer` is `200` with a report refusing `main`'s deletion as git refuses it.
fn assert_deletion_prohibited(answer: &(u16, Vec<u8>)) {
  let (status, report) = answer;
  assert_eq!(*status, 200, "{}", String::from_utf8_lossy(report));
  assert!(
    report
      .windows(DELETION_PROHIBITED.len())
      .any(|line| line == DELETION_PROHIBITED),
    "{}",
    String::from_utf8_lossy(report)
  );
}

/// A-654 part 3 (§4.17.2 "A push" and "Transports" item 1; gitprotocol-http's smart service; golden vectors: stock git
/// 2.50.1's `create` push, captured by `make-push-fixtures.sh`). Do: start a two-shard daemon whose edge serves loopback
/// TLS; create a git repository on the second shard's partition and enroll its owner's token; then, each on a new
/// connection with the owner's bearer token, ask for receive-pack's advertisement, POST the request stock git sent to
/// create `main`, ask for the advertisement again, set a policy denying `main`'s deletion, POST a delete of `main`, and
/// ask for the advertisement once more. Expect: the first advertisement `200`, the `# service=` prelude and then no ref
/// under this server's capabilities; the push `200`, its body the very report `git receive-pack` wrote; the second
/// advertisement `main` at the pushed commit; the delete `200`, refused `deletion prohibited` in git's words; `main`
/// still advertised at the pushed commit.
#[test]
fn a_stock_git_push_over_the_daemons_edge_lands_through_the_push_driver() {
  let (identity, certificate) = identity();
  let daemon = daemon(identity);
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = name_on_the_repositorys_partition();
  create(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let edge = (port, &certificate);
  let before = advertised(edge, &name);
  let created = pushed(edge, &name, CREATE_REQUEST);
  let after = advertised(edge, &name);
  deny_deletion(&daemon, &name);
  let deleted = pushed(edge, &name, &delete_of_main());
  let last = advertised(edge, &name);
  daemon.stop();
  let main = [(named(FIRST), b"refs/heads/main".as_slice())];
  assert_eq!(before, (200, served_advertisement(&[])));
  assert_eq!(created, (200, gits_create_report().to_vec()));
  assert_eq!(after, (200, served_advertisement(&main)));
  assert_deletion_prohibited(&deleted);
  assert_eq!(last, after);
}
