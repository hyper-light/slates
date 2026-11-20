//! Tests of the daemon's HTTP edge (A-625 part 3): a daemon in-process starts it from its configuration, and a rustls
//! client over loopback TLS asks for a repository another shard's partition holds, with each kind of credential;
//! pushes to it the bytes stock git sent (A-654 part 3); and fetches back what it pushed (A-682).

use std::io::{Read, Write};
use std::net::TcpStream as ClientStream;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use slates_db::HostId;
use slates_db::catalog::Principal;
use slates_git::Admission;
use slates_git::http::{self, Service, Version};
use slates_git::object_name::{ObjectFormat as GitFormat, ObjectId};
use slates_git::pktline::{self, Packet};
use slates_git::receive::{Advertised, write_advertisement};
use slates_git::unpack::{self, NoIndex};
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
  daemon_named(Some(identity)).0
}

/// A daemon of [`SHARDS`] shards whose configuration asks for an edge on loopback presenting `identity`, or serving
/// cleartext without one, and its instance's name, which its edge's realm names (`slates <instance>`).
fn daemon_named(identity: Option<EdgeIdentity>) -> (Daemon, String) {
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
    identity,
  });
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: instance.clone(),
    },
  )
  .expect("the daemon starts");
  daemon
    .bootstrap(true)
    .expect("the daemon creates its initial group");
  (daemon, instance)
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
/// credentials from the one record), and A-682 (upload-pack served from the store the shard holds). Do: start a
/// two-shard daemon whose configuration asks for an edge on loopback with a self-signed identity; create a git
/// repository on the second shard's partition; enroll tokens for its owner, for a principal no rule names, and an
/// owner's that expired long ago; then, through a rustls client over TLS, ask for the repository's upload-pack
/// advertisement, each on a new connection: with the owner's bearer token, with the owner's token as Basic's password,
/// with no credentials, with the expired token, with the stranger's. Expect: the owner's two answered `200` (proven,
/// moved to the repository's shard, found, admitted by its policy, and served from the store that shard holds); no
/// credentials and the expired token `401`, each counted under its kind on the listener's shard, neither moved; the
/// stranger moved and refused `403` there, counted; nothing counted unavailable; the listener's shard counting three
/// moves and the repository's three arrivals, so every proven request was decided where its repository lives.
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
  assert_eq!(answers, [200, 200, 401, 401, 403]);
  assert_eq!(listener, (3, 1, 1), "moves, no credentials, expired");
  assert_eq!(repository, (3, 1, 0), "arrivals, forbidden, unavailable");
}

/// Shape: a v2 `ls-refs` request asking for `HEAD` with its symref's target, listed even unborn: the command, the
/// delimiter, `symrefs`, `unborn` and `ref-prefix HEAD`, then the flush [gitprotocol-v2 "ls-refs"].
const LS_REFS_HEAD: &[u8] = b"0014command=ls-refs\n0001000csymrefs\n000bunborn\n\
  0014ref-prefix HEAD\n0000";

/// Shape: the answer to [`LS_REFS_HEAD`] for a repository whose default branch `main` has no commit yet: `HEAD` unborn,
/// naming its branch, then the flush [gitprotocol-v2: `obj-id-or-unborn SP refname *(SP ref-attribute)`].
const UNBORN_MAIN: &[u8] = b"002eunborn HEAD symref-target:refs/heads/main\n0000";

/// A-682 (A-625 part 3's upload-pack served from the store A-662 holds on the repository's shard). Do: start a two-shard
/// daemon with its edge; create a git repository whose default branch is `main` on the second shard's partition, and
/// enroll its owner's token; then, as the owner over TLS, ask for upload-pack's advertisement at protocol v2, and POST
/// an `ls-refs` request asking for `HEAD` with `symrefs` and `unborn`. Expect: both answered `200`, moved to the
/// repository's shard and served there from its store: the advertisement v2's capabilities, `version 2` first and
/// `ls-refs=unborn` among them; the `ls-refs` answer `HEAD` unborn naming `refs/heads/main`, the default branch the
/// repository's record names; and nothing counted unavailable on the repository's shard. Before A-682 the daemon's site
/// lent no store, and both were `503`.
#[test]
fn a_daemons_edge_serves_upload_pack_from_the_store_its_repositorys_shard_holds() {
  let (identity, certificate) = identity();
  let daemon = daemon(identity);
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = name_on_the_repositorys_partition();
  create(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let fields = format!(
    "Host: localhost\r\nAuthorization: Bearer {OWNER_TOKEN}\r\nGit-Protocol: version=2\r\n"
  );
  let advertise = format!("GET /{name}/info/refs?service=git-upload-pack HTTP/1.1\r\n{fields}\r\n");
  let advertise = advertise.into_bytes();
  let mut ls_refs = format!(
    "POST /{name}/git-upload-pack HTTP/1.1\r\n{fields}\
     Content-Type: application/x-git-upload-pack-request\r\nContent-Length: {}\r\n\r\n",
    LS_REFS_HEAD.len()
  )
  .into_bytes();
  ls_refs.extend_from_slice(LS_REFS_HEAD);
  let advertisement = exchange(port, &certificate, &advertise);
  let listed = exchange(port, &certificate, &ls_refs);
  let unavailable = counted_on(&daemon, REPOSITORY, "vcs_http.git.unavailable");
  daemon.stop();
  let (status, capabilities) = advertisement;
  assert_eq!(status, 200, "{}", String::from_utf8_lossy(&capabilities));
  assert!(
    capabilities.starts_with(b"000eversion 2\n"),
    "{capabilities:?}"
  );
  assert!(
    capabilities
      .windows(b"ls-refs=unborn".len())
      .any(|window| window == b"ls-refs=unborn"),
    "{capabilities:?}"
  );
  assert_eq!(listed, (200, UNBORN_MAIN.to_vec()));
  assert_eq!(unavailable, 0);
}

/// A-625 part 3 (§4.17.2 "Transports" item 1). Do: ask whether listeners an anchor might hand over listen where a
/// daemon's configuration says: at the same address and port; at the same address where the configuration asks for port
/// 0 (the system's pick, which the anchor made once for every daemon); at another port; at another address. Expect: the
/// first two taken and the last two refused, so a daemon adopts the anchor's listener only where it would have bound its
/// own, and never serves the edge somewhere the operator did not ask.
#[test]
fn a_held_listener_is_adopted_only_where_the_configuration_says() {
  let at = |octets: [u8; 4], port: u16| SocketAddrV4::new(Ipv4Addr::from(octets), port);
  assert!(crate::daemon::listens_at(
    at([127, 0, 0, 1], 8080),
    at([127, 0, 0, 1], 8080)
  ));
  assert!(crate::daemon::listens_at(
    at([127, 0, 0, 1], 51234),
    at([127, 0, 0, 1], 0)
  ));
  assert!(!crate::daemon::listens_at(
    at([127, 0, 0, 1], 8081),
    at([127, 0, 0, 1], 8080)
  ));
  assert!(!crate::daemon::listens_at(
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

/// Format: the most bytes one stored DEFLATE block holds (RFC 1951 §3.2.4: `LEN` is two bytes).
const STORED_BLOCK: usize = 0xffff;

/// Format: Adler-32's modulus, the largest prime below 2^16 (RFC 1950 §8.2).
const ADLER_MODULUS: u32 = 65_521;

/// `data` as a zlib stream of stored blocks (RFC 1950 §2.2, RFC 1951 §3.2.4): what git's reader inflates to `data`,
/// a few bytes longer than it, so a pack of it is longer than its content.
fn stored_zlib(data: &[u8]) -> Vec<u8> {
  let mut out = vec![0x78, 0x01];
  let mut blocks = data.chunks(STORED_BLOCK).peekable();
  while let Some(block) = blocks.next() {
    out.push(u8::from(blocks.peek().is_none()));
    let len = u16::try_from(block.len()).expect("a stored block's length");
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&(!len).to_le_bytes());
    out.extend_from_slice(block);
  }
  let (low, high) = data.iter().fold((1u32, 0u32), |(low, high), byte| {
    let low = (low + u32::from(*byte)) % ADLER_MODULUS;
    (low, (high + low) % ADLER_MODULUS)
  });
  out.extend_from_slice(&((high << 16) | low).to_be_bytes());
  out
}

/// A pack of `objects`, each stored whole in stored blocks.
fn stored_pack(objects: &[(slates_git::object_name::Kind, &[u8])]) -> Vec<u8> {
  use slates_git::entry::{self, Header, Representation};
  let mut out = Vec::new();
  let count = u32::try_from(objects.len()).expect("a count");
  let mut writer = slates_git::pack::Writer::new(GitFormat::Sha1, count, &mut out);
  let mut at = entry::PACK_HEADER_LEN;
  for (kind, content) in objects {
    let header = Header {
      representation: Representation::Whole(*kind),
      size: u64::try_from(content.len()).expect("a size"),
    };
    let zlib = stored_zlib(content);
    entry::write(header, at, &mut writer).expect("the entry's header");
    slates_git::out::Out::put(&mut writer, &zlib);
    let len = entry::written_len(header, at).expect("the header's length");
    at += len + u64::try_from(zlib.len()).expect("a length");
  }
  writer.finish().expect("the trailer");
  out
}

/// The name git gives an object of `kind` holding `content`.
fn git_name(kind: slates_git::object_name::Kind, content: &[u8]) -> ObjectId {
  slates_git::object_name::name_of(GitFormat::Sha1, kind, content).expect("named")
}

/// Stock git's request creating `main` at a root commit whose one file is `bytes` long, and that commit: the file's
/// bytes counted out by a 64-bit LCG (Knuth's MMIX constants), so no two windows of it repeat.
fn large_create(bytes: usize) -> (Vec<u8>, ObjectId) {
  use slates_git::object_name::Kind;
  let mut state = 0x9e37_79b9_7f4a_7c15u64;
  let blob: Vec<u8> = (0..bytes)
    .map(|_| {
      state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
      state.to_be_bytes()[0]
    })
    .collect();
  let blob_id = git_name(Kind::Blob, &blob);
  let mut tree = b"100644 large\0".to_vec();
  tree.extend_from_slice(blob_id.as_bytes());
  let tree_id = git_name(Kind::Tree, &tree);
  let commit = format!(
    "tree {tree_id}\nauthor A <a@example.com> 1700000000 +0000\n\
     committer A <a@example.com> 1700000000 +0000\n\nlarge\n"
  )
  .into_bytes();
  let commit_id = git_name(Kind::Commit, &commit);
  let pack = stored_pack(&[
    (Kind::Commit, &commit),
    (Kind::Tree, &tree),
    (Kind::Blob, &blob),
  ]);
  let mut request = Vec::new();
  let line = format!("{ZERO} {commit_id} refs/heads/main\0 report-status");
  pktline::write(Packet::Data(line.as_bytes()), &mut request).expect("the command");
  pktline::write(Packet::Flush, &mut request).expect("the flush");
  request.extend_from_slice(&pack);
  (request, commit_id)
}

/// A-704 (found by use, 2026-11-19: stock git's push of git/git, a 324 MiB pack, answered `413`, and its
/// `refs/tags/v0.99` alone `unpack eof before pack header was fully read`; receive.maxInputSize is git's own bound, and
/// it is unset by default). Do: start a two-shard daemon whose edge serves loopback TLS, each request admitted one ring
/// region (the edge's `Bounds::room`); create a git repository and push, as stock git posts it, a create of `main` at a
/// commit whose one file is twice that room, so the pack and what reading it builds are each past it; then ask for the
/// advertisement. Expect: the push `200`, its report `unpack ok` and `ok refs/heads/main`; `main` advertised at the
/// commit: a push is bounded by what its repository's shard can hold, never by one request's room.
#[test]
fn a_push_whose_pack_is_larger_than_one_ring_region_lands_through_the_daemons_edge() {
  let (identity, certificate) = identity();
  let (daemon, instance) = daemon_named(Some(identity));
  let room = super::bounds_of(&crate::DaemonConfig::derive(
    &crate::daemon::test_profile(),
    &instance,
    Some(SHARDS),
  ))
  .room;
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = name_on_the_repositorys_partition();
  create(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let edge = (port, &certificate);
  let (request, commit) = large_create(usize::try_from(room * 2).expect("a size"));
  let created = pushed(edge, &name, &request);
  let after = advertised(edge, &name);
  daemon.stop();
  let mut report = Vec::new();
  for line in [b"unpack ok\n".as_slice(), b"ok refs/heads/main\n"] {
    pktline::write(Packet::Data(line), &mut report).expect("a line");
  }
  pktline::write(Packet::Flush, &mut report).expect("the flush");
  let main = [(commit, b"refs/heads/main".as_slice())];
  assert_eq!(
    (created.0, String::from_utf8_lossy(&created.1)),
    (200, String::from_utf8_lossy(&report))
  );
  assert_eq!(after, (200, served_advertisement(&main)));
}

/// Shape: the room a pack is read whole within here: far past the `create` push's three small objects.
const PACK_ROOM: u64 = 1 << 20;

/// The data packets of `bytes` from `at` to the first flush, each whole, and where that flush ends; bytes that end
/// before one fail the test.
fn packets_from(bytes: &[u8], mut at: usize) -> (Vec<Vec<u8>>, usize) {
  let mut lines = Vec::new();
  loop {
    let rest = bytes.get(at..).unwrap_or_default();
    match pktline::read(rest, u64::try_from(at).unwrap_or(u64::MAX)) {
      Ok(Some((Packet::Data(data), length))) => {
        lines.push(data.to_vec());
        at += length;
      }
      Ok(Some((Packet::Flush, length))) => return (lines, at + length),
      other => panic!("no flush ended the packets ({other:?}): {bytes:?}"),
    }
  }
}

/// The ids of the objects `pack` holds, read whole as git's index-pack reads one, in name order.
fn ids_in(pack: &[u8]) -> Vec<String> {
  let mut admission = Admission::new(PACK_ROOM);
  let unpacked = unpack::read_whole(pack, GitFormat::Sha1, &NoIndex, &mut admission);
  let unpacked = unpacked.expect("a whole pack");
  let mut ids: Vec<String> = unpacked
    .objects
    .iter()
    .map(|object| object.id.to_string())
    .collect();
  ids.sort();
  ids
}

/// The pack a protocol v2 `fetch` answer carries: its `packfile` section's band-1 data, joined.
fn fetched_pack(answer: &[u8]) -> Vec<u8> {
  let (lines, _) = packets_from(answer, 0);
  let (section, packets) = lines.split_first().expect("a section");
  assert_eq!(section.as_slice(), b"packfile\n", "{answer:?}");
  let mut pack = Vec::new();
  for line in packets {
    match line.split_first() {
      Some((1, data)) => pack.extend_from_slice(data),
      other => panic!("not the pack's band: {other:?}"),
    }
  }
  pack
}

/// upload-pack's answer to `request`, POSTed at protocol v2 by the owner of repository `name`: its status and body.
fn uploaded(edge: (u16, &CertificateDer<'static>), name: &str, request: &[u8]) -> (u16, Vec<u8>) {
  let (port, certificate) = edge;
  let mut sent = format!(
    "POST /{name}/git-upload-pack HTTP/1.1\r\nHost: localhost\r\n{}\r\nGit-Protocol: version=2\r\n\
     Content-Type: application/x-git-upload-pack-request\r\nContent-Length: {}\r\n\r\n",
    owner_bearer(),
    request.len()
  )
  .into_bytes();
  sent.extend_from_slice(request);
  exchange(port, certificate, &sent)
}

/// A v2 `fetch` of [`FIRST`] with `done` and no progress, as `git clone` asks for a branch's commit.
fn fetch_of_first() -> Vec<u8> {
  let mut request = Vec::new();
  let lines = [
    "command=fetch\n".to_owned(),
    format!("want {FIRST}\n"),
    "no-progress\n".to_owned(),
    "done\n".to_owned(),
  ];
  let (command, arguments) = lines.split_first().expect("a command");
  pktline::write(Packet::Data(command.as_bytes()), &mut request).expect("the command");
  pktline::write(Packet::Delim, &mut request).expect("the delimiter");
  for argument in arguments {
    pktline::write(Packet::Data(argument.as_bytes()), &mut request).expect("an argument");
  }
  pktline::write(Packet::Flush, &mut request).expect("the flush");
  request
}

/// A-682 with A-654 part 3 (§4.17.2 "A fetch" and "A push" over "Transports" item 1; golden vectors: stock git 2.50.1's
/// `create` push). Do: start a two-shard daemon whose edge serves loopback TLS; create a git repository on the second
/// shard's partition and enroll its owner's token; POST the request stock git sent to create `main` to receive-pack;
/// then, as the owner at protocol v2 on the same edge, POST upload-pack an `ls-refs` asking for `HEAD` with `symrefs`
/// and `unborn`, and a `fetch` of the pushed commit with `done`. Expect: the push `200`; `HEAD` listed at the pushed
/// commit, naming `refs/heads/main`; the fetch `200`, its packfile section a whole pack holding exactly the objects
/// stock git's push sent (the commit, its tree and its blob), so what one service admitted the other serves.
#[test]
fn a_branch_stock_git_pushed_over_the_daemons_edge_is_listed_and_fetched_whole_over_it() {
  let (identity, certificate) = identity();
  let daemon = daemon(identity);
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = name_on_the_repositorys_partition();
  create(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let edge = (port, &certificate);
  let created = pushed(edge, &name, CREATE_REQUEST);
  let listed = uploaded(edge, &name, LS_REFS_HEAD);
  let fetched = uploaded(edge, &name, &fetch_of_first());
  daemon.stop();
  let mut head = Vec::new();
  let line = format!("{FIRST} HEAD symref-target:refs/heads/main\n");
  pktline::write(Packet::Data(line.as_bytes()), &mut head).expect("the line");
  pktline::write(Packet::Flush, &mut head).expect("the flush");
  let (_, commands_end) = packets_from(CREATE_REQUEST, 0);
  let pushed_pack = CREATE_REQUEST.get(commands_end..).expect("the push's pack");
  assert_eq!((created.0, listed, fetched.0), (200, (200, head), 200));
  assert_eq!(ids_in(&fetched_pack(&fetched.1)), ids_in(pushed_pack));
}

/// Shape: more turns than the `states` fixture's import takes, so an import that never ends fails the test.
const IMPORT_TURNS_AT_MOST: u64 = 1 << 16;

/// A clock one nanosecond on at each reading, so every turn of an import passes its quantum of one nanosecond.
struct Ticking(u64);

impl slates_hg::import::Clock for Ticking {
  fn now_ns(&mut self) -> u64 {
    self.0 = self.0.saturating_add(1);
    self.0
  }
}

/// A Mercurial repository's name that routes to [`REPOSITORY`]'s partition.
fn hg_name_on_the_repositorys_partition() -> String {
  (0u32..)
    .map(|serial| format!("hg-{serial}"))
    .find(|name| usize::from(crate::verbs::owner_of_name(name, usize::from(SHARDS))) == REPOSITORY)
    .expect("a name routes there")
}

/// Creates the Mercurial repository `name` on its partition, owned by [`owner`], and imports `fixtures/state`'s
/// `states` repository into its store there (A-675), a unit a turn to its end: the heads `heads` answers of the history
/// its store then holds, every changeset served while its register holds no phase root, each a revision no other names
/// as a parent, newest first, in hexadecimal, as 7.2 lists them.
fn create_hg_from_states(daemon: &Daemon, name: &str) -> Vec<u8> {
  let name = name.to_owned();
  crate::daemon::observe_at(daemon, REPOSITORY, move |state| {
    let created = crate::verbs::dispatch(
      state,
      1,
      &owner(),
      RequestBody::CreateRepository {
        name: name.clone(),
        tool: VcsTool::Mercurial,
        format: ObjectFormat::Sha1,
        default_branch: "branch:default:0".to_owned(),
        green: None,
      },
    );
    assert!(!matches!(created, ReplyBody::Refused { .. }), "{created:?}");
    let repositories = state.db.partition().repositories();
    let id = repositories.repository_named(&name).expect("the record").id;
    let (host, directory) =
      crate::hg_base::tests::laid_out(crate::hg_base::tests::STATES.iter().copied());
    assert_eq!(
      crate::vcs_store::hg_import::begin(state, id, (Box::new(host), directory)),
      Ok(())
    );
    let mut clock = Ticking(0);
    let ended = (0..IMPORT_TURNS_AT_MOST).find_map(|_| {
      match crate::vcs_store::hg_import::turn(state, id, (&mut clock, 1)) {
        Some(Ok(crate::vcs_store::hg_import::Turned::More)) => None,
        other => Some(other),
      }
    });
    assert!(
      matches!(
        ended,
        Some(Some(Ok(crate::vcs_store::hg_import::Turned::Done(_))))
      ),
      "the import did not end: {ended:?}"
    );
    let (content, store) = state.vcs_stores.mercurial_ref(id).expect("the store");
    let changelog = slates_hg::index::Log::Changelog;
    let rows: Vec<_> = (0..store.index.len(content, changelog))
      .map(|rev| {
        store
          .index
          .changeset(content, rev)
          .expect("a changelog row")
      })
      .collect();
    let named = |rev: u32| rows.iter().any(|row| row.p1 == rev || row.p2 == rev);
    let heads: Vec<String> = (0..rows.len())
      .rev()
      .filter(|rev| !named(u32::try_from(*rev).unwrap()))
      .map(|rev| {
        rows[rev]
          .node
          .iter()
          .map(|byte| format!("{byte:02x}"))
          .collect()
      })
      .collect();
    let mut answer = heads.join(" ").into_bytes();
    answer.push(b'\n');
    answer
  })
}

/// The status and body of `GET /<name>?cmd=<command>` by the owner's bearer token, as stock hg asks a command.
fn hg_command(edge: (u16, &CertificateDer<'static>), name: &str, command: &str) -> (u16, Vec<u8>) {
  let (port, certificate) = edge;
  let request = format!(
    "GET /{name}?cmd={command} HTTP/1.1\r\nHost: localhost\r\nAccept: application/mercurial-0.1\r\n{}\r\n\r\n",
    owner_bearer()
  );
  exchange(port, certificate, request.as_bytes())
}

/// A-680 part 2 (main's ruling, 2026-11-19: Mercurial's site on the daemon's edge beside its lender). Do: start a
/// two-shard daemon whose configuration asks for an edge on loopback with a self-signed identity; on the second shard's
/// partition create a Mercurial repository and import `fixtures/state`'s `states` repository into its store, and create
/// a git repository; enroll the owner's token; then, each on a new TLS connection with the owner's bearer token, ask the
/// Mercurial repository for `capabilities` and `heads` as stock hg asks them (`GET /<name>?cmd=...`), ask the git
/// repository for `capabilities` the same way, and ask the git repository for upload-pack's advertisement. Expect: the
/// two Mercurial commands `200`, the capabilities naming `getbundle` and the `httpheader` bound HTTP's transport adds,
/// and `heads` the heads of the history the store holds, newest first; the git repository asked as Mercurial asks
/// `404`, a repository of another tool answered as one that does not exist; the git request still git's, answered as
/// its upload-pack half answers (`503` until A-682 lends its store, `200` after), never Mercurial's `404`.
#[test]
fn a_mercurial_repositorys_store_is_served_over_the_daemons_edge() {
  let (identity, certificate) = identity();
  let daemon = daemon(identity);
  let port = daemon.vcs_http_port().expect("the edge started");
  let hg = hg_name_on_the_repositorys_partition();
  let git = name_on_the_repositorys_partition();
  let expected_heads = create_hg_from_states(&daemon, &hg);
  create(&daemon, &git);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let edge = (port, &certificate);
  let capabilities = hg_command(edge, &hg, "capabilities");
  let heads = hg_command(edge, &hg, "heads");
  let git_as_hg = hg_command(edge, &git, "capabilities").0;
  let git_advertisement = status_of(
    port,
    &certificate,
    &format!("/{git}/info/refs?service=git-upload-pack"),
    &[&owner_bearer()],
  );
  daemon.stop();
  let line = String::from_utf8_lossy(&capabilities.1).into_owned();
  assert_eq!(capabilities.0, 200, "{line}");
  assert!(
    line.split(' ').any(|cap| cap == "getbundle")
      && line.split(' ').any(|cap| cap.starts_with("httpheader=")),
    "{line}"
  );
  assert_eq!(
    (heads.0, String::from_utf8_lossy(&heads.1).into_owned()),
    (200, String::from_utf8_lossy(&expected_heads).into_owned())
  );
  assert_eq!(git_as_hg, 404);
  assert!(
    matches!(git_advertisement, 200 | 503),
    "git's request answered {git_advertisement}"
  );
}

/// A plain connection to the edge on loopback `port`, its reads waiting [`PATIENCE_NS`].
fn cleartext_client(port: u16) -> ClientStream {
  let socket = ClientStream::connect(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
    .expect("the edge's listener takes the connection");
  socket
    .set_read_timeout(Some(Duration::from_nanos(PATIENCE_NS)))
    .expect("a read timeout");
  socket
}

/// The head and the body of the answer to `request`, sent whole on `stream`.
fn head_and_body(stream: &mut (impl Read + Write), request: &[u8]) -> (String, Vec<u8>) {
  stream.write_all(request).expect("the request is sent");
  let mut head = Vec::new();
  while !head.ends_with(b"\r\n\r\n") {
    head.extend(line_of(stream));
  }
  let head = String::from_utf8_lossy(&head).into_owned();
  let body = body_of(stream, &head);
  (head, body)
}

/// The values of `head`'s `WWW-Authenticate` fields, in the order its answer gives them.
fn challenges_in(head: &str) -> Vec<String> {
  head
    .lines()
    .filter_map(|line| {
      let (name, value) = line.split_once(':')?;
      name
        .eq_ignore_ascii_case("WWW-Authenticate")
        .then(|| value.trim().to_owned())
    })
    .collect()
}

/// The status the answer `head` heads names, if its status line names one.
fn status_in(head: &str) -> Option<u16> {
  head.split(' ').nth(1)?.parse().ok()
}

/// The status and the challenges of the answer `head` heads.
fn status_and_challenges(head: &str) -> (Option<u16>, Vec<String>) {
  (status_in(head), challenges_in(head))
}

/// The status and the challenges an edge whose realm names `instance` answers a request with no credentials: `401`,
/// Basic's challenge with its charset first and Bearer's second (A-686).
fn challenged(instance: &str) -> (Option<u16>, Vec<String>) {
  (
    Some(401),
    vec![
      format!("Basic realm=\"slates {instance}\", charset=\"UTF-8\""),
      format!("Bearer realm=\"slates {instance}\""),
    ],
  )
}

/// Format: a request for a git repository's refs that carries no credentials.
const BARE_REQUEST: &[u8] =
  b"GET /r.git/info/refs?service=git-upload-pack HTTP/1.1\r\nHost: localhost\r\n\r\n";

/// A-686 (main's ruling, 2026-11-19: Basic's challenge first on every connection, TLS included). Do: start a daemon
/// whose edge serves loopback TLS and ask it for a repository's refs with no credentials; then one whose edge serves
/// loopback cleartext (no identity) and ask it the same. Expect: both answer `401` with two challenges, Basic's with
/// its charset first and Bearer's second, each naming its daemon's realm (`slates <instance>`), so stock Mercurial 7.2,
/// whose urllib reads the first challenge alone, reads Basic from either edge.
#[test]
fn a_daemons_edge_challenges_basic_first_then_bearer_over_tls_and_in_cleartext() {
  let (identity, certificate) = identity();
  let (daemon, over_tls) = daemon_named(Some(identity));
  let port = daemon.vcs_http_port().expect("the edge started");
  let (tls_head, _) = head_and_body(&mut tls_client(port, &certificate), BARE_REQUEST);
  daemon.stop();
  let (daemon, in_cleartext) = daemon_named(None);
  let port = daemon.vcs_http_port().expect("the edge started");
  let (cleartext_head, _) = head_and_body(&mut cleartext_client(port), BARE_REQUEST);
  daemon.stop();
  assert_eq!(
    (
      status_and_challenges(&tls_head),
      status_and_challenges(&cleartext_head)
    ),
    (challenged(&over_tls), challenged(&in_cleartext))
  );
}

/// A-686 (main's ruling, 2026-11-19: Basic with the token as the password is taken on the loopback cleartext edge as
/// over TLS). Do: start a two-shard daemon whose edge serves loopback cleartext (no identity); create a git repository
/// on the second shard's partition and enroll its owner's token; then, each on a new plain connection, ask for
/// receive-pack's advertisement with the owner's token as Basic's password (`git:<token>`, the only credential stock
/// git sends), and with no credentials. Expect: the Basic request proven, moved to the repository's shard and answered
/// `200` with the push driver's advertisement (the prelude, then no ref), the bare one `401`, and the listener's shard
/// counting the one move.
#[test]
fn basic_proves_its_principal_on_a_daemons_loopback_cleartext_edge() {
  let (daemon, _) = daemon_named(None);
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = name_on_the_repositorys_partition();
  create(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let advertise = |credentials: &str| {
    format!(
      "GET /{name}/info/refs?service=git-receive-pack HTTP/1.1\r\nHost: localhost\r\n\
       {credentials}\r\n"
    )
  };
  let basic = advertise(&format!("{OWNER_BASIC}\r\n"));
  let (basic_head, basic_body) = head_and_body(&mut cleartext_client(port), basic.as_bytes());
  let (bare_head, _) = head_and_body(&mut cleartext_client(port), advertise("").as_bytes());
  let moved = counts_on(&daemon, LISTENER).moved;
  daemon.stop();
  assert_eq!(
    (
      status_in(&basic_head),
      basic_body,
      status_in(&bare_head),
      moved
    ),
    (Some(200), served_advertisement(&[]), Some(401), 1),
    "{basic_head}"
  );
}

/// Shape: the variable naming the command that runs stock Mercurial 7.2, its words split at spaces: on the Mac Studio
/// `uvx --quiet --python 3.12 --from mercurial==7.2 hg`, as `crates/hg/fixtures/make-fixtures.sh` runs it. Unset, `hg`
/// on the path.
const STOCK_HG: &str = "SLATES_TEST_HG";

/// Stock Mercurial 7.2's command, its words as [`STOCK_HG`] names them, or `None` when this host runs none, which the
/// caller skips loudly: the command must answer `--version` with 7.2's line.
fn stock_hg() -> Option<Vec<String>> {
  let line = std::env::var(STOCK_HG).unwrap_or_else(|_| "hg".to_owned());
  let words: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
  let (program, leading) = words.split_first()?;
  let version = Command::new(program)
    .args(leading)
    .arg("--version")
    .env("HGPLAIN", "1")
    .env("HGRCPATH", "")
    .stdin(Stdio::null())
    .output()
    .ok()?;
  let said = String::from_utf8_lossy(&version.stdout);
  (version.status.success() && said.contains("(version 7.2")).then_some(words)
}

/// Runs stock hg (its command's `words`) with `args`, outside any user's configuration (`HGRCPATH` empty), in its plain
/// words (`HGPLAIN`), with no terminal to prompt on and each of its socket's waits bounded by [`PATIENCE_NS`]
/// (`http.timeout`): its exit code and both outputs.
fn hg_run(words: &[String], args: &[&str]) -> (Option<i32>, String, String) {
  let (program, leading) = words.split_first().expect("a command");
  let timeout = format!(
    "http.timeout={}",
    Duration::from_nanos(PATIENCE_NS).as_secs_f64()
  );
  let output = Command::new(program)
    .args(leading)
    .args(["--config", &timeout])
    .args(args)
    .env("HGPLAIN", "1")
    .env("HGRCPATH", "")
    .stdin(Stdio::null())
    .output()
    .expect("stock hg runs");
  (
    output.status.code(),
    String::from_utf8_lossy(&output.stdout).into_owned(),
    String::from_utf8_lossy(&output.stderr).into_owned(),
  )
}

/// A-686 with A-680 part 2a (main's ruling, 2026-11-19: Basic with the token as the password taken on the loopback
/// cleartext edge as over TLS, Basic's challenge first). Do: start a two-shard daemon whose edge serves loopback
/// cleartext (no identity); on the second shard's partition create a Mercurial repository from `fixtures/state`'s
/// `states` and enroll its owner's token; run stock Mercurial 7.2's `hg debugcapabilities` against the repository with
/// the token as Basic's password in the URL. Expect: hg reads the edge's `401`, whose first challenge is Basic's, sends
/// Basic, which the loopback edge takes, and is answered by Mercurial's site from the repository's store on its owner
/// shard: exit 0, `getbundle` among the capabilities it lists. Before A-686 the cleartext edge's `401` offered Bearer
/// alone, on which 7.2's urllib digest handler, reading the first challenge alone, raised (`** unknown exception`, exit
/// 1). Skips loudly without stock hg 7.2 ([`STOCK_HG`]).
#[test]
fn stock_hg_reads_a_repositorys_capabilities_on_a_daemons_loopback_edge_by_basic() {
  let Some(hg) = stock_hg() else {
    eprintln!(
      "skipping stock hg against the daemon's edge: no Mercurial 7.2 here \
       (set {STOCK_HG} to the command that runs it)"
    );
    return;
  };
  let (daemon, _) = daemon_named(None);
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = hg_name_on_the_repositorys_partition();
  create_hg_from_states(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let url = format!("http://hg:{OWNER_TOKEN}@127.0.0.1:{port}/{name}");
  let (code, out, err) = hg_run(&hg, &["debugcapabilities", &url]);
  daemon.stop();
  assert_eq!(
    (code, out.lines().any(|line| line.trim() == "getbundle")),
    (Some(0), true),
    "{out}{err}"
  );
}
