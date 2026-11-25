//! Tests of the daemon's HTTP edge (A-625 part 3): a daemon in-process starts it from its configuration, and a rustls
//! client over loopback TLS asks for a repository another shard's partition holds, with each kind of credential;
//! pushes to it the bytes stock git sent (A-654 part 3); fetches back what it pushed (A-682); and pushes to it the
//! bundle stock Mercurial pushed (A-680 part 2b).

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
use slates_hg::bookmarks::{self as hg_bookmarks, Bookmark};
use slates_hg::bundle2::{Bundle2, Event, Param, Part};
use slates_ipc::protocol::{
  CredentialMaterial, ObjectFormat, PolicyEffect, PolicyNames, PolicyRight, PolicyRule, PolicyWho,
  ReplyBody, RequestBody, VcsTool,
};
use slates_rt::tcp::{Ipv4Addr, SocketAddrV4};

use crate::config::{EdgeIdentity, VcsHttpConfig};
use crate::daemon::{Daemon, SegmentSource};
use crate::repository::hg_push::tests::{Commit, Made, changegroup, history_of};
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
/// Shape: how long a client waits for an answer in a developer's debug build: ten of the daemon's liveness budgets, past
/// a [`LONG_HISTORY`] push's and fetch's answers there, as a stock client waits for as long as its server works.
const PATIENCE_NS: u64 = 10 * crate::daemon::LIVENESS_BUDGET_NS;

/// Derived: how long a client waits for an answer in this lane: [`PATIENCE_NS`] times the lane's declared slowdown
/// ([`crate::test_lane`]). Unscaled, ThreadSanitizer's lane gave up on four answers at the ten budgets before a byte of
/// them came (diag run 45315777446, 2026-11-24: "the answer ended inside a line", the read timed out with nothing read).
fn patience_ns() -> u64 {
  PATIENCE_NS.saturating_mul(crate::test_lane::slowdown())
}

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
fn daemon(identity: EdgeIdentity) -> crate::test_store::StoredDaemon {
  daemon_named(Some(identity)).0
}

/// A daemon of [`SHARDS`] shards whose configuration asks for an edge on loopback presenting `identity`, or serving
/// cleartext without one, and its instance's name, which its edge's realm names (`slates <instance>`).
fn daemon_named(identity: Option<EdgeIdentity>) -> (crate::test_store::StoredDaemon, String) {
  daemon_configured(identity, |_| {})
}

/// [`daemon_named`], its configuration changed by `configure` before it starts. Every daemon here holds a changeset
/// store (`crate::test_store`): a repository's history lands there, and a daemon without one makes no repository of any
/// tool (C1, C2, C5).
fn daemon_configured(
  identity: Option<EdgeIdentity>,
  configure: impl FnOnce(&mut crate::DaemonConfig),
) -> (crate::test_store::StoredDaemon, String) {
  let (profile, config, instance) = edge_config(identity, configure);
  let daemon = crate::test_store::StoredDaemon::start(
    "vcs-http",
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

/// The profile, configuration and instance name of a daemon of [`SHARDS`] shards serving the edge on loopback presenting
/// `identity`, or cleartext without one, its configuration changed by `configure`.
fn edge_config(
  identity: Option<EdgeIdentity>,
  configure: impl FnOnce(&mut crate::DaemonConfig),
) -> (slates_machine::MachineProfile, crate::DaemonConfig, String) {
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
  configure(&mut config);
  (profile, config, instance)
}

/// A git repository's name that routes to [`REPOSITORY`]'s partition, not the listener's.
fn name_on_the_repositorys_partition() -> String {
  (0u32..)
    .map(|serial| format!("moved-{serial}.git"))
    .find(|name| usize::from(crate::verbs::owner_of_name(name, usize::from(SHARDS))) == REPOSITORY)
    .expect("a name routes there")
}

/// Creates the git repository `name` on its partition, owned by [`owner`], as a client's creation is made: answered from
/// a task of the shard while this thread waits for it to be recorded ([`crate::test_store::create_on`]). Made in a
/// history holding the shard (`test_store::create`), the shard waited out the store's commit in a thread's `join`, and
/// a commit slower than the liveness window ended the whole test binary (exit 4, "shard 1 is wedged").
fn create(daemon: &Daemon, name: &str) {
  crate::test_store::create_on(
    daemon,
    REPOSITORY,
    &owner(),
    (name, VcsTool::Git, ObjectFormat::Sha1, "main"),
  );
}

/// Enrolls `token` for uid `uid` until `expires_ns`, on the partition its hash routes to, as the human surface does.
fn enroll(daemon: &Daemon, token: &str, uid: u32, expires_ns: Option<u64>) {
  let id = crate::credentials::token_id(token.as_bytes());
  let cram_md5_contexts = crate::credentials::cram_md5_contexts(token.as_bytes());
  let partition = usize::from(crate::credentials::owner_of(&id, usize::from(SHARDS)));
  let reply = crate::daemon::observe_at(daemon, partition, move |state| {
    let principal = slates_ipc::protocol::Principal::Uid { host: 0, uid };
    let material = CredentialMaterial::HttpToken {
      id,
      cram_md5_contexts,
    };
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
fn tls_client(
  port: u16,
  certificate: &CertificateDer<'static>,
) -> StreamOwned<ClientConnection, ClientStream> {
  tls_client_within(port, certificate, Duration::from_nanos(patience_ns()))
}

/// [`tls_client`], its reads waiting at most `patience` for an answer.
#[allow(clippy::disallowed_types)] // rustls takes its configuration and provider as `Arc` by signature (D-8's test-harness exception).
fn tls_client_within(
  port: u16,
  certificate: &CertificateDer<'static>,
  patience: Duration,
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
    .set_read_timeout(Some(patience))
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
  let (status, _, body) = typed_exchange(port, certificate, request);
  (status, body)
}

/// The status, `Content-Type` (empty where none is named) and body of the answer to `request`, sent whole on a new TLS
/// connection to the edge on `port`.
fn typed_exchange(
  port: u16,
  certificate: &CertificateDer<'static>,
  request: &[u8],
) -> (u16, String, Vec<u8>) {
  typed_exchange_within(
    port,
    certificate,
    request,
    Duration::from_nanos(patience_ns()),
  )
}

/// [`typed_exchange`], waiting at most `patience` for each read of its answer.
fn typed_exchange_within(
  port: u16,
  certificate: &CertificateDer<'static>,
  request: &[u8],
  patience: Duration,
) -> (u16, String, Vec<u8>) {
  let mut stream = tls_client_within(port, certificate, patience);
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
  let media_type = head
    .lines()
    .find_map(|field| {
      let (name, value) = field.split_once(':')?;
      name
        .eq_ignore_ascii_case("content-type")
        .then(|| value.trim().to_owned())
    })
    .unwrap_or_default();
  (status, media_type, body_of(&mut stream, &head))
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

/// Goal 93 (main's ruling of 2026-11-24 00:44): an hg reply answered whole, judged by its body as well as its status:
/// Mercurial answers a refused push or fetch `200` with an abort part in its bundle (`ERROR:ABORT`, its words after
/// it), so a status alone hid `push refused: Index(Unread { rev: 5 })` (C1 (1)'s red).
fn assert_hg_answered(reply: &(u16, String, Vec<u8>)) {
  let body = String::from_utf8_lossy(&reply.2);
  assert_eq!(reply.0, 200, "{body}");
  assert!(
    !body.contains("ERROR:ABORT"),
    "answered with an abort: {body}"
  );
}

/// Goal 93 (main's ruling of 2026-11-24 00:44): a git push accepted, judged by its report as well as its status: a
/// refused ref is reported `ng` in a `200` answer, so a status alone cannot tell it.
fn assert_git_pushed(reply: &(u16, Vec<u8>)) {
  let report = String::from_utf8_lossy(&reply.1);
  assert_eq!(reply.0, 200, "{report}");
  assert!(report.contains("unpack ok"), "unpacked: {report}");
  assert!(!report.contains("ng refs/"), "every ref taken: {report}");
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
        hg_publishing: true,
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
pub(crate) fn stored_pack(objects: &[(slates_git::object_name::Kind, &[u8])]) -> Vec<u8> {
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
pub(crate) fn git_name(kind: slates_git::object_name::Kind, content: &[u8]) -> ObjectId {
  slates_git::object_name::name_of(GitFormat::Sha1, kind, content).expect("named")
}

/// Stock git's request creating `main` at a root commit whose one file is `bytes` long, and that commit: the file's
/// bytes counted out by a 64-bit LCG (Knuth's MMIX constants), so no two windows of it repeat.
pub(crate) fn large_create(bytes: usize) -> (Vec<u8>, ObjectId) {
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

/// Shape: the commits of [`long_history`]: as many as git/git's history to v1.0.0 (2,930), whose every clone was refused
/// for its room.
pub(crate) const LONG_HISTORY: usize = 3_000;

/// Stock git's request creating `main` at the last of `commits` commits in one line, each changing the one file, and that
/// commit: three objects a commit.
pub(crate) fn long_history(commits: usize) -> (Vec<u8>, ObjectId) {
  let (request, made) = long_history_refs(commits, 0);
  (request, made.last().copied().expect("a commit"))
}

/// [`long_history`]'s line of `commits` commits, the request creating `main` at its last and `branches` more branches
/// `b0`, `b1`, ... at commits spread along it, and every commit made, oldest first.
fn long_history_refs(commits: usize, branches: usize) -> (Vec<u8>, Vec<ObjectId>) {
  use slates_git::object_name::Kind;
  let mut objects: Vec<(Kind, Vec<u8>)> = Vec::new();
  let mut parent: Option<ObjectId> = None;
  let mut made = Vec::new();
  for at in 0..commits {
    let blob = format!("line {at}\n").into_bytes();
    let mut tree = b"100644 file\0".to_vec();
    tree.extend_from_slice(git_name(Kind::Blob, &blob).as_bytes());
    let tree_id = git_name(Kind::Tree, &tree);
    let parent_line = parent.map_or_else(String::new, |parent| format!("parent {parent}\n"));
    let commit = format!(
      "tree {tree_id}\n{parent_line}author A <a@example.com> 1700000000 +0000\n\
       committer A <a@example.com> 1700000000 +0000\n\ncommit {at}\n"
    )
    .into_bytes();
    parent = Some(git_name(Kind::Commit, &commit));
    made.extend(parent);
    objects.extend([
      (Kind::Commit, commit),
      (Kind::Tree, tree),
      (Kind::Blob, blob),
    ]);
  }
  let last = parent.expect("a commit");
  let listed: Vec<(Kind, &[u8])> = objects
    .iter()
    .map(|(kind, bytes)| (*kind, bytes.as_slice()))
    .collect();
  let mut request = Vec::new();
  // Stock git's capabilities (A-722's keepalive: the answer multiplexed, so the edge can keep it alive while it is made).
  let line = format!("{ZERO} {last} refs/heads/main\0{CAPABILITIES}");
  pktline::write(Packet::Data(line.as_bytes()), &mut request).expect("the command");
  for branch in 0..branches {
    let at = made[branch * commits / branches.max(1)];
    let line = format!("{ZERO} {at} refs/heads/b{branch}");
    pktline::write(Packet::Data(line.as_bytes()), &mut request).expect("a command");
  }
  pktline::write(Packet::Flush, &mut request).expect("the flush");
  request.extend_from_slice(&stored_pack(&listed));
  (request, made)
}

/// Shape: the branches [`a_push_of_a_long_history_and_many_branches_lands_every_ref_through_the_daemons_edge`] creates beside
/// `main`: a hundred, each one command of the push's decision.
const MANY_BRANCHES: usize = 100;

/// A-719 (found by use, 2026-11-20: stock git's push of git/git, 1,027 refs, into a release slates spun its shard,
/// 1,046 ms of CPU without a step, and the anchor killed the daemon). Do: push through the daemon's edge a line of
/// [`LONG_HISTORY`] commits with `main` at its last and [`MANY_BRANCHES`] branches along it, as stock git posts them.
/// Expect: the push `200`, every ref reported `ok`, the daemon's watchdog never finding its shard stuck (it ends the
/// test's process when it does). A step's own length by the wall clock is no test here: on a loaded host it measures
/// the host; `git_push::tests::t_a719_a_push_is_decided_a_command_a_step` holds the decision to a command a step.
#[test]
fn a_push_of_a_long_history_and_many_branches_lands_every_ref_through_the_daemons_edge() {
  let (identity, certificate) = identity();
  let daemon = daemon(identity);
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = name_on_the_repositorys_partition();
  create(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let (request, _) = long_history_refs(LONG_HISTORY, MANY_BRANCHES);
  let created = pushed((port, &certificate), &name, &request);
  daemon.stop();
  let report = String::from_utf8_lossy(&created.1).into_owned();
  let landed = report.matches("ok refs/heads/").count();
  assert_eq!((created.0, landed), (200, MANY_BRANCHES + 1), "{report}");
}

/// Shape: how long [`a_fetch_whose_let_go_window_the_store_never_answers_fails_cleanly_within_its_bound`]'s node store
/// is waited for on each read: its stand-in for `read_through::PATIENCE_NS`, short so the test is.
const UNANSWERED_PATIENCE_NS: u64 = 200_000_000;

/// Goal 93's read-through bound (main 01a20182's ruling of 2026-11-22 17:00: an await without a bound is a defect on
/// its own, banned items 8 and 9). Do: on a two-shard daemon with its edge, push a commit whose one file spans several
/// of the content store's windows; on the repository's shard, let every window of its staged pack go as landed (those
/// the push's own let-go has not), and
/// make the node's store never answer a bring-back's read (`read_through::TEST_UNANSWERED`), each waited for
/// [`UNANSWERED_PATIENCE_NS`]; then fetch the commit as `git clone` asks, from a thread the test waits on. Expect: the
/// fetch answered, not hung (well inside a minute); its answer no whole pack of every object pushed, so the client fails
/// cleanly; the refusals counted `vcs_store.read_through.unanswered`. Failing first: the bring-back awaited the store
/// without end and the fetch never answered.
#[test]
fn a_fetch_whose_let_go_window_the_store_never_answers_fails_cleanly_within_its_bound() {
  let (identity, certificate) = identity();
  let daemon = daemon(identity);
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = name_on_the_repositorys_partition();
  create(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let (request, commit) = large_create(1 << 20);
  let created = pushed((port, &certificate), &name, &request);
  assert_git_pushed(&created);
  let (_, commands_end) = packets_from(&request, 0);
  let pack = request
    .get(commands_end..)
    .expect("the push's pack")
    .to_vec();
  let let_go = crate::daemon::observe_at(&daemon, REPOSITORY, move |state| {
    crate::vcs_store::read_through::TEST_UNANSWERED.with(|unanswered| unanswered.set(true));
    crate::vcs_store::read_through::TEST_PATIENCE_NS
      .with(|patience| patience.set(UNANSWERED_PATIENCE_NS));
    crate::vcs_store::within_budget(state, 0, |stores| {
      let Some(content) = stores.content_mut() else {
        return 0;
      };
      let window = content.window();
      pack
        .chunks(window)
        .map(slates_vcs::ContentId::of)
        // Let go here, or by the push's own landed let-go already (its windows no home pins).
        .filter(|id| {
          if content.mark_landed(id) {
            let _ = content.let_go_landed(id, None);
          }
          content.is_away(id)
        })
        .count()
    })
  });
  assert!(let_go > 1, "the pack's windows let go: {let_go}");
  let (sent, got) = std::sync::mpsc::channel();
  let fetch = fetch_of(commit);
  let asked = std::thread::spawn(move || {
    let _ = sent.send(uploaded((port, &certificate), &name, &fetch));
  });
  let answered = got.recv_timeout(Duration::from_secs(60));
  let unanswered = counted_on(
    &daemon,
    REPOSITORY,
    crate::vcs_store::read_through::UNANSWERED,
  );
  let answered = answered.expect("the fetch answered within its bound, never hung");
  let _ = asked.join();
  daemon.stop();
  let (status, body) = answered;
  // A pack read whole holding every object pushed, or not: an answer cut short or ended on git's error band is not.
  let pushed_objects = ids_in(&request[commands_end..]).len();
  let whole = status == 200
    && body.starts_with(b"000dpackfile\n")
    && std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      ids_in(&fetched_pack(&body)).len()
    }))
    .is_ok_and(|fetched| fetched == pushed_objects);
  assert!(
    !whole,
    "no whole pack answered from windows the store never gave back"
  );
  assert!(unanswered > 0, "each refusal counted");
}

/// Shape: the annotated tags [`a_push_of_annotated_tags_is_listed_and_fetched_whole_once_its_pack_is_landed`] pushes,
/// each after a file of [`TAGGED_FILE_BYTES`] in its pack, so each lies in a window of its own.
const TAGS: usize = 4;
/// Shape: the bytes of each file before a tag in that test's pack: past a window of the content store (256 KiB).
const TAGGED_FILE_BYTES: usize = 400 * 1024;

/// Stock git's request creating `main` at a root commit of [`TAGS`] files and an annotated tag `refs/tags/t<k>` of it
/// for each, the pack laid out commit, tree, then each file followed by its tag; the commit, and each tag's name.
fn tagged_create() -> (Vec<u8>, ObjectId, Vec<ObjectId>) {
  use slates_git::object_name::Kind;
  let mut state = 0x2545_f491_4f6c_dd1du64;
  let files: Vec<Vec<u8>> = (0..TAGS)
    .map(|_| {
      (0..TAGGED_FILE_BYTES)
        .map(|_| {
          state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
          state.to_be_bytes()[0]
        })
        .collect()
    })
    .collect();
  let mut tree = Vec::new();
  for (at, file) in files.iter().enumerate() {
    tree.extend_from_slice(format!("100644 f{at}\0").as_bytes());
    tree.extend_from_slice(git_name(Kind::Blob, file).as_bytes());
  }
  let tree_id = git_name(Kind::Tree, &tree);
  let commit = format!(
    "tree {tree_id}\nauthor A <a@example.com> 1700000000 +0000\n\
     committer A <a@example.com> 1700000000 +0000\n\ntagged\n"
  )
  .into_bytes();
  let commit_id = git_name(Kind::Commit, &commit);
  let tags: Vec<Vec<u8>> = (0..TAGS)
    .map(|at| {
      format!(
        "object {commit_id}\ntype commit\ntag t{at}\ntagger A <a@example.com> 1700000000 +0000\n\ntag {at}\n"
      )
      .into_bytes()
    })
    .collect();
  let tag_ids: Vec<ObjectId> = tags.iter().map(|tag| git_name(Kind::Tag, tag)).collect();
  let mut objects: Vec<(Kind, &[u8])> = vec![(Kind::Commit, &commit), (Kind::Tree, &tree)];
  for (file, tag) in files.iter().zip(&tags) {
    objects.push((Kind::Blob, file));
    objects.push((Kind::Tag, tag));
  }
  let mut request = Vec::new();
  let line = format!("{ZERO} {commit_id} refs/heads/main\0 report-status");
  pktline::write(Packet::Data(line.as_bytes()), &mut request).expect("the command");
  for (at, tag) in tag_ids.iter().enumerate() {
    let line = format!("{ZERO} {tag} refs/tags/t{at}");
    pktline::write(Packet::Data(line.as_bytes()), &mut request).expect("a command");
  }
  pktline::write(Packet::Flush, &mut request).expect("the flush");
  request.extend_from_slice(&stored_pack(&objects));
  (request, commit_id, tag_ids)
}

/// Goal 93 item 2b(a) (main 01a20182's r52, 2026-11-22: once git/git's push ended its landed pack was let go, and every
/// fetch of it failed, ls-remote and clone alike, "aborting due to possible repository corruption on the remote side":
/// the advertisement's tag peel read a window let go, `Unreadable`, where no read brings it back). Do: on a two-shard
/// daemon with its edge, push as stock git posts it a commit of [`TAGS`] files and an annotated tag of it for each, each
/// tag in a window of its own; wait for the push's end to run its landed let-go (counted, or 3 s), and let the
/// repository go idle (`git_encode::give_back_idle`: the objects its reads keep given back); then, as `git ls-remote`
/// and `git clone` ask at protocol v2, list every ref with its peel and fetch the commit and every tag.
/// Expect: the push `200`, every ref `ok`; the listing exactly `main` and each tag peeled to the commit; the fetch `200`,
/// its pack every object pushed. Failing first: the tags' windows let go, the listing refused.
#[test]
fn a_push_of_annotated_tags_is_listed_and_fetched_whole_once_its_pack_is_landed() {
  let (identity, certificate) = identity();
  let daemon = daemon(identity);
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = name_on_the_repositorys_partition();
  create(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let edge = (port, &certificate);
  let (request, commit, tags) = tagged_create();
  let created = pushed(edge, &name, &request);
  // The push's end runs its landed let-go (its `ReceivePack` dropped) as the edge finishes its answer.
  let waited = std::time::Instant::now();
  while counted_on(&daemon, REPOSITORY, crate::vcs_store::LANDED_LET_GO_BYTES) == 0
    && waited.elapsed() < Duration::from_secs(3)
  {
    // The harness paces its looks; the daemon under test waits on its wheel.
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(Duration::from_millis(20));
  }
  // Then the repository goes idle, as it does once no fetch of it is answered: what its reads keep of the objects read
  // lately (`packed::Recent`, which the push's decision filled with the tags) given back, so the listing reads them from
  // their windows, as git/git's tags, far past that cache, were read.
  let let_go = crate::daemon::observe_at(&daemon, REPOSITORY, |state| {
    for id in state.vcs_stores.git_repositories() {
      crate::vcs_store::git_encode::give_back_idle(state, id);
    }
    let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
    (
      count(crate::vcs_store::LANDED_LET_GO_BYTES),
      count(crate::vcs_store::LANDED_PINNED),
    )
  });
  let mut ls_refs = Vec::new();
  pktline::write(Packet::Data(b"command=ls-refs\n"), &mut ls_refs).expect("the command");
  pktline::write(Packet::Delim, &mut ls_refs).expect("the delimiter");
  for argument in [b"peel\n".as_slice(), b"ref-prefix refs/\n"] {
    pktline::write(Packet::Data(argument), &mut ls_refs).expect("an argument");
  }
  pktline::write(Packet::Flush, &mut ls_refs).expect("the flush");
  let listed = uploaded(edge, &name, &ls_refs);
  let mut fetch = Vec::new();
  pktline::write(Packet::Data(b"command=fetch\n"), &mut fetch).expect("the command");
  pktline::write(Packet::Delim, &mut fetch).expect("the delimiter");
  let mut arguments = vec![format!("want {commit}\n")];
  arguments.extend(tags.iter().map(|tag| format!("want {tag}\n")));
  arguments.extend(["no-progress\n".to_owned(), "done\n".to_owned()]);
  for argument in &arguments {
    pktline::write(Packet::Data(argument.as_bytes()), &mut fetch).expect("an argument");
  }
  pktline::write(Packet::Flush, &mut fetch).expect("the flush");
  let fetched = uploaded(edge, &name, &fetch);
  daemon.stop();
  eprintln!("landed let-go bytes and windows pinned before the listing: {let_go:?}");
  let report = String::from_utf8_lossy(&created.1).into_owned();
  assert_eq!(
    (created.0, report.matches("ok refs/").count()),
    (200, TAGS + 1),
    "{report}"
  );
  let mut expected = Vec::new();
  let main = format!("{commit} refs/heads/main\n");
  pktline::write(Packet::Data(main.as_bytes()), &mut expected).expect("a line");
  for (at, tag) in tags.iter().enumerate() {
    let line = format!("{tag} refs/tags/t{at} peeled:{commit}\n");
    pktline::write(Packet::Data(line.as_bytes()), &mut expected).expect("a line");
  }
  pktline::write(Packet::Flush, &mut expected).expect("the flush");
  assert_eq!(
    (listed.0, String::from_utf8_lossy(&listed.1)),
    (200, String::from_utf8_lossy(&expected)),
    "every ref listed, each tag peeled"
  );
  assert_eq!(fetched.0, 200);
  let answer = String::from_utf8_lossy(
    fetched
      .1
      .get(..fetched.1.len().min(400))
      .unwrap_or_default(),
  )
  .into_owned();
  assert!(answer.starts_with("000dpackfile\n"), "{answer}");
  let (_, commands_end) = packets_from(&request, 0);
  let pushed_pack = request.get(commands_end..).expect("the push's pack");
  assert_eq!(
    ids_in(&fetched_pack(&fetched.1)).len(),
    ids_in(pushed_pack).len()
  );
}

/// A v2 `fetch` of `want` with `done` and no progress, as `git clone` asks for a branch's commit.
pub(crate) fn fetch_of(want: ObjectId) -> Vec<u8> {
  let mut request = Vec::new();
  pktline::write(Packet::Data(b"command=fetch\n"), &mut request).expect("the command");
  pktline::write(Packet::Delim, &mut request).expect("the delimiter");
  for argument in [
    format!("want {want}\n"),
    "no-progress\n".to_owned(),
    "done\n".to_owned(),
  ] {
    pktline::write(Packet::Data(argument.as_bytes()), &mut request).expect("an argument");
  }
  pktline::write(Packet::Flush, &mut request).expect("the flush");
  request
}

/// A v2 `fetch` of `want` one commit deep, as `git clone --depth 1` asks: `want` it, `deepen 1`, `done`.
pub(crate) fn shallow_fetch_of(want: ObjectId) -> Vec<u8> {
  let mut request = Vec::new();
  pktline::write(Packet::Data(b"command=fetch\n"), &mut request).expect("the command");
  pktline::write(Packet::Delim, &mut request).expect("the delimiter");
  for argument in [
    format!("want {want}\n"),
    "deepen 1\n".to_owned(),
    "no-progress\n".to_owned(),
    "done\n".to_owned(),
  ] {
    pktline::write(Packet::Data(argument.as_bytes()), &mut request).expect("an argument");
  }
  pktline::write(Packet::Flush, &mut request).expect("the flush");
  request
}

/// The pack a protocol v2 `fetch` answer carries after whatever sections come before it (a shallow fetch's
/// `shallow-info`): [`fetched_pack`] from its `packfile` section on.
pub(crate) fn fetched_pack_after(answer: &[u8]) -> Vec<u8> {
  let section = b"000dpackfile\n";
  let at = answer
    .windows(section.len())
    .position(|window| window == section)
    .expect("a packfile section");
  fetched_pack(&answer[at..])
}

/// Shape: the commits of [`a_long_pushs_answer_is_kept_alive_with_gits_keepalive_until_its_report`]'s push: enough
/// that its answer takes many turns.
pub(crate) const KEPT_ALIVE_HISTORY: usize = 300;

/// A-722's keepalive (the load-only edge tests: a long push's answer wrote nothing until its report, and the client's
/// read gave up on the silence at a load of 56 to 67; git's receive-pack writes `0005\1` every `receive.keepAlive`
/// while it processes a push under side-band-64k). Do: start a daemon whose keepalive interval is none; push a history
/// of [`KEPT_ALIVE_HISTORY`] commits under side-band-64k over its edge. Expect: `200`; the answer begun with git's
/// keepalive, and its report whole after the keepalives (`unpack ok`, `ok refs/heads/main`).
#[test]
fn a_long_pushs_answer_is_kept_alive_with_gits_keepalive_until_its_report() {
  let (identity, certificate) = identity();
  let (daemon, _) = daemon_configured(Some(identity), |config| config.git_keepalive_ns = 0);
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = name_on_the_repositorys_partition();
  create(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let (request, _) = long_history(KEPT_ALIVE_HISTORY);
  let (status, answer) = pushed((port, &certificate), &name, &request);
  daemon.stop();
  assert_eq!(status, 200, "{}", String::from_utf8_lossy(&answer));
  let keepalive = b"0005\x01";
  assert!(
    answer.starts_with(keepalive),
    "{}",
    String::from_utf8_lossy(&answer)
  );
  let mut rest = answer.as_slice();
  while let Some(after) = rest.strip_prefix(keepalive.as_slice()) {
    rest = after;
  }
  let report = String::from_utf8_lossy(rest);
  assert!(
    report.contains("unpack ok") && report.contains("ok refs/heads/main"),
    "{report}"
  );
}

/// A-722's keepalive on the fetch (main's goal-93 piece, 2026-11-21: a fetch's answer wrote nothing from its
/// `packfile` section's head until its pack's first packet while the pack's objects were chosen over many turns; git's
/// upload-pack writes `0005\1` while `pack-objects` is silent, git 2.50.1's `create_pack_file`). Do: start a daemon
/// whose keepalive interval is none; push a history of [`KEPT_ALIVE_HISTORY`] commits; fetch its last commit at
/// protocol v2 with `done`, as `git clone` asks. Expect: `200`; the `packfile` section's first packet git's keepalive
/// (an empty band-1 packet), and the section's pack whole after the keepalives, every object pushed.
/// Shape: how much of each of a fetch's first packets [`a_long_fetchs_answer_is_kept_alive_with_gits_keepalive_while_its_pack_is_chosen`]
/// compares and shows: past a section's name and a keepalive, short of a pack's whole packet.
pub(crate) const OPENING_SHOWN: usize = 16;

#[test]
fn a_long_fetchs_answer_is_kept_alive_with_gits_keepalive_while_its_pack_is_chosen() {
  let (identity, certificate) = identity();
  let (daemon, _) = daemon_configured(Some(identity), |config| config.git_keepalive_ns = 0);
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = name_on_the_repositorys_partition();
  create(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let edge = (port, &certificate);
  let (request, last) = long_history(KEPT_ALIVE_HISTORY);
  let created = pushed(edge, &name, &request);
  let fetched = uploaded(edge, &name, &fetch_of(last));
  daemon.stop();
  assert_git_pushed(&created);
  assert_eq!(fetched.0, 200);
  let (lines, _) = packets_from(&fetched.1, 0);
  let opening: Vec<&[u8]> = lines
    .iter()
    .take(2)
    .map(|line| {
      line
        .get(..line.len().min(OPENING_SHOWN))
        .unwrap_or_default()
    })
    .collect();
  assert_eq!(opening, [b"packfile\n".as_slice(), [1].as_slice()]);
  let (_, commands_end) = packets_from(&request, 0);
  let pushed_pack = request.get(commands_end..).expect("the push's pack");
  assert_eq!(
    ids_in(&fetched_pack(&fetched.1)).len(),
    ids_in(pushed_pack).len()
  );
}

/// A-713 (found by use, 2026-11-20: with git/git cut at v1.0.0, 2,930 commits in a 3.4 MB pack, the push landed and every
/// clone, over HTTP and SSH, was refused `upload-pack: the request needs more room than the server admits`). Do: start a
/// two-shard daemon whose edge serves loopback TLS, each request admitted one ring region; push a line of
/// [`LONG_HISTORY`] commits as stock git posts it; then, as the owner at protocol v2, fetch its last commit with `done`,
/// as `git clone` asks. Expect: the push `200`; the fetch `200`, its packfile section a whole pack holding every object
/// pushed: a fetch is admitted what its repository's shard can serve, never one request's room.
#[test]
fn a_long_history_pushed_over_the_daemons_edge_is_fetched_whole_over_it() {
  let (identity, certificate) = identity();
  let daemon = daemon(identity);
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = name_on_the_repositorys_partition();
  create(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let edge = (port, &certificate);
  let (request, last) = long_history(LONG_HISTORY);
  let created = pushed(edge, &name, &request);
  let fetched = uploaded(edge, &name, &fetch_of(last));
  daemon.stop();
  assert_git_pushed(&created);
  assert_eq!(fetched.0, 200);
  let (_, commands_end) = packets_from(&request, 0);
  let pushed_pack = request.get(commands_end..).expect("the push's pack");
  let answer = String::from_utf8_lossy(
    fetched
      .1
      .get(..fetched.1.len().min(400))
      .unwrap_or_default(),
  )
  .into_owned();
  assert!(answer.starts_with("000dpackfile\n"), "{answer}");
  assert_eq!(
    ids_in(&fetched_pack(&fetched.1)).len(),
    ids_in(pushed_pack).len()
  );
}

/// Shape: the room a pack is read whole within here: far past the [`long_history`] push's objects.
const PACK_ROOM: u64 = 1 << 26;

/// The data packets of `bytes` from `at` to the first flush, each whole, and where that flush ends; bytes that end
/// before one fail the test.
pub(crate) fn packets_from(bytes: &[u8], mut at: usize) -> (Vec<Vec<u8>>, usize) {
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
pub(crate) fn ids_in(pack: &[u8]) -> Vec<String> {
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

/// The objects `pack` holds, read whole as git's index-pack reads one: each id with its bytes.
pub(crate) fn objects_of(pack: &[u8]) -> std::collections::BTreeMap<String, Vec<u8>> {
  let mut admission = Admission::new(PACK_ROOM);
  let unpacked = unpack::read_whole(pack, GitFormat::Sha1, &NoIndex, &mut admission);
  unpacked
    .expect("a whole pack")
    .objects
    .into_iter()
    .map(|object| (object.id.to_string(), object.content))
    .collect()
}

/// The pack a protocol v2 `fetch` answer carries: its `packfile` section's band-1 data, joined.
pub(crate) fn fetched_pack(answer: &[u8]) -> Vec<u8> {
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
/// `states` repository into its store there (A-675), a unit a turn to its end, and lands it (A-687): its phase roots and
/// markers recorded, so the store serves what 7.2's `served` filter serves of it ([`STATES_SERVED_HEADS`]).
fn create_hg_from_states(daemon: &Daemon, name: &str) {
  let (id, _) = crate::test_store::create_on(
    daemon,
    REPOSITORY,
    &owner(),
    (
      name,
      VcsTool::Mercurial,
      ObjectFormat::Sha1,
      "branch:default:0",
    ),
  );
  crate::daemon::observe_at(daemon, REPOSITORY, move |state| {
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
  });
  // A done import waits to land (A-687); landed from this thread, which waits, so the shard is never held through a
  // segment's commit; landed, its index is the store's and the store is lent again.
  assert_eq!(
    crate::vcs_store::hg_import::land_imported_on(daemon, REPOSITORY, id),
    Ok(())
  );
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
/// Format: stock 7.2's `heads` of the `states` repository's served view (slates-hg's
/// `fixtures/state/states-served-heads.txt`): its amended changeset hidden and its secret one left out, since the
/// import lands their phase roots and markers (A-687).
const STATES_SERVED_HEADS: &str = "b252f2a7c3d2e773bea91bf42aaa0acb39463176\n";

#[test]
fn a_mercurial_repositorys_store_is_served_over_the_daemons_edge() {
  let (identity, certificate) = identity();
  let daemon = daemon(identity);
  let port = daemon.vcs_http_port().expect("the edge started");
  let hg = hg_name_on_the_repositorys_partition();
  let git = name_on_the_repositorys_partition();
  create_hg_from_states(&daemon, &hg);
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
    (200, STATES_SERVED_HEADS.to_owned())
  );
  assert_eq!(git_as_hg, 404);
  assert!(
    matches!(git_advertisement, 200 | 503),
    "git's request answered {git_advertisement}"
  );
}

/// Format: stock 7.2's push over HTTP (`crates/hg/fixtures/wire`, `make-wire-fixtures.sh`'s `http-push`): the request
/// its client sent with `unbundle` (`fourth` onto `third`, published), the body its server answered, and its answer to
/// `listkeys phases` after it.
const HG_PUSH_REQUEST: &[u8] = include_bytes!("../../../../hg/fixtures/wire/http-push.07.request");
/// Format: see [`HG_PUSH_REQUEST`].
const HG_PUSH_REPLY: &[u8] =
  include_bytes!("../../../../hg/fixtures/wire/http-push.07.response-body");
/// Format: see [`HG_PUSH_REQUEST`].
const HG_PHASES_AFTER: &[u8] =
  include_bytes!("../../../../hg/fixtures/wire/http-push.08.response-body");
/// Format: the history the captured server held before that push, as `make-wire-fixtures.sh` made it with stock 7.2
/// (user `test`, date `0 0`): `base`, `second` and `third`, each adding one file to the one before.
const WIRE_HISTORY: [Commit; 3] = [
  ("base", "a", b"a\n", None, "default"),
  ("second", "b", b"b\n", Some(0), "default"),
  ("third", "c", b"c\n", Some(1), "default"),
];
/// Format: `third`, the captured server's head before the push (its `branchmap`), and `fourth`, the changeset the push
/// carried (its changegroup's).
const THIRD: &str = "d62662368ebecf4e7b3e39404f37e3b36d25a79e";
/// Format: see [`THIRD`].
const FOURTH: &str = "4658fce07b287c6ebe1bba3d950e5d0b3e978631";
/// Format: an empty bundle2 stream (`HG20`, no stream parameter, no part): the reply to a push that asked for none.
const EMPTY_REPLY: &[u8] = b"HG20\0\0\0\0\0\0\0\0";
/// Format: stock 7.2's `unbundle` fields over HTTP but its body's length and the credential (`http-push`'s request):
/// `heads` `force` (in hexadecimal) as a bundle2 push sends it.
const HG_PUSH_FIELDS: &str = "Accept-Encoding: identity\r\nContent-Type: application/mercurial-0.1\r\n\
  Vary: X-HgArg-1,X-HgProto-1\r\nX-HgArg-1: heads=666f726365\r\n\
  X-HgProto-1: 0.1 0.2 comp=zstd,zlib,none,bzip2 partial-pull\r\nAccept: application/mercurial-0.1\r\n";

/// Creates the empty Mercurial repository `name` on its partition, owned by [`owner`], as [`create`] creates a git one:
/// the shard is never held for the store's commit.
fn create_empty_hg(daemon: &Daemon, name: &str) {
  crate::test_store::create_on(
    daemon,
    REPOSITORY,
    &owner(),
    (
      name,
      VcsTool::Mercurial,
      ObjectFormat::Sha1,
      "branch:default:0",
    ),
  );
}

/// A bundle2 holding [`WIRE_HISTORY`] (a changegroup 03, every revision its full text) and `main` at `base`, asking no
/// reply: the captured server's state, pushed into an empty repository.
fn wire_history_bundle() -> Vec<u8> {
  let made = history_of(&WIRE_HISTORY);
  assert_eq!(
    crate::verbs::hex_of(&made[2].node),
    THIRD,
    "the history rebuilt is the captured server's"
  );
  history_bundle(&made, &[0, 1, 2])
}

/// A bundle2 as stock hg 7.2 pushes `made`'s changesets at `revs`: their changegroup (version 03), and bookmark `main`
/// at the first.
pub(crate) fn history_bundle(made: &[Made], revs: &[usize]) -> Vec<u8> {
  bundle_of(made, revs, Some(made[0].node))
}

/// [`history_bundle`] with bookmark `main` at `main` where given, and no bookmarks part where not.
fn bundle_of(made: &[Made], revs: &[usize], main: Option<slates_hg::Node>) -> Vec<u8> {
  let group = changegroup(made, revs);
  let mut marks = Vec::new();
  hg_bookmarks::encode(
    &[Bookmark {
      name: b"main",
      node: main,
    }],
    &mut marks,
  )
  .expect("a bookmarks payload");
  let part = |kind: &'static [u8], id: u32, params: Vec<Param<'static>>| Part {
    kind,
    id,
    mandatory_params: params.len(),
    params,
    depth: 0,
  };
  let mut parts = vec![part(
    b"CHANGEGROUP",
    0,
    vec![Param {
      key: b"version",
      value: b"03",
    }],
  )];
  let mut events = vec![Event::Open(0), Event::Chunk(&group.payload), Event::Close];
  if main.is_some() {
    parts.push(part(b"BOOKMARKS", 1, Vec::new()));
    events.extend([Event::Open(1), Event::Chunk(&marks), Event::Close]);
  }
  let bundle = Bundle2 {
    params: Vec::new(),
    parts,
    events,
  };
  let mut out = Vec::new();
  bundle.encode(&mut out).expect("a bundle2");
  out
}

/// Format: the first room a push's intake is given in
/// [`a_mercurial_push_past_its_first_admission_waits_for_room_and_lands_whole`]: far less than its changegroup's
/// framing, as a small learned demand left one on w-interop's runner.
const STARVED_ROOM: u64 = 64;

/// Main 01a20182's order of 2026-11-22 03:03 (w-interop's ubuntu find: stock 7.2's push of hg's history refused
/// `Delta { log: Manifests, .. PastAdmission { asked: 73365, left: 50646 } }`; the rule, A-857: an intake past its
/// admission waits for room, never refused). Do: start a two-shard daemon over loopback TLS; create a Mercurial
/// repository; push one changeset as stock hg posts it, its intake's first room [`STARVED_ROOM`] bytes
/// (`hg_push::TEST_FIRST_INTAKE_ROOM`); then ask `heads`. Expect the push `200` in `application/mercurial-0.1`, `heads`
/// the pushed changeset, and the wait counted (`vcs.hg.push_intake_room_waits`, `vcs_share.room_waits_taken`). Red
/// before: the push refused past its admission.
#[test]
fn a_mercurial_push_past_its_first_admission_waits_for_room_and_lands_whole() {
  let (identity, certificate) = identity();
  let (daemon, _instance) = daemon_named(Some(identity));
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = hg_name_on_the_repositorys_partition();
  create_empty_hg(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let edge = (port, &certificate);
  let made = history_of(&[(
    "starved",
    "starved",
    b"a push past its first room\n",
    None,
    "default",
  )]);
  crate::daemon::observe_at(&daemon, REPOSITORY, |_| {
    crate::repository::hg_push::TEST_FIRST_INTAKE_ROOM.with(|first| first.set(STARVED_ROOM));
  });
  let pushed = hg_unbundle(edge, &name, &history_bundle(&made, &[0]));
  let heads = hg_command(edge, &name, "heads");
  let counted = |name: &'static str| {
    (0..daemon.shards().len())
      .map(|partition| {
        crate::daemon::observe_at(&daemon, partition, move |state| {
          state.refusals.get(name).copied().unwrap_or(0)
        })
      })
      .sum::<u64>()
  };
  let waits = (
    counted(crate::repository::hg_push::INTAKE_ROOM_WAITS),
    counted(crate::vcs_share::wait::TAKEN),
  );
  daemon.stop();
  assert_eq!(
    (pushed.0, pushed.1.as_str()),
    (200, "application/mercurial-0.1"),
    "{}",
    String::from_utf8_lossy(&pushed.2)
  );
  let tip = crate::verbs::hex_of(&made[0].node);
  assert_eq!(
    heads,
    (200, format!("{tip}\n").into_bytes()),
    "{}",
    String::from_utf8_lossy(&pushed.2)
  );
  assert!(
    waits.0 > 0 && waits.1 > 0,
    "the intake waited for room and took it: {waits:?}"
  );
}

/// Shape: the push's first room in [`a_mercurial_push_whose_wait_is_past_its_repositorys_ceiling_is_refused_in_the_typed_words`]:
/// short of its begin's first charge (176 bytes), past the reply that answers its refusal (131), since the edge frames
/// the reply within the room the push was given.
const CEILING_ROOM: u64 = 160;

/// Main 01a20182's (ii) of 2026-11-22 05:01 (w-interop's whole-history push of hg on arm: its need passed its
/// repository's ceiling while it waited, and it was answered its step's bare `PastAdmission`, the wait's give-up past
/// the ceiling told as one past its patience). Do: start a two-shard daemon over loopback TLS; create a Mercurial
/// repository; push one changeset as stock hg posts it, its intake's first room [`CEILING_ROOM`] bytes
/// (`hg_push::TEST_FIRST_INTAKE_ROOM`) and its repository's ceiling one byte (`vcs_share::wait::TEST_CEILING`), so its
/// begin's wait is for more than it could ever be lent. Expect the push refused in the typed words that it could never
/// be taken (`hg_push::PAST_CEILING`), no wait taken (`vcs_share.room_waits_taken` zero). Red before: its step's bare
/// `PastAdmission` refusal.
#[test]
fn a_mercurial_push_whose_wait_is_past_its_repositorys_ceiling_is_refused_in_the_typed_words() {
  let (identity, certificate) = identity();
  let (daemon, _instance) = daemon_named(Some(identity));
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = hg_name_on_the_repositorys_partition();
  create_empty_hg(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let made = history_of(&[(
    "ceiling",
    "ceiling",
    b"a push past its repository's ceiling\n",
    None,
    "default",
  )]);
  crate::daemon::observe_at(&daemon, REPOSITORY, |_| {
    crate::repository::hg_push::TEST_FIRST_INTAKE_ROOM.with(|first| first.set(CEILING_ROOM));
    crate::vcs_share::wait::TEST_CEILING.with(|ceiling| ceiling.set(1));
  });
  let pushed = hg_unbundle((port, &certificate), &name, &history_bundle(&made, &[0]));
  let taken = (0..daemon.shards().len())
    .map(|partition| {
      crate::daemon::observe_at(&daemon, partition, |state| {
        state
          .refusals
          .get(crate::vcs_share::wait::TAKEN)
          .copied()
          .unwrap_or(0)
      })
    })
    .sum::<u64>();
  crate::daemon::observe_at(&daemon, REPOSITORY, |_| {
    crate::vcs_share::wait::TEST_CEILING.with(|ceiling| ceiling.set(0));
  });
  daemon.stop();
  let words = crate::repository::hg_push::PAST_CEILING;
  assert!(
    pushed.2.windows(words.len()).any(|at| at == words),
    "the push refused in the typed words past its ceiling: {}",
    String::from_utf8_lossy(&pushed.2)
  );
  assert_eq!(taken, 0, "no wait was taken");
}

/// Main 01a20182's order of 2026-11-22 03:35 (w-interop's ubuntu find: a chunked push of hg's history at revision 12,000
/// answered `push refused: the content store could not hold its increments; push again`). Do: start a two-shard daemon
/// over loopback TLS; create a Mercurial repository; push one changeset as stock hg posts it, its increments' first hold
/// refused as a full content store refuses it (`hg_push::TEST_HOLD_REFUSED`); then ask `heads`. Expect the push `200` in
/// `application/mercurial-0.1`, `heads` the pushed changeset, and the wait counted (`vcs.hg.push_held_ahead_room_waits`).
/// Red before: `push again` to a stock client.
#[test]
fn a_mercurial_push_its_store_cannot_hold_for_now_waits_for_room_and_lands_whole() {
  let (identity, certificate) = identity();
  let (daemon, _instance) = daemon_named(Some(identity));
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = hg_name_on_the_repositorys_partition();
  create_empty_hg(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let edge = (port, &certificate);
  let made = history_of(&[(
    "held",
    "held",
    b"a push the store holds once room comes\n",
    None,
    "default",
  )]);
  crate::daemon::observe_at(&daemon, REPOSITORY, |_| {
    crate::repository::hg_push::TEST_HOLD_REFUSED.with(|refused| refused.set(true));
  });
  let pushed = hg_unbundle(edge, &name, &history_bundle(&made, &[0]));
  let heads = hg_command(edge, &name, "heads");
  let waits = (0..daemon.shards().len())
    .map(|partition| {
      crate::daemon::observe_at(&daemon, partition, |state| {
        state
          .refusals
          .get(crate::repository::hg_push::HELD_AHEAD_ROOM_WAITS)
          .copied()
          .unwrap_or(0)
      })
    })
    .sum::<u64>();
  daemon.stop();
  assert_eq!(
    (pushed.0, pushed.1.as_str()),
    (200, "application/mercurial-0.1"),
    "{}",
    String::from_utf8_lossy(&pushed.2)
  );
  let tip = crate::verbs::hex_of(&made[0].node);
  assert_eq!(heads, (200, format!("{tip}\n").into_bytes()));
  assert!(waits > 0, "the push waited for the store's room");
}

/// A line of `changesets` changesets, each adding a file of its own, and the bundle stock hg posts to push them all.
fn a_line_of(changesets: usize) -> (Vec<Made>, Vec<u8>) {
  let paths: &'static [String] = Box::leak(
    (0..changesets)
      .map(|at| format!("file-{at}"))
      .collect::<Vec<_>>()
      .into_boxed_slice(),
  );
  let texts: &'static [Vec<u8>] = Box::leak(
    (0..changesets)
      .map(|at| format!("revision {at}\n").into_bytes())
      .collect::<Vec<_>>()
      .into_boxed_slice(),
  );
  let commits: Vec<Commit> = (0..changesets)
    .map(|at| {
      (
        paths[at].as_str(),
        paths[at].as_str(),
        texts[at].as_slice(),
        at.checked_sub(1),
        "default",
      )
    })
    .collect();
  let made = history_of(&commits);
  let bundle = history_bundle(&made, &(0..changesets).collect::<Vec<_>>());
  (made, bundle)
}

/// Format: the room left to the steps of [`a_mercurial_push_is_admitted_its_known_need_and_never_waits_for_its_own_keeping`]'s
/// push once its begin is taken: far less than what its changesets keep.
const STEP_ROOM: u64 = 64;

/// Main 01a20182's ruling (a) of 2026-11-22 04:23 (w-interop's whole-history push of hg on ubuntu and arm refused at a
/// manifest delta: the push's own keeping spent the one room it held, and a wait for room no one else held could never
/// end). Do: start a two-shard daemon over loopback TLS; create a Mercurial repository; push a line of
/// [`FRESH_CHANGESETS`] changesets as stock hg posts it, the room left to its steps after its begin [`STEP_ROOM`] bytes
/// (`hg_push::TEST_STEP_ROOM`); then ask `heads`. Expect the push `200`, `heads` the line's last changeset, the push
/// admitted its known need once (`vcs.hg.push_need_admitted`), and no step waiting for room
/// (`vcs.hg.push_intake_room_waits` zero). Red before: its steps waited for room the push already held.
#[test]
fn a_mercurial_push_is_admitted_its_known_need_and_never_waits_for_its_own_keeping() {
  let (identity, certificate) = identity();
  let (daemon, _instance) = daemon_named(Some(identity));
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = hg_name_on_the_repositorys_partition();
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let (made, bundle) = a_line_of(FRESH_CHANGESETS);
  create_empty_hg(&daemon, &name);
  crate::daemon::observe_at(&daemon, REPOSITORY, |_| {
    crate::repository::hg_push::TEST_STEP_ROOM.with(|left| left.set(STEP_ROOM));
  });
  let pushed = hg_unbundle((port, &certificate), &name, &bundle);
  let heads = hg_command((port, &certificate), &name, "heads");
  let counted = |counter: &'static str| {
    (0..daemon.shards().len())
      .map(|partition| {
        crate::daemon::observe_at(&daemon, partition, move |state| {
          state.refusals.get(counter).copied().unwrap_or(0)
        })
      })
      .sum::<u64>()
  };
  let (admitted, waited) = (
    counted(crate::repository::hg_push::PUSH_NEED_ADMITTED),
    counted(crate::repository::hg_push::INTAKE_ROOM_WAITS),
  );
  daemon.stop();
  assert_eq!(
    (pushed.0, pushed.1.as_str()),
    (200, "application/mercurial-0.1"),
    "{}",
    String::from_utf8_lossy(&pushed.2)
  );
  let tip = crate::verbs::hex_of(&made[FRESH_CHANGESETS - 1].node);
  assert_eq!(heads, (200, format!("{tip}\n").into_bytes()));
  assert_eq!(
    (admitted, waited),
    (1, 0),
    "its need admitted once, no step waiting for its own keeping"
  );
}

/// Format: the changesets of [`a_fresh_repositorys_first_push_of_hundreds_of_changesets_is_taken_at_once`]'s push, as
/// w-interop's 500-revision chunks were.
const FRESH_CHANGESETS: usize = 500;

/// Main 01a20182's order of 2026-11-22 03:50 (w-interop: a fresh repository's first 500-revision chunk refused at once,
/// `the content store could not hold its increments; push again`, the daemon at 64 to 88 MiB on x86 and arm). Do: start
/// a two-shard daemon over loopback TLS; create a Mercurial repository and, with no gap, push a line of
/// [`FRESH_CHANGESETS`] changesets as stock hg posts it; then ask `heads`. Expect the push `200` in
/// `application/mercurial-0.1` and `heads` the line's last changeset: a first push is taken whole, never told to push
/// again.
#[test]
fn a_fresh_repositorys_first_push_of_hundreds_of_changesets_is_taken_at_once() {
  let (identity, certificate) = identity();
  let (daemon, _instance) = daemon_named(Some(identity));
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = hg_name_on_the_repositorys_partition();
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let (made, bundle) = a_line_of(FRESH_CHANGESETS);
  create_empty_hg(&daemon, &name);
  let pushed = hg_unbundle((port, &certificate), &name, &bundle);
  let heads = hg_command((port, &certificate), &name, "heads");
  daemon.stop();
  assert_eq!(
    (pushed.0, pushed.1.as_str()),
    (200, "application/mercurial-0.1"),
    "{}",
    String::from_utf8_lossy(&pushed.2)
  );
  let tip = crate::verbs::hex_of(&made[FRESH_CHANGESETS - 1].node);
  assert_eq!(
    heads,
    (200, format!("{tip}\n").into_bytes()),
    "{}",
    String::from_utf8_lossy(&pushed.2)
  );
}

/// A-711 (found by use, 2026-11-20: stock hg 7.2's push of Mercurial's own repository, 57,036 changesets in an 84 MiB
/// store, answered `abort: HTTP Error 413: Content Too Large` after 7.5 s; hg's twin of A-704). Do: start a two-shard
/// daemon whose edge serves loopback TLS, each request admitted one ring region (the edge's `Bounds::room`); create a
/// Mercurial repository and push, as stock hg posts it, one changeset whose one file is twice that room, so the bundle
/// and what reading it builds are each past it; then ask `heads`. Expect the push `200` in `application/mercurial-0.1`
/// and `heads` the pushed changeset: a push is bounded by what its repository's shard can hold, never by one request's
/// room.
#[test]
fn a_mercurial_push_larger_than_one_ring_region_lands_through_the_daemons_edge() {
  let (identity, certificate) = identity();
  let (daemon, instance) = daemon_named(Some(identity));
  let room = super::bounds_of(&crate::DaemonConfig::derive(
    &crate::daemon::test_profile(),
    &instance,
    Some(SHARDS),
  ))
  .room;
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = hg_name_on_the_repositorys_partition();
  create_empty_hg(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let edge = (port, &certificate);
  let mut state = 0x9e37_79b9_7f4a_7c15u64;
  let large: Vec<u8> = (0..usize::try_from(room * 2).expect("a size"))
    .map(|_| {
      state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
      state.to_be_bytes()[0]
    })
    .collect();
  let large: &'static [u8] = Box::leak(large.into_boxed_slice());
  let made = history_of(&[("large", "large", large, None, "default")]);
  let pushed = hg_unbundle(edge, &name, &history_bundle(&made, &[0]));
  let heads = hg_command(edge, &name, "heads");
  daemon.stop();
  assert_eq!(
    (pushed.0, pushed.1.as_str()),
    (200, "application/mercurial-0.1"),
    "{}",
    String::from_utf8_lossy(&pushed.2)
  );
  let tip = crate::verbs::hex_of(&made[0].node);
  assert_eq!(heads, (200, format!("{tip}\n").into_bytes()));
}

/// A-745. A fetch's answer (§4.17.1, main's ruling 2026-11-20: a clone's working set is bounded whatever the repository's size;
/// found by use, w-interop: every clone of Mercurial's own history, 57,035 revisions, was refused `PastAdmission`, its
/// answer built whole within one request's room). Do: start a two-shard daemon whose edge serves loopback TLS, each
/// request admitted one ring region; push, as stock hg posts it, one changeset whose one file is twice that room; then
/// clone it as stock 7.2 asks (`getbundle` of its head, nothing common, bundle2 with changegroup 02 and 03, the answer
/// uncompressed). Expect `200`, and the answer's changegroup carrying the file's bytes whole: an answer is made a slice
/// at a time from what is stored, each slice admitted, never the whole answer within one request's room.
#[test]
fn a_mercurial_clone_larger_than_one_ring_region_is_answered_through_the_daemons_edge() {
  let (identity, certificate) = identity();
  let (daemon, instance) = daemon_named(Some(identity));
  let room = super::bounds_of(&crate::DaemonConfig::derive(
    &crate::daemon::test_profile(),
    &instance,
    Some(SHARDS),
  ))
  .room;
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = hg_name_on_the_repositorys_partition();
  create_empty_hg(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let edge = (port, &certificate);
  let mut state = 0x2545_f491_4f6c_dd1du64;
  let large: Vec<u8> = (0..usize::try_from(room * 2).expect("a size"))
    .map(|_| {
      state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
      state.to_be_bytes()[0]
    })
    .collect();
  let large: &'static [u8] = Box::leak(large.into_boxed_slice());
  let made = history_of(&[("large", "large", large, None, "default")]);
  let pushed = hg_unbundle(edge, &name, &history_bundle(&made, &[0]));
  let tip = crate::verbs::hex_of(&made[0].node);
  let cloned = hg_getbundle(edge, &name, &tip);
  daemon.stop();
  assert_hg_answered(&pushed);
  assert_eq!(
    (cloned.0, cloned.1.as_str()),
    (200, "application/mercurial-0.2"),
    "{}",
    String::from_utf8_lossy(&cloned.2)
  );
  let stream = cloned
    .2
    .strip_prefix(b"\x04none")
    .expect("an uncompressed answer");
  let bundle =
    Bundle2::decode(stream, &mut slates_hg::Admission::new(u64::MAX)).expect("a bundle2");
  let (index, _) = bundle
    .parts
    .iter()
    .enumerate()
    .find(|(_, part)| part.kind.eq_ignore_ascii_case(b"changegroup"))
    .unwrap_or_else(|| panic!("no changegroup part: {}", String::from_utf8_lossy(stream)));
  let payload = bundle
    .payload_bytes(index, &mut slates_hg::Admission::new(u64::MAX))
    .expect("its payload");
  let found = payload
    .windows(64)
    .position(|window| window == &large[..64])
    .expect("the file's bytes");
  assert_eq!(payload.get(found..found + large.len()), Some(large));
}

/// A-745 (§4.17.1 "A fetch's answer"; found by use, w-interop: `clone --stream` of Mercurial's own history refused
/// `PastAdmission`). Do: start a two-shard daemon whose edge serves loopback TLS, each request admitted one ring region;
/// push one changeset whose one file is twice that room; then ask `stream_out` as `hg clone --stream` asks it. Expect
/// `200` in `application/mercurial-0.1`, the answer `0` and its counts, and the file's revlog carrying its bytes whole:
/// the revlogs sent a row at a time, never rendered whole within one request's room.
#[test]
fn a_mercurial_stream_clone_larger_than_one_ring_region_is_answered_through_the_daemons_edge() {
  let (identity, certificate) = identity();
  let (daemon, instance) = daemon_named(Some(identity));
  let room = super::bounds_of(&crate::DaemonConfig::derive(
    &crate::daemon::test_profile(),
    &instance,
    Some(SHARDS),
  ))
  .room;
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = hg_name_on_the_repositorys_partition();
  create_empty_hg(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let edge = (port, &certificate);
  let mut state = 0x6a09_e667_f3bc_c908u64;
  let large: Vec<u8> = (0..usize::try_from(room * 2).expect("a size"))
    .map(|_| {
      state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
      state.to_be_bytes()[0] | 1
    })
    .collect();
  let large: &'static [u8] = Box::leak(large.into_boxed_slice());
  let made = history_of(&[("large", "large", large, None, "default")]);
  let pushed = hg_unbundle(edge, &name, &history_bundle(&made, &[0]));
  let (port, certificate) = edge;
  let request = format!(
    "GET /{name}?cmd=stream_out HTTP/1.1\r\nHost: localhost\r\nAccept: application/mercurial-0.1\r\n{}\r\n\r\n",
    owner_bearer()
  );
  let streamed = typed_exchange(port, certificate, request.as_bytes());
  daemon.stop();
  assert_hg_answered(&pushed);
  assert_eq!(
    (streamed.0, streamed.1.as_str()),
    (200, "application/mercurial-0.1"),
    "{}",
    String::from_utf8_lossy(&streamed.2[..streamed.2.len().min(200)])
  );
  assert!(
    streamed.2.starts_with(b"0\n"),
    "{:?}",
    &streamed.2[..streamed.2.len().min(40)]
  );
  let found = streamed
    .2
    .windows(64)
    .position(|window| window == &large[..64])
    .expect("the file's bytes");
  assert_eq!(streamed.2.get(found..found + large.len()), Some(large));
}

/// The answer to a clone of Mercurial repository `name` at `head` by its owner, as stock hg 7.2 asks one over HTTP
/// (`getbundle` with bundle2's changegroup 02 and 03, nothing common, the answer uncompressed): its status,
/// `Content-Type` and body.
fn hg_getbundle(
  edge: (u16, &CertificateDer<'static>),
  name: &str,
  head: &str,
) -> (u16, String, Vec<u8>) {
  hg_getbundle_over(edge, name, (&"0".repeat(40), head))
}

/// [`hg_getbundle`] over `common` (a pull's: the client holds it), for `heads` (`+` between several).
fn hg_getbundle_over(
  edge: (u16, &CertificateDer<'static>),
  name: &str,
  (common, head): (&str, &str),
) -> (u16, String, Vec<u8>) {
  let (port, certificate) = edge;
  let arguments = format!(
    "bundlecaps=HG20%2Cbundle2%3DHG20%250Achangegroup%253D02%252C03&cg=1&common={common}&heads={head}"
  );
  let sent = format!(
    "GET /{name}?cmd=getbundle HTTP/1.1\r\nHost: localhost\r\n{}\r\nAccept: application/mercurial-0.1\r\n\
     X-HgArg-1: {arguments}\r\nX-HgProto-1: 0.1 0.2 comp=none\r\n\r\n",
    owner_bearer()
  );
  typed_exchange(port, certificate, sent.as_bytes())
}

/// `bundle` pushed to Mercurial repository `name` by its owner as stock hg 7.2 pushes over HTTP: the answer's status,
/// `Content-Type` and body.
fn hg_unbundle(
  edge: (u16, &CertificateDer<'static>),
  name: &str,
  bundle: &[u8],
) -> (u16, String, Vec<u8>) {
  hg_unbundle_within(edge, name, bundle, Duration::from_nanos(patience_ns()))
}

/// [`hg_unbundle`], its client waiting at most `patience` for each read of the answer.
fn hg_unbundle_within(
  edge: (u16, &CertificateDer<'static>),
  name: &str,
  bundle: &[u8],
  patience: Duration,
) -> (u16, String, Vec<u8>) {
  let (port, certificate) = edge;
  let mut sent = format!(
    "POST /{name}?cmd=unbundle HTTP/1.1\r\nHost: localhost\r\n{}\r\n{HG_PUSH_FIELDS}\
     Content-Length: {}\r\n\r\n",
    owner_bearer(),
    bundle.len()
  )
  .into_bytes();
  sent.extend_from_slice(bundle);
  typed_exchange_within(port, certificate, &sent, patience)
}

/// A bundle2 part's parameters: each its name and value.
type PartParameters = Vec<(Vec<u8>, Vec<u8>)>;

/// The parts of the bundle2 `stream`, each its type and parameters, its `output` parts left out unless `outputs`: what
/// 7.2's server prints while it takes a push, which it captures there over HTTP (`slates_hg::serve::http`'s module doc).
fn reply_parts(stream: &[u8], outputs: bool) -> Vec<(Vec<u8>, PartParameters)> {
  let bundle =
    Bundle2::decode(stream, &mut slates_hg::Admission::new(u64::MAX)).expect("a bundle2");
  bundle
    .parts
    .iter()
    .filter(|part| outputs || !part.kind.eq_ignore_ascii_case(b"output"))
    .map(|part| {
      let params = part
        .params
        .iter()
        .map(|param| (param.key.to_vec(), param.value.to_vec()))
        .collect();
      (part.kind.to_vec(), params)
    })
    .collect()
}

/// A plain connection to the edge on loopback `port`, its reads waiting [`patience_ns`].
fn cleartext_client(port: u16) -> ClientStream {
  let socket = ClientStream::connect(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
    .expect("the edge's listener takes the connection");
  socket
    .set_read_timeout(Some(Duration::from_nanos(patience_ns())))
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

/// A-680 part 2b (main's ruling, 2026-11-19: a stock `hg push` over HTTP lands on its repository's owner shard; golden:
/// stock 7.2's push over HTTP, `crates/hg/fixtures/wire`'s `http-push`). Do: start a two-shard daemon whose edge serves
/// loopback TLS; create an empty Mercurial repository on the second shard's partition and enroll its owner's token;
/// then, each on a new connection with the owner's bearer token and stock hg's fields (`POST ?cmd=unbundle`, `heads`
/// `force`): push the captured server's history (`base`, `second`, `third`; `main` at `base`), then the very bundle
/// stock 7.2's client pushed to that server (`fourth` onto `third`, with its race checks); then ask `heads` and
/// `listkeys phases`. Expect: both pushes `200` in `application/mercurial-0.1`, the first an empty reply (it asked for
/// none), the second 7.2's own reply but its progress words (`reply:changegroup` naming the changegroup part, the heads
/// one as before); `heads` `fourth` alone, the served view of the repository's store, so the push's revisions are kept
/// there; `listkeys phases` 7.2's own answer after the push, byte for byte.
#[test]
fn a_stock_mercurial_push_over_the_daemons_edge_lands_on_its_repositorys_shard() {
  let (identity, certificate) = identity();
  let daemon = daemon(identity);
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = hg_name_on_the_repositorys_partition();
  create_empty_hg(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let edge = (port, &certificate);
  let history = hg_unbundle(edge, &name, &wire_history_bundle());
  let body_at = HG_PUSH_REQUEST
    .windows(4)
    .position(|four| four == b"\r\n\r\n")
    .expect("the request's head ends")
    + 4;
  let stock = hg_unbundle(edge, &name, &HG_PUSH_REQUEST[body_at..]);
  let heads = hg_command(edge, &name, "heads");
  let phases = hg_command(edge, &name, "listkeys&namespace=phases");
  daemon.stop();
  let mercurial = "application/mercurial-0.1".to_owned();
  assert_eq!(
    (history.0, history.1.as_str(), history.2.as_slice()),
    (200, mercurial.as_str(), EMPTY_REPLY),
    "{}",
    String::from_utf8_lossy(&history.2)
  );
  assert_eq!(
    (stock.0, stock.1.as_str()),
    (200, mercurial.as_str()),
    "{}",
    String::from_utf8_lossy(&stock.2)
  );
  assert_eq!(
    reply_parts(&stock.2, true),
    reply_parts(HG_PUSH_REPLY, false)
  );
  assert_eq!(heads, (200, format!("{FOURTH}\n").into_bytes()));
  assert_eq!(phases, (200, HG_PHASES_AFTER.to_vec()));
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
/// words (`HGPLAIN`), with no terminal to prompt on and each of its socket's waits bounded by [`patience_ns`]
/// (`http.timeout`): its exit code and both outputs.
fn hg_run(words: &[String], args: &[&str]) -> (Option<i32>, String, String) {
  let (program, leading) = words.split_first().expect("a command");
  let timeout = format!(
    "http.timeout={}",
    Duration::from_nanos(patience_ns()).as_secs_f64()
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
  let daemon = daemon_named(None).0;
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

/// A Subversion repository's name that routes to [`REPOSITORY`]'s partition.
#[cfg(unix)]
fn svn_name_on_the_repositorys_partition() -> String {
  (0u32..)
    .map(|serial| format!("svn-hooked-{serial}"))
    .find(|name| usize::from(crate::verbs::owner_of_name(name, usize::from(SHARDS))) == REPOSITORY)
    .expect("a name routes there")
}

/// Creates the Subversion repository `name` on [`REPOSITORY`]'s partition, owned by [`owner`], its policy running
/// `hooks` (each a point and its program).
#[cfg(unix)]
fn create_svn_hooked(
  daemon: &Daemon,
  name: &str,
  hooks: Vec<(slates_ipc::protocol::HookPointName, String)>,
) {
  // A hook is shown its repository beneath the root mount the daemon published (A-645 P3).
  daemon
    .record_root_mount(std::path::Path::new(crate::vcs_svn::tests::TEST_ROOT))
    .expect("the root mount recorded");
  let asked = (name, VcsTool::Subversion, ObjectFormat::Revisions, "trunk");
  crate::test_store::create_on(daemon, REPOSITORY, &owner(), asked);
  let name = name.to_owned();
  let set = crate::daemon::observe_at(daemon, REPOSITORY, move |state| {
    crate::verbs::dispatch(
      state,
      2,
      &owner(),
      RequestBody::SetRepositoryPolicy {
        repository: name.clone(),
        read: 0,
        rules: Vec::new(),
        hooks: hooks
          .iter()
          .map(|(point, program)| slates_ipc::protocol::HookConfig {
            point: *point,
            name: svn_point_name(*point).to_owned(),
            program: program.clone(),
          })
          .collect(),
        hooks_env: Vec::new(),
        signers: Vec::new(),
        hg_publishing: true,
      },
    )
  });
  assert_eq!(set, ReplyBody::Acknowledged);
}

/// The name Subversion gives a hook at `point`.
#[cfg(unix)]
fn svn_point_name(point: slates_ipc::protocol::HookPointName) -> &'static str {
  use slates_ipc::protocol::HookPointName as Point;
  match point {
    Point::SvnStartCommit => "start-commit",
    Point::SvnPreCommit => "pre-commit",
    Point::SvnPostCommit => "post-commit",
    Point::SvnPreLock => "pre-lock",
    Point::SvnPostLock => "post-lock",
    Point::SvnPreUnlock => "pre-unlock",
    Point::SvnPostUnlock => "post-unlock",
    Point::SvnPreRevpropChange => "pre-revprop-change",
    Point::SvnPostRevpropChange => "post-revprop-change",
    _ => "other",
  }
}

/// The status, head and body of the owner's DAV request `method target` with `fields` and `body`, on a new TLS
/// connection to the edge.
#[cfg(unix)]
fn dav(
  edge: (u16, &CertificateDer<'static>),
  (method, target): (&str, &str),
  fields: &[&str],
  body: &[u8],
) -> (u16, String, String) {
  let (port, certificate) = edge;
  let mut request = format!(
    "{method} {target} HTTP/1.1\r\nHost: localhost\r\n{}\r\nContent-Length: {}\r\n",
    owner_bearer(),
    body.len()
  );
  for field in fields {
    request.push_str(field);
    request.push_str("\r\n");
  }
  request.push_str("\r\n");
  let mut sent = request.into_bytes();
  sent.extend_from_slice(body);
  let mut stream = tls_client(port, certificate);
  stream.write_all(&sent).expect("the request is sent");
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
  let body = body_of(&mut stream, &head);
  (status, head, String::from_utf8_lossy(&body).into_owned())
}

/// The value of field `name` in an answer's `head`.
#[cfg(unix)]
fn field(head: &str, name: &str) -> Option<String> {
  head.lines().find_map(|line| {
    let (named, value) = line.split_once(':')?;
    named
      .eq_ignore_ascii_case(name)
      .then(|| value.trim().to_owned())
  })
}

/// A file's whole text as svndiff 0, as a PUT of an added file sends it: the header, then one window of new data alone.
#[cfg(unix)]
fn fulltext_delta(text: &[u8]) -> Vec<u8> {
  let length = u8::try_from(text.len()).expect("a text under 64 bytes");
  let mut delta = b"SVN\0".to_vec();
  // A window: source offset 0, source length 0, target length, instructions' length 1, new data's length; one
  // instruction, new data (`0b10` in its top two bits) of the whole length.
  delta.extend_from_slice(&[0, 0, length, 1, length, 0x80 | length]);
  delta.extend_from_slice(text);
  delta
}

/// A DAV commit of repository `name` adding `f.txt` with `text`, as stock svn sends it: the POST, the PUT and the
/// MERGE; each answer's status, and the MERGE's body.
#[cfg(unix)]
fn dav_commit(
  edge: (u16, &CertificateDer<'static>),
  name: &str,
  text: &[u8],
) -> (u16, u16, u16, String) {
  let posted = dav(
    edge,
    ("POST", &format!("/{name}/!svn/me")),
    &["Content-Type: application/vnd.svn-skel"],
    b"( create-txn )",
  );
  let Some(transaction) = field(&posted.1, "SVN-Txn-Name") else {
    return (posted.0, 0, 0, posted.2);
  };
  let put = dav(
    edge,
    ("PUT", &format!("/{name}/!svn/txr/{transaction}/f.txt")),
    &["Content-Type: application/vnd.svn-svndiff"],
    &fulltext_delta(text),
  );
  let merge = format!(
    "<?xml version=\"1.0\" encoding=\"utf-8\"?><D:merge xmlns:D=\"DAV:\"><D:source><D:href>/{name}/!svn/txn/\
     {transaction}</D:href></D:source><D:no-auto-merge/><D:no-checkout/><D:prop><D:checked-in/><D:version-name/>\
     </D:prop></D:merge>"
  );
  let merged = dav(
    edge,
    ("MERGE", &format!("/{name}")),
    &["Content-Type: text/xml"],
    merge.as_bytes(),
  );
  (posted.0, put.0, merged.0, merged.2)
}

/// A-733 (`mod_dav_svn` runs start-commit as the POST makes the transaction, `check_hook_result`'s words). Do: on a
/// daemon's HTTP edge, a Subversion repository whose policy runs a start-commit hook that writes to standard error and
/// exits 1; POST a transaction as stock svn does. Expect: the POST refused, its error in `svnserve`'s words "Commit
/// blocked by start-commit hook (exit code 1) with output:" and the hook's standard error. Before A-733 the site refused
/// it in its own words ("does not run this repository's hooks yet"), the hook never run.
#[cfg(unix)]
#[test]
fn a_dav_posts_start_commit_hook_that_refuses_refuses_the_transaction_in_svnserves_words() {
  let dir = crate::git_hooks::tests::ScratchDir::new();
  let refusing =
    crate::vcs_svn::tests::hook_script(&dir.path, "start-commit", "echo 'not today' >&2\nexit 1");
  let (identity, certificate) = identity();
  let daemon = daemon(identity);
  let port = daemon.vcs_http_port().expect("the edge serves");
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let name = svn_name_on_the_repositorys_partition();
  create_svn_hooked(
    &daemon,
    &name,
    vec![(
      slates_ipc::protocol::HookPointName::SvnStartCommit,
      refusing,
    )],
  );
  let (status, _, body) = dav(
    (port, &certificate),
    ("POST", &format!("/{name}/!svn/me")),
    &["Content-Type: application/vnd.svn-skel"],
    b"( create-txn )",
  );
  daemon.stop();
  assert!(
    status >= 400
      && body.contains("Commit blocked by start-commit hook (exit code 1) with output:\nnot today"),
    "{status}: {body}"
  );
}

/// A-733 (`mod_dav_svn` runs pre-commit at the MERGE before the verdict and post-commit once the revision stands, its
/// failure the MERGE response's `S:post-commit-err`, `merge.c`). Do: a repository whose policy runs start-commit and
/// pre-commit hooks that record their arguments and accept, and a post-commit hook that writes to standard error and
/// exits 1; commit a file over DAV as stock svn does (POST, PUT, MERGE). Expect: the POST `201`, the PUT `201`, the MERGE
/// `200` naming revision 1 with post-commit's words in `S:post-commit-err`; start-commit given REPOS, the user,
/// `mergeinfo` and the transaction; pre-commit REPOS and the same transaction.
#[cfg(unix)]
#[test]
fn a_dav_commit_runs_its_repositorys_commit_hooks_as_mod_dav_svn_runs_them() {
  use slates_ipc::protocol::HookPointName;
  let dir = crate::git_hooks::tests::ScratchDir::new();
  let at = dir.path.display().to_string();
  let start = crate::vcs_svn::tests::hook_script(
    &dir.path,
    "start-commit",
    &format!("for arg in \"$@\"; do echo \"$arg\"; done > {at}/start.args"),
  );
  let pre = crate::vcs_svn::tests::hook_script(
    &dir.path,
    "pre-commit",
    &format!("for arg in \"$@\"; do echo \"$arg\"; done > {at}/pre.args"),
  );
  let post =
    crate::vcs_svn::tests::hook_script(&dir.path, "post-commit", "echo 'post said no' >&2\nexit 1");
  let (identity, certificate) = identity();
  let daemon = daemon(identity);
  let port = daemon.vcs_http_port().expect("the edge serves");
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let name = svn_name_on_the_repositorys_partition();
  create_svn_hooked(
    &daemon,
    &name,
    vec![
      (HookPointName::SvnStartCommit, start),
      (HookPointName::SvnPreCommit, pre),
      (HookPointName::SvnPostCommit, post),
    ],
  );
  let (posted, put, merged, body) = dav_commit((port, &certificate), &name, b"hello\n");
  daemon.stop();
  #[allow(clippy::disallowed_methods)]
  // test code: what the hooks recorded in its own scratch directory
  let lines = |file: &str| -> Vec<String> {
    std::fs::read_to_string(dir.path.join(file))
      .unwrap_or_default()
      .lines()
      .map(str::to_owned)
      .collect()
  };
  let (started, precommitted) = (lines("start.args"), lines("pre.args"));
  let repos = format!("{}/{name}", crate::vcs_svn::tests::TEST_ROOT);
  assert_eq!(
    (
      (posted, put, merged),
      started.first(),
      started.get(2).map(String::as_str),
      started.len(),
      precommitted.first(),
      precommitted.get(1) == started.get(3),
    ),
    (
      (201, 201, 200),
      Some(&repos),
      Some("mergeinfo"),
      4,
      Some(&repos),
      true
    ),
    "start-commit {started:?}, pre-commit {precommitted:?}: {body}"
  );
  assert!(
    body.contains("<S:post-commit-err>") && body.contains("post said no"),
    "{body}"
  );
}

/// A PROPPATCH body setting `svn:log` to `value` as ra_serf writes it (`commit.c` `create_proppatch_body`,
/// `write_prop_xml`), with `V:old-value` naming `expected` where it is given.
#[cfg(unix)]
fn log_proppatch(value: &str, expected: Option<&str>) -> Vec<u8> {
  let old = expected.map_or_else(String::new, |old| {
    format!("<V:old-value>{old}</V:old-value>")
  });
  format!(
    "<?xml version=\"1.0\" encoding=\"utf-8\"?><D:propertyupdate xmlns:D=\"DAV:\" \
     xmlns:V=\"http://subversion.tigris.org/xmlns/dav/\" xmlns:C=\"http://subversion.tigris.org/xmlns/custom/\" \
     xmlns:S=\"http://subversion.tigris.org/xmlns/svn/\"><D:set><D:prop><S:log>{old}{value}</S:log></D:prop>\
     </D:set></D:propertyupdate>"
  )
  .into_bytes()
}

/// main 01a20182's red of 2026-11-21 23:24 (run r6, stock svn 1.15 over HTTPv2: `svn propset --revprop --revision 3
/// svn:log ...` refused, the edge serving no PROPPATCH on `!svn/rev/N`; `deadprops.c` `save_value` makes it through
/// `svn_repos_fs_change_rev_prop4`). Do: on a daemon's HTTP edge, a repository whose policy runs pre- and
/// post-revprop-change hooks that record their arguments and standard input and accept; commit revision 1 over DAV;
/// PROPPATCH `!svn/rev/1` as ra_serf does: `svn:log` set with no expectation, then set again expecting the first value,
/// then set expecting that first value once more. Expect: the first two `207`, the second showing the first landed; the
/// third refused "revprop 'svn:log' has unexpected value in filesystem" (FSFS's base-value check); pre-revprop-change
/// given REPOS, the revision, the user, the name and `M`, the new value on its standard input; post-revprop-change the
/// old value.
#[cfg(unix)]
#[test]
fn a_dav_proppatch_of_a_revision_changes_its_property_through_its_revprop_hooks() {
  use slates_ipc::protocol::HookPointName;
  let dir = crate::git_hooks::tests::ScratchDir::new();
  let at = dir.path.display().to_string();
  let pre = crate::vcs_svn::tests::hook_script(
    &dir.path,
    "pre-revprop-change",
    &format!("for arg in \"$@\"; do echo \"$arg\"; done > {at}/pre.args\ncat > {at}/pre.in"),
  );
  let post = crate::vcs_svn::tests::hook_script(
    &dir.path,
    "post-revprop-change",
    &format!("cat > {at}/post.in"),
  );
  let (identity, certificate) = identity();
  let daemon = daemon(identity);
  let port = daemon.vcs_http_port().expect("the edge serves");
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let name = svn_name_on_the_repositorys_partition();
  create_svn_hooked(
    &daemon,
    &name,
    vec![
      (HookPointName::SvnPreRevpropChange, pre),
      (HookPointName::SvnPostRevpropChange, post),
    ],
  );
  let (_, _, merged, _) = dav_commit((port, &certificate), &name, b"hello\n");
  let revision = format!("/{name}/!svn/rev/1");
  let patch = |body: Vec<u8>| {
    dav(
      (port, &certificate),
      ("PROPPATCH", &revision),
      &["Content-Type: text/xml"],
      &body,
    )
  };
  let first = patch(log_proppatch("an edit", None));
  let second = patch(log_proppatch("a second edit", Some("an edit")));
  let stale = patch(log_proppatch("stale", Some("an edit")));
  daemon.stop();
  #[allow(clippy::disallowed_methods)]
  // test code: what the hooks recorded in its own scratch directory
  let read = |file: &str| std::fs::read_to_string(dir.path.join(file)).unwrap_or_default();
  let repos = format!("{}/{name}", crate::vcs_svn::tests::TEST_ROOT);
  let args: Vec<String> = read("pre.args").lines().map(str::to_owned).collect();
  assert_eq!(
    (merged, first.0, second.0),
    (200, 207, 207),
    "first: {}\nsecond: {}",
    first.2,
    second.2
  );
  assert!(
    stale.0 >= 400
      && stale
        .2
        .contains("revprop 'svn:log' has unexpected value in filesystem"),
    "{}: {}",
    stale.0,
    stale.2
  );
  assert_eq!(
    (
      args.first().map(String::as_str),
      args.get(1).map(String::as_str),
      args.get(3).map(String::as_str),
      args.get(4).map(String::as_str),
      read("pre.in"),
      read("post.in"),
    ),
    (
      Some(repos.as_str()),
      Some("1"),
      Some("svn:log"),
      Some("M"),
      "stale".to_owned(),
      "an edit".to_owned(),
    ),
    "pre-revprop-change {args:?}"
  );
}

/// A-733 (`mod_dav_svn`'s LOCK runs pre-lock through `svn_repos_fs_lock`, its refusal "Failed to create new lock." with
/// the hook's error, `lock.c`). Do: commit a file over DAV, then LOCK it as stock svn does, through a repository whose
/// policy runs a pre-lock hook that writes to standard error and exits 1. Expect: the LOCK refused, its error in
/// `svnserve`'s words "Lock blocked by pre-lock hook (exit code 1) with output:" and the hook's standard error.
#[cfg(unix)]
#[test]
fn a_dav_lock_runs_pre_lock_and_its_refusal_answers_the_lock() {
  let dir = crate::git_hooks::tests::ScratchDir::new();
  let refusing =
    crate::vcs_svn::tests::hook_script(&dir.path, "pre-lock", "echo 'no locks here' >&2\nexit 1");
  let (identity, certificate) = identity();
  let daemon = daemon(identity);
  let port = daemon.vcs_http_port().expect("the edge serves");
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let name = svn_name_on_the_repositorys_partition();
  create_svn_hooked(
    &daemon,
    &name,
    vec![(slates_ipc::protocol::HookPointName::SvnPreLock, refusing)],
  );
  let edge = (port, &certificate);
  let (_, _, merged, merge_body) = dav_commit(edge, &name, b"hello\n");
  let lockinfo: &[u8] =
    b"<?xml version=\"1.0\" encoding=\"utf-8\"?><lockinfo xmlns=\"DAV:\"><lockscope><exclusive/>\
</lockscope><locktype><write/></locktype><owner>editing it</owner></lockinfo>";
  let (status, _, body) = dav(
    edge,
    ("LOCK", &format!("/{name}/f.txt")),
    &["Content-Type: text/xml"],
    lockinfo,
  );
  daemon.stop();
  assert_eq!(merged, 200, "{merge_body}");
  assert!(
    status >= 400
      && body.contains("Lock blocked by pre-lock hook (exit code 1) with output:\nno locks here"),
    "{status}: {body}"
  );
}

/// Shape: the blobs a partial clone's checkout fetches lazily in one request, as `git clone --filter=blob:none` asks
/// for every blob of its checkout: 2,900, toward git/git's checkout (about 4,400 files), past the about 2,870 wants
/// whose parsed lists alone filled one ring region's room (278,528 bytes) at about 97 bytes a want.
const LAZY_BLOBS: usize = 2_900;

/// A v2 `fetch` as git's promisor fetch asks for missing objects (`promisor-remote.c` `fetch_objects`: `git fetch
/// --filter=blob:none --stdin` with the no-op negotiator): a `want` for each of `ids`, the filter, no haves, `done`.
fn lazy_fetch_of(ids: &[ObjectId]) -> Vec<u8> {
  let mut request = Vec::new();
  pktline::write(Packet::Data(b"command=fetch\n"), &mut request).expect("the command");
  pktline::write(Packet::Delim, &mut request).expect("the delimiter");
  let mut arguments: Vec<String> = ids.iter().map(|id| format!("want {id}\n")).collect();
  arguments.extend([
    "filter blob:none\n".to_owned(),
    "no-progress\n".to_owned(),
    "done\n".to_owned(),
  ]);
  for argument in arguments {
    pktline::write(Packet::Data(argument.as_bytes()), &mut request).expect("an argument");
  }
  pktline::write(Packet::Flush, &mut request).expect("the flush");
  request
}

/// A-745 (found by use: w-interop's stock-git matrix, rows partial-clone and partial-blame, r103 on 36c6ed55d and r110
/// on 944588f6a, and w-bugs' own run of the matrix on 612 commits over 2,000 files: `git clone --filter=blob:none` exited
/// 128, "could not fetch <blob> from promisor remote", its checkout's lazy fetch refused `upload-pack: the request needs
/// more room than the server admits`). Do: push a line of [`LONG_HISTORY`] commits through the daemon's edge, each
/// request admitted one ring region; then fetch [`LAZY_BLOBS`] of its blobs as git's promisor fetch asks for a
/// checkout's missing blobs. Expect: the fetch `200`, a packfile section holding every blob asked: a fetch's answer is
/// admitted a slice at a time, never sized by its whole request.
#[test]
fn a_partial_clones_lazy_fetch_of_many_blobs_is_answered_over_the_daemons_edge() {
  use slates_git::object_name::Kind;
  let (identity, certificate) = identity();
  let daemon = daemon(identity);
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = name_on_the_repositorys_partition();
  create(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let edge = (port, &certificate);
  let (request, _) = long_history(LONG_HISTORY);
  let created = pushed(edge, &name, &request);
  let blobs: Vec<ObjectId> = (0..LAZY_BLOBS)
    .map(|at| git_name(Kind::Blob, format!("line {at}\n").as_bytes()))
    .collect();
  let fetched = uploaded(edge, &name, &lazy_fetch_of(&blobs));
  daemon.stop();
  assert_git_pushed(&created);
  let answer = String::from_utf8_lossy(
    fetched
      .1
      .get(..fetched.1.len().min(400))
      .unwrap_or_default(),
  )
  .into_owned();
  assert_eq!(fetched.0, 200, "{answer}");
  assert!(answer.starts_with("000dpackfile\n"), "{answer}");
  assert_eq!(ids_in(&fetched_pack(&fetched.1)).len(), LAZY_BLOBS);
}

/// Shape: [`merge_history`]'s main line: each of its commits after the first merges a topic.
const MERGE_MAINLINE: usize = 150;
/// Shape: each topic's commits, as long as a typical git/git topic series.
const MERGE_TOPIC: usize = 20;
/// Shape: how far back along the main line a topic branches, as git/git's topics branch from an older release.
const MERGE_BACK: usize = 60;

/// A history shaped as git/git's: a main line of [`MERGE_MAINLINE`] commits, each after the first a merge of the line
/// and a topic of [`MERGE_TOPIC`] commits branched [`MERGE_BACK`] commits back, every commit over one tree (the walk is
/// the graph's, so the texts are left out). The push creating `main` at its tip, and the tip.
fn merge_history() -> (Vec<u8>, ObjectId) {
  use slates_git::object_name::Kind;
  let blob = b"one\n".to_vec();
  let mut tree = b"100644 file\0".to_vec();
  tree.extend_from_slice(git_name(Kind::Blob, &blob).as_bytes());
  let tree_id = git_name(Kind::Tree, &tree);
  let mut objects: Vec<(Kind, Vec<u8>)> = vec![(Kind::Blob, blob), (Kind::Tree, tree)];
  let mut commit = |parents: &[ObjectId], message: String| {
    let lines: String = parents
      .iter()
      .map(|parent| format!("parent {parent}\n"))
      .collect();
    let bytes = format!(
      "tree {tree_id}\n{lines}author A <a@example.com> 1700000000 +0000\n\
       committer A <a@example.com> 1700000000 +0000\n\n{message}\n"
    )
    .into_bytes();
    let id = git_name(Kind::Commit, &bytes);
    objects.push((Kind::Commit, bytes));
    id
  };
  let mut main: Vec<ObjectId> = vec![commit(&[], "main 0".to_owned())];
  for at in 1..MERGE_MAINLINE {
    let mut topic = main[at.saturating_sub(MERGE_BACK)];
    for step in 0..MERGE_TOPIC {
      topic = commit(&[topic], format!("topic {at} {step}"));
    }
    let previous = main[at - 1];
    main.push(commit(&[previous, topic], format!("merge {at}")));
  }
  let tip = *main.last().expect("a commit");
  let listed: Vec<(Kind, &[u8])> = objects
    .iter()
    .map(|(kind, bytes)| (*kind, bytes.as_slice()))
    .collect();
  let mut request = Vec::new();
  let line = format!("{ZERO} {tip} refs/heads/main\0{CAPABILITIES}");
  pktline::write(Packet::Data(line.as_bytes()), &mut request).expect("the command");
  pktline::write(Packet::Flush, &mut request).expect("the flush");
  request.extend_from_slice(&stored_pack(&listed));
  (request, tip)
}

/// A v2 `fetch` deepening a shallow clone of `tip` by `depth` as `git fetch --deepen <depth>` asks: `want` the tip, its
/// `shallow` boundary, `deepen <depth>` and `deepen-relative`, `have` the tip, `done`.
fn deepen_of(tip: ObjectId, depth: usize) -> Vec<u8> {
  let mut request = Vec::new();
  pktline::write(Packet::Data(b"command=fetch\n"), &mut request).expect("the command");
  pktline::write(Packet::Delim, &mut request).expect("the delimiter");
  for argument in [
    format!("want {tip}\n"),
    format!("shallow {tip}\n"),
    format!("deepen {depth}\n"),
    "deepen-relative\n".to_owned(),
    format!("have {tip}\n"),
    "no-progress\n".to_owned(),
    "done\n".to_owned(),
  ] {
    pktline::write(Packet::Data(argument.as_bytes()), &mut request).expect("an argument");
  }
  pktline::write(Packet::Flush, &mut request).expect("the flush");
  request
}

/// A-745 (found by use: w-interop's stock-git matrix, row deepen, on git/git, r103 on 36c6ed55d and r110 on 944588f6a:
/// `git fetch --deepen 100` from a `--depth 1` clone exited 128, `upload-pack: the request needs more room than the
/// server admits`). Do: push [`merge_history`] (git/git's shape: every main commit a merge of a long topic) through the
/// daemon's edge, each request admitted one ring region; then, as a shallow clone of its tip, fetch at protocol v2 as
/// `git fetch --deepen 100` asks. Expect: the fetch `200`, a `shallow-info` section and a packfile section: a
/// deepening's walk is admitted a slice at a time, never sized by the history it walks.
#[test]
fn a_shallow_clone_of_a_merge_heavy_history_is_deepened_over_the_daemons_edge() {
  let (identity, certificate) = identity();
  let daemon = daemon(identity);
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = name_on_the_repositorys_partition();
  create(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let edge = (port, &certificate);
  let (request, tip) = merge_history();
  let created = pushed(edge, &name, &request);
  let deepened = uploaded(edge, &name, &deepen_of(tip, 100));
  daemon.stop();
  assert_git_pushed(&created);
  let answer = String::from_utf8_lossy(
    deepened
      .1
      .get(..deepened.1.len().min(400))
      .unwrap_or_default(),
  )
  .into_owned();
  assert_eq!(deepened.0, 200, "{answer}");
  assert!(answer.contains("shallow-info"), "{answer}");
  assert!(
    String::from_utf8_lossy(&deepened.1).contains("packfile"),
    "{answer}"
  );
}

/// The v2 `fetch` stock git 2.50's `git fetch --deepen <depth>` sends from a shallow clone of `tip` (its
/// `GIT_TRACE_PACKET` against slates, read): its choices with `sideband-all`, the tip its `shallow` line, `deepen <depth>`
/// and `deepen-relative`, `want-ref` its branch (slates advertises `ref-in-want`), `have` the tip, and no `done`, so its
/// first round is a negotiation's.
fn stock_deepen_of(tip: ObjectId, depth: usize) -> Vec<u8> {
  let mut request = Vec::new();
  pktline::write(Packet::Data(b"command=fetch\n"), &mut request).expect("the command");
  pktline::write(Packet::Delim, &mut request).expect("the delimiter");
  for argument in [
    "thin-pack\n".to_owned(),
    "no-progress\n".to_owned(),
    "include-tag\n".to_owned(),
    "ofs-delta\n".to_owned(),
    "sideband-all\n".to_owned(),
    format!("shallow {tip}\n"),
    format!("deepen {depth}\n"),
    "deepen-relative\n".to_owned(),
    "want-ref refs/heads/main\n".to_owned(),
    format!("have {tip}\n"),
  ] {
    pktline::write(Packet::Data(argument.as_bytes()), &mut request).expect("an argument");
  }
  pktline::write(Packet::Flush, &mut request).expect("the flush");
  request
}

/// Goal 93 (main 01a20182's r115 on git/git, `git_interop.rs` "deepened by one": after a `--depth 1` clone, `git fetch
/// --deepen=1` left the history one commit deep). The server's half, held: the session's repository kept the verb's
/// default branch, `main`, where git/git's is `master`, so its work clone began unborn and its commit was a root (the
/// session now makes the repository on the source's default branch). This guards the answer stock git's own request
/// gets. Do: push [`merge_history`] through the daemon's edge, then a commit over its tip and two tags (at the first
/// push's tip and at its tree) in a second push; then fetch at protocol v2 exactly as `git fetch --deepen 1` asks from
/// a shallow clone of the new tip ([`stock_deepen_of`]). Expect: `200`; a `shallow-info` section that unshallows the
/// tip (`unshallow <tip>`), never names it shallow again, and names its parent shallow, the new boundary; and a
/// packfile section after it, as `upload-pack.c`'s `deepen` with `deepen_relative` answers.
#[test]
fn a_shallow_clone_deepened_by_one_unshallows_its_tip_over_the_daemons_edge() {
  let (identity, certificate) = identity();
  let daemon = daemon(identity);
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = name_on_the_repositorys_partition();
  create(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let edge = (port, &certificate);
  let (request, base) = merge_history();
  let created = pushed(edge, &name, &request);
  // A second push, a commit over the first's tip, as the work clone's pushes came after the mirror's.
  let (on_top, tip) = {
    use slates_git::object_name::Kind;
    let blob = b"one\n".to_vec();
    let mut tree = b"100644 file\0".to_vec();
    tree.extend_from_slice(git_name(Kind::Blob, &blob).as_bytes());
    let tree_id = git_name(Kind::Tree, &tree);
    let commit = format!(
      "tree {tree_id}\nparent {base}\nauthor A <a@example.com> 1700000001 +0000\n\
       committer A <a@example.com> 1700000001 +0000\n\non top\n"
    )
    .into_bytes();
    let tip = git_name(Kind::Commit, &commit);
    let mut request = Vec::new();
    let line = format!("{base} {tip} refs/heads/main\0{CAPABILITIES}");
    pktline::write(Packet::Data(line.as_bytes()), &mut request).expect("the command");
    // A tag at the first push's tip and one at its tree, as the dataset's tags name its tip and tree.
    for (name, at) in [("refs/tags/at-base", base), ("refs/tags/at-tree", tree_id)] {
      let line = format!("{ZERO} {at} {name}");
      pktline::write(Packet::Data(line.as_bytes()), &mut request).expect("a tag's command");
    }
    pktline::write(Packet::Flush, &mut request).expect("the flush");
    request.extend_from_slice(&stored_pack(&[(Kind::Commit, commit.as_slice())]));
    (request, tip)
  };
  let moved = pushed(edge, &name, &on_top);
  let deepened = uploaded(edge, &name, &stock_deepen_of(tip, 1));
  daemon.stop();
  assert_git_pushed(&created);
  assert_eq!(moved.0, 200, "{}", String::from_utf8_lossy(&moved.1));
  let whole = String::from_utf8_lossy(&deepened.1).into_owned();
  let info = whole
    .split("packfile")
    .next()
    .unwrap_or_default()
    .to_owned();
  assert_eq!(deepened.0, 200, "{info}");
  assert!(info.contains("shallow-info"), "{info}");
  assert!(
    info.contains(&format!("unshallow {tip}")),
    "the tip unshallowed: {info}"
  );
  assert!(
    !info.contains(&format!("\u{1}shallow {tip}")),
    "the tip not shallow again: {info}"
  );
  assert!(
    info.contains(&format!("shallow {base}")),
    "its parent the new boundary: {info}"
  );
  assert!(whole.contains("packfile"), "{info}");
}

/// A-766 part 2 (DeltaV v1, ra_serf 1.14's `open_root` without HTTPv2; `mod_dav_svn` 1.14.x `activity.c`, `version.c`,
/// `mod_dav.c`). Do: on a daemon's HTTP edge, commit a file as a client without HTTPv2 does: MKACTIVITY of an activity;
/// CHECKOUT of the VCC into it (`apply-to-version`); PROPPATCH of the working baseline the CHECKOUT named, setting
/// `svn:log`; CHECKOUT of the root's version resource; PUT of `f.txt` under the working root that CHECKOUT named; MERGE
/// of the activity; DELETE of the activity. Expect: `201` "Activity ... has been created."; `201` naming
/// `!svn/wbl/ID/0`; `207`; `201` naming `!svn/wrk/ID/`; `201`; `200` naming revision 1; `204`; and revision 1 carrying
/// the log message and the file. Before, the site refused every one of them.
#[cfg(unix)]
#[test]
fn a_deltav_v1_commit_lands_as_mod_dav_svn_lands_it() {
  let (identity, certificate) = identity();
  let daemon = daemon(identity);
  let port = daemon.vcs_http_port().expect("the edge serves");
  let edge = (port, &certificate);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let name = svn_name_on_the_repositorys_partition();
  create_svn_hooked(&daemon, &name, Vec::new());
  let activity = "4c1d2b8a-0f6e-4b52-9c7a-3d2e1f0a9b8c";
  let made = dav(
    edge,
    ("MKACTIVITY", &format!("/{name}/!svn/act/{activity}")),
    &[],
    b"",
  );
  let checkout_body = format!(
    "<?xml version=\"1.0\" encoding=\"utf-8\"?><D:checkout xmlns:D=\"DAV:\"><D:activity-set><D:href>/{name}/!svn/act/\
     {activity}</D:href></D:activity-set><D:apply-to-version/></D:checkout>"
  );
  let xml = ["Content-Type: text/xml"];
  let baseline = dav(
    edge,
    ("CHECKOUT", &format!("/{name}/!svn/vcc/default")),
    &xml,
    checkout_body.as_bytes(),
  );
  let working_baseline = field(&baseline.1, "Location").unwrap_or_default();
  let logged = dav(
    edge,
    ("PROPPATCH", &working_baseline),
    &xml,
    b"<?xml version=\"1.0\" encoding=\"utf-8\"?><D:propertyupdate xmlns:D=\"DAV:\" \
      xmlns:S=\"http://subversion.tigris.org/xmlns/svn/\"><D:set><D:prop><S:log>a v1 commit</S:log></D:prop>\
      </D:set></D:propertyupdate>",
  );
  let root = dav(
    edge,
    ("CHECKOUT", &format!("/{name}/!svn/ver/0/")),
    &xml,
    checkout_body.as_bytes(),
  );
  let working_root = field(&root.1, "Location").unwrap_or_default();
  let put = dav(
    edge,
    ("PUT", &format!("{}f.txt", working_root)),
    &["Content-Type: application/vnd.svn-svndiff"],
    &fulltext_delta(b"hello\n"),
  );
  let merge = format!(
    "<?xml version=\"1.0\" encoding=\"utf-8\"?><D:merge xmlns:D=\"DAV:\"><D:source><D:href>/{name}/!svn/act/\
     {activity}</D:href></D:source><D:no-auto-merge/><D:no-checkout/><D:prop><D:checked-in/><D:version-name/>\
     </D:prop></D:merge>"
  );
  let merged = dav(edge, ("MERGE", &format!("/{name}")), &xml, merge.as_bytes());
  let deleted = dav(
    edge,
    ("DELETE", &format!("/{name}/!svn/act/{activity}")),
    &[],
    b"",
  );
  let landed_name = name.clone();
  let landed = crate::daemon::observe_at(&daemon, REPOSITORY, move |state| {
    let id = state
      .db
      .partition()
      .repositories()
      .repository_named(&landed_name)?
      .id;
    let records = state.vcs_stores.subversion_records(id)?;
    let log = records
      .revision_properties(1)?
      .iter()
      .find(|(name, _)| name.as_slice() == b"svn:log")
      .map(|(_, value)| String::from_utf8_lossy(value).into_owned());
    let file = records.index().node("/f.txt", 1).ok().flatten().is_some();
    Some((records.youngest(), log, file))
  });
  daemon.stop();
  assert_eq!(
    (
      (made.0, made.2.contains("has been created.")),
      (
        baseline.0,
        working_baseline.ends_with(&format!("/!svn/wbl/{activity}/0"))
      ),
      logged.0,
      (
        root.0,
        working_root.ends_with(&format!("/!svn/wrk/{activity}/"))
      ),
      put.0,
      (
        merged.0,
        merged.2.contains("<D:version-name>1</D:version-name>")
      ),
      deleted.0,
      landed,
    ),
    (
      (201, true),
      (201, true),
      207,
      (201, true),
      201,
      (200, true),
      204,
      Some((1, Some(String::from("a v1 commit")), true)),
    ),
    "MKACTIVITY {made:?}\nCHECKOUT {baseline:?}\nPROPPATCH {logged:?}\nCHECKOUT {root:?}\nPUT {put:?}\nMERGE {merged:?}"
  );
}

/// Stock `svn` run with `arguments` in `directory`, as alice with the owner's token over Basic, its configuration in
/// `config`: whether it succeeded, and what it printed.
#[cfg(unix)]
fn stock_svn(
  config: &std::path::Path,
  directory: &std::path::Path,
  arguments: &[&str],
) -> (bool, String) {
  let config_dir = config.to_string_lossy().into_owned();
  let output = std::process::Command::new("svn")
    .current_dir(directory)
    .args([
      "--non-interactive",
      "--no-auth-cache",
      "--config-dir",
      &config_dir,
      "--config-option",
      "servers:global:http-timeout=60",
      "--username",
      "alice",
      "--password",
      OWNER_TOKEN,
    ])
    .args(arguments)
    .stdin(std::process::Stdio::null())
    .output()
    .expect("stock svn runs");
  (
    output.status.success(),
    format!(
      "{}{}",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    ),
  )
}

/// A-766 part 4 (DeltaV v1 by use): stock svn 1.7 and later speak v1 to a server whose OPTIONS has no HTTPv2 headers
/// (ra_serf `options.c`: HTTPv2 only where `SVN-Me-Resource` is sent), as to `mod_dav_svn` with
/// `SVNAdvertiseV2Protocol Off`. Do: start a daemon whose edge serves loopback cleartext; create a Subversion
/// repository named with [`crate::vcs_http::svn::V1_ONLY_PREFIX`], whose OPTIONS withholds those headers in tests; run stock svn's
/// `mkdir` of `/trunk`, `checkout` of it, `add` and `commit` of a file, and `log`. Expect: each succeeds, the log lists
/// both commits' messages, and the edge made at least two DeltaV v1 activities (MKACTIVITY): the commits went over v1,
/// not HTTPv2. Skips loudly without stock svn.
#[cfg(unix)]
#[test]
fn stock_svn_commits_over_deltav_v1_on_a_daemons_loopback_edge() {
  let present = std::process::Command::new("svn")
    .args(["--version", "--quiet"])
    .output()
    .is_ok_and(|output| output.status.success());
  if !present {
    eprintln!(
      "SKIPPED stock_svn_commits_over_deltav_v1_on_a_daemons_loopback_edge: no `svn` on this host"
    );
    return;
  }
  let daemon = daemon_named(None).0;
  let port = daemon.vcs_http_port().expect("the edge started");
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let name = (0u32..)
    .map(|serial| format!("{}{serial}", crate::vcs_http::svn::V1_ONLY_PREFIX))
    .find(|name| usize::from(crate::verbs::owner_of_name(name, usize::from(SHARDS))) == REPOSITORY)
    .expect("a name routes there");
  create_svn_hooked(&daemon, &name, Vec::new());
  let scratch = crate::git_hooks::tests::ScratchDir::new();
  let config = scratch.path.join("config");
  let url = format!("http://127.0.0.1:{port}/{name}");
  let at = scratch.path.clone();
  let mkdir = stock_svn(
    &config,
    &at,
    &["mkdir", "-m", "the trunk over v1", &format!("{url}/trunk")],
  );
  let checkout = stock_svn(&config, &at, &["checkout", &format!("{url}/trunk"), "wc"]);
  let wc = scratch.path.join("wc");
  #[allow(clippy::disallowed_methods)]
  // test code: a file in its own working copy, in its scratch directory
  let written = std::fs::write(wc.join("a.txt"), b"hello over v1\n").is_ok();
  let add = stock_svn(&config, &wc, &["add", "a.txt"]);
  let commit = stock_svn(&config, &wc, &["commit", "-m", "a file over v1"]);
  let log = stock_svn(&config, &at, &["log", &url]);
  let activities = crate::daemon::observe_at(&daemon, REPOSITORY, |state| {
    state
      .refusals
      .get(crate::vcs_http::svn::V1_ACTIVITIES)
      .copied()
      .unwrap_or(0)
  });
  daemon.stop();
  for (step, (ran, output)) in [
    ("mkdir", &mkdir),
    ("checkout", &checkout),
    ("add", &add),
    ("commit", &commit),
    ("log", &log),
  ] {
    eprintln!("svn {step} (success {ran}):\n{output}");
  }
  assert_eq!(
    (
      (mkdir.0, checkout.0, written, add.0, commit.0, log.0),
      log.1.contains("the trunk over v1") && log.1.contains("a file over v1"),
      activities >= 2,
    ),
    ((true, true, true, true, true, true), true, true),
    "the edge made {activities} v1 activities"
  );
}

/// Goal 93 (w-sessionlog's red by use: git/git's whole clones were answered "possible repository corruption on the
/// remote side", and nothing on the server said why). Do: start a two-shard daemon with an edge; create a git repository;
/// as its owner, fetch an object it does not hold (git's own `not our ref`). Expect: the fetch answered refused, in git's
/// words, and the refusal counted once on the shard that served it (`vcs_http.git.upload_refused`), its typed refusal
/// named in the log. Before, a refused answer was counted nowhere.
#[test]
fn a_refused_fetch_over_the_daemons_edge_is_counted() {
  let (identity, certificate) = identity();
  let daemon = daemon(identity);
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = name_on_the_repositorys_partition();
  create(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let held_nowhere = ObjectId::from_bytes(GitFormat::Sha1, &[0x11; 20]).expect("an id");
  let fetched = uploaded((port, &certificate), &name, &fetch_of(held_nowhere));
  let refused: u64 = (0..2)
    .map(|partition| {
      counted_on(
        &daemon,
        partition,
        crate::vcs_http::git::upload::UPLOAD_REFUSED,
      )
    })
    .sum();
  daemon.stop();
  assert_eq!(fetched.0, 200);
  assert!(
    fetched
      .1
      .windows(b"ERR ".len())
      .any(|window| window == b"ERR "),
    "{:?}",
    String::from_utf8_lossy(&fetched.1)
  );
  assert_eq!(refused, 1, "the refused answer counted once");
}

/// Shape: git/git's refs (A-855; r16's dataset f7de814569 holds 1,019 tags over 83,079 commits, whose whole clone
/// outgrew a region's 256 KiB). On a line of this many commits, each tagged, the clone's answer takes 164,807 bytes
/// (this Mac, 2026-11-22), so the test's daemon is given a region a quarter of the size ([`REGION_SHARE`]), and the test
/// holds the need past its edge's room.
const TAGGED_REFS: usize = 1_019;

/// Shape: the share of its derived region's bulk the git-scale clone's daemon is given (A-855), so a line's clone of
/// git/git's refs outgrows one region as git/git's history outgrew the derived one.
const REGION_SHARE: u64 = 4;

/// Stock git's request creating `main` at the last of a line of `commits` commits (three objects each) and an annotated
/// tag `refs/tags/v<n>` of each, the tags' commands in name order as git sends them: the request, every ref's object, and
/// the count of objects it brings.
pub(crate) fn tagged_history(commits: usize) -> (Vec<u8>, Vec<ObjectId>, usize) {
  use slates_git::object_name::Kind;
  let mut objects: Vec<(Kind, Vec<u8>)> = Vec::new();
  let mut parent: Option<ObjectId> = None;
  let mut tags: Vec<(String, ObjectId)> = Vec::new();
  for at in 0..commits {
    let blob = format!("tagged {at}\n").into_bytes();
    let mut tree = b"100644 file\0".to_vec();
    tree.extend_from_slice(git_name(Kind::Blob, &blob).as_bytes());
    let tree_id = git_name(Kind::Tree, &tree);
    let parent_line = parent.map_or_else(String::new, |parent| format!("parent {parent}\n"));
    let commit = format!(
      "tree {tree_id}\n{parent_line}author A <a@example.com> 1700000000 +0000\n\
       committer A <a@example.com> 1700000000 +0000\n\ncommit {at}\n"
    )
    .into_bytes();
    let commit_id = git_name(Kind::Commit, &commit);
    let tag = format!(
      "object {commit_id}\ntype commit\ntag v{at}\ntagger A <a@example.com> 1700000000 +0000\n\nv{at}\n"
    )
    .into_bytes();
    tags.push((format!("refs/tags/v{at}"), git_name(Kind::Tag, &tag)));
    parent = Some(commit_id);
    objects.extend([
      (Kind::Commit, commit),
      (Kind::Tree, tree),
      (Kind::Blob, blob),
      (Kind::Tag, tag),
    ]);
  }
  let last = parent.expect("a commit");
  tags.sort();
  let listed: Vec<(Kind, &[u8])> = objects
    .iter()
    .map(|(kind, bytes)| (*kind, bytes.as_slice()))
    .collect();
  let mut request = Vec::new();
  let line = format!("{ZERO} {last} refs/heads/main\0{CAPABILITIES}");
  pktline::write(Packet::Data(line.as_bytes()), &mut request).expect("the command");
  for (name, id) in &tags {
    let line = format!("{ZERO} {id} {name}");
    pktline::write(Packet::Data(line.as_bytes()), &mut request).expect("a command");
  }
  pktline::write(Packet::Flush, &mut request).expect("the flush");
  request.extend_from_slice(&stored_pack(&listed));
  let mut wants = vec![last];
  wants.extend(tags.iter().map(|(_, id)| *id));
  (request, wants, objects.len())
}

/// A v2 `fetch` of every one of `wants` with `done`, `ofs-delta` and no progress, as `git clone` asks for a repository's
/// refs.
pub(crate) fn whole_fetch_of(wants: &[ObjectId]) -> Vec<u8> {
  let mut request = Vec::new();
  pktline::write(Packet::Data(b"command=fetch\n"), &mut request).expect("the command");
  pktline::write(Packet::Delim, &mut request).expect("the delimiter");
  for want in wants {
    pktline::write(
      Packet::Data(format!("want {want}\n").as_bytes()),
      &mut request,
    )
    .expect("a want");
  }
  for argument in ["ofs-delta\n", "no-progress\n", "done\n"] {
    pktline::write(Packet::Data(argument.as_bytes()), &mut request).expect("an argument");
  }
  pktline::write(Packet::Flush, &mut request).expect("the flush");
  request
}

/// The room the answer to `fetch` of repository `name` takes, answered whole on its shard in an unbounded room: what its
/// walk's tables, its request and its pack's buffers are charged; and the shard steps it took. It runs as a task of the
/// repository's shard, one step its request's serve and then one step each write of its answer, yielding between them,
/// as the edge's own answer runs: answered whole in one step, it held the shard past the watchdog's window under
/// ThreadSanitizer's slowdown (main's TSan lane, run 45385055058: 23,056 ms of CPU against a 22,998 ms window, the lib
/// run killed).
fn room_needed(daemon: &Daemon, name: &str, fetch: &[u8]) -> (u64, u64) {
  let (name, fetch) = (name.to_owned(), fetch.to_vec());
  let measured = move || async move {
    let lent = |step: &mut dyn FnMut(&mut crate::state::ShardState)| {
      crate::state::with_state(|state| step(state)).expect("the shard's state")
    };
    let mut id = None;
    lent(&mut |state| {
      id = state
        .db
        .partition()
        .repositories()
        .repository_named(&name)
        .map(|record| record.id);
    });
    let id = id.expect("the repository");
    let unbounded = u64::MAX >> 1;
    let mut admission = slates_git::Admission::new(unbounded);
    let mut answer = None;
    lent(&mut |state| {
      answer = crate::vcs_store::upload::lend_on(state, (id, Some(&owner())), |served, packs| {
        slates_git::upload::serve(
          served,
          slates_git::http::Version::V2,
          slates_git::upload::Asked::Request(&fetch),
          packs,
          &mut admission,
        )
      });
    });
    let mut answer = answer.expect("lent");
    let mut steps = 1u64;
    let mut out = Vec::new();
    loop {
      slates_rt::futures::yield_now().await;
      steps = steps.saturating_add(1);
      let mut written = None;
      lent(&mut |state| {
        written = crate::vcs_store::upload::lend_on(state, (id, None), |served, packs| {
          answer.write(
            served,
            packs,
            &mut admission,
            &mut out,
            slates_git::pktline::MAX_LEN,
          )
        });
      });
      if written.expect("lent") != slates_git::upload::Written::More {
        break;
      }
    }
    (unbounded.saturating_sub(admission.left()), steps)
  };
  crate::test_store::run_at(
    daemon,
    REPOSITORY,
    measured,
    std::time::Duration::from_nanos(patience_ns()),
  )
}

/// The first bytes of `answer`, as text, for a failure's message.
fn opening(answer: &[u8]) -> String {
  String::from_utf8_lossy(answer.get(..answer.len().min(400)).unwrap_or_default()).into_owned()
}

/// A-855 (found by use: r16-git on the big Studio, release b099adee4 over git/git f7de814569: `git clone` aborted
/// "remote: aborting due to possible repository corruption on the remote side", the anchor's log naming
/// `Negotiation(PastAdmission { asked: 7912, left: 283 })`; an answer was admitted one ring region, 256 KiB, whatever its
/// repository). Do: on a daemon whose region is a [`REGION_SHARE`]th of its derived one, push a line of [`TAGGED_REFS`]
/// commits, `main` and a tag of each, through the daemon's edge; measure
/// the room a whole fetch's answer takes; then fetch every ref whole as `git clone` asks. Expect: the answer needs more
/// than the edge's request room (the shape's guard); the fetch `200`, a packfile section holding every object pushed,
/// and no abort: an answer's walk is admitted what its repository's shard admits it, never one region. Red before: the
/// pack cut with git's abort words.
#[test]
fn a_clone_of_git_gits_many_refs_is_answered_whole_over_the_daemons_edge() {
  let (identity, certificate) = identity();
  let (daemon, _) = daemon_configured(Some(identity), |config| {
    config.region.bulk_bytes /= REGION_SHARE;
  });
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = name_on_the_repositorys_partition();
  create(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let edge = (port, &certificate);
  let (request, wants, objects) = tagged_history(TAGGED_REFS);
  let created = pushed(edge, &name, &request);
  let edge_room = crate::vcs_http::daemon_edge::bounds_of(daemon.config()).room;
  let fetch = whole_fetch_of(&wants);
  let (needed, steps) = room_needed(&daemon, &name, &fetch);
  let fetched = uploaded(edge, &name, &fetch);
  daemon.stop();
  assert_eq!(created.0, 200, "{}", opening(&created.1));
  assert!(
    steps > 2,
    "the measurement took its answer's writes a shard step each: {steps} steps"
  );
  assert!(
    needed > edge_room,
    "the shape's answer needs {needed}, within one region's {edge_room}"
  );
  assert_eq!(fetched.0, 200, "{}", opening(&fetched.1));
  assert!(
    !fetched
      .1
      .windows(b"aborting".len())
      .any(|window| window == b"aborting"),
    "the pack cut short"
  );
  assert_eq!(ids_in(&fetched_pack(&fetched.1)).len(), objects);
}

/// A-855 (main's ruling of 2026-11-21: a refusal costs time, never an answer; red first: a fetch refused `PastAdmission`
/// at negotiation waits and is answered whole). Do: push [`tagged_history`] of a few commits through the daemon's edge;
/// make the next room its repository's shard gives a fetch the fetch request's own length (room for its body, none for
/// its answer, as other works holding the shard's room would leave it), then the shard's own; fetch every ref whole. Expect: the fetch `200`, a packfile section holding every object pushed
/// and no abort; the answer made again once, counted `vcs_http.git.answered_again`, and nothing counted refused. Red
/// before: the fetch answered refused in its first room.
#[test]
fn a_fetch_past_its_room_waits_for_more_and_is_answered_whole() {
  let (identity, certificate) = identity();
  let daemon = daemon(identity);
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = name_on_the_repositorys_partition();
  create(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let edge = (port, &certificate);
  let (request, wants, objects) = tagged_history(TAGGED_FEW);
  let created = pushed(edge, &name, &request);
  let fetch = whole_fetch_of(&wants);
  let short = u64::try_from(fetch.len()).expect("a length");
  crate::daemon::observe_at(&daemon, REPOSITORY, move |_| {
    crate::vcs_store::upload::ROOMS.with(|rooms| rooms.borrow_mut().push_back(short));
  });
  let fetched = uploaded(edge, &name, &fetch);
  let counted = crate::daemon::observe_at(&daemon, REPOSITORY, |state| {
    let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
    (
      count(crate::vcs_http::git::upload::ANSWERED_AGAIN),
      count(crate::vcs_http::git::upload::UPLOAD_REFUSED),
    )
  });
  daemon.stop();
  assert_eq!(created.0, 200, "{}", opening(&created.1));
  assert_eq!(fetched.0, 200, "{}", opening(&fetched.1));
  assert!(
    !fetched
      .1
      .windows(b"aborting".len())
      .any(|window| window == b"aborting"),
    "the pack cut short"
  );
  assert_eq!(ids_in(&fetched_pack(&fetched.1)).len(), objects);
  assert_eq!(counted, (1, 0), "answered again once, refused never");
}

/// Shape: [`a_fetch_past_its_room_waits_for_more_and_is_answered_whole`]'s line: a few commits, each tagged.
pub(crate) const TAGGED_FEW: usize = 8;

std::thread_local! {
  /// A repository's landing turn a test holds on the shard's thread ([`a_mercurial_push_parks_for_its_repositorys_landing_turn_and_no_other_waits`]).
  static HELD_TURN: core::cell::RefCell<Option<crate::repository::svn_commit::landing::Turn>> =
    const { core::cell::RefCell::new(None) };
}

/// Derived: how long [`a_mercurial_push_parks_for_its_repositorys_landing_turn_and_no_other_waits`] holds the turn
/// while its push parks: long past a push's own landing on loopback, so a push that did not park would have answered.
const TURN_HELD: Duration = Duration::from_millis(400);

/// Takes repository `name`'s landing turn on its shard and holds it there ([`HELD_TURN`]); `patience_ns` a push's
/// patience for it there from now on, none for the server's own.
fn hold_turn(daemon: &Daemon, name: &str, patience_ns: u64) {
  let name = name.to_owned();
  crate::daemon::observe_at(daemon, REPOSITORY, move |state| {
    let id = state
      .db
      .partition()
      .repositories()
      .repository_named(&name)
      .map(|record| record.id)
      .expect("the repository");
    let turn =
      crate::repository::svn_commit::landing::Turn::take_now(state, id).expect("the turn free");
    HELD_TURN.with(|held| *held.borrow_mut() = Some(turn));
    crate::repository::hg_run::TEST_TURN_PATIENCE_NS.with(|patience| patience.set(patience_ns));
  });
}

/// Gives back the landing turn [`hold_turn`] holds.
fn give_back_turn(daemon: &Daemon) {
  crate::daemon::observe_at(daemon, REPOSITORY, |state| {
    if let Some(turn) = HELD_TURN.with(|held| held.borrow_mut().take()) {
      turn.give_back(state);
    }
  });
}

/// The `n`th Mercurial repository's name that routes to [`REPOSITORY`]'s partition, from the first.
fn hg_names_on_the_repositorys_partition(n: usize) -> Vec<String> {
  (0u32..)
    .map(|serial| format!("hg-{serial}"))
    .filter(|name| {
      usize::from(crate::verbs::owner_of_name(name, usize::from(SHARDS))) == REPOSITORY
    })
    .take(n)
    .collect()
}

/// Main 01a20182's ruling (i) of 2026-11-22 04:44 (a push lands under its repository's landing turn from its first
/// segment to its commit; a same-repository push needing it parks with its intake held, under the patience, refused
/// typed past it; other repositories never wait). Do: start a two-shard daemon over loopback TLS; create two Mercurial
/// repositories on one shard; hold the first's landing turn (as a long push's run holds it); push a changeset to the
/// second; push one to the first on a thread of its own, wait [`TURN_HELD`] and give the turn back; then, the turn held
/// again with a patience of 50 ms, push another to the first. Expect the second's push `200` with no wait counted; the
/// first's parked (`vcs.hg.push_turn_waits`) and still unanswered while the turn is held, then `200` and its head the
/// pushed changeset, the wait counted at least [`TURN_HELD`]; and the last refused in slates' words for a turn held past
/// its patience, counted given up, the repository's head unmoved.
#[test]
fn a_mercurial_push_parks_for_its_repositorys_landing_turn_and_no_other_waits() {
  let (identity, certificate) = identity();
  let (daemon, _instance) = daemon_named(Some(identity));
  let port = daemon.vcs_http_port().expect("the edge started");
  let names = hg_names_on_the_repositorys_partition(2);
  let (first, second) = (names[0].clone(), names[1].clone());
  create_empty_hg(&daemon, &first);
  create_empty_hg(&daemon, &second);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let made = history_of(&[
    (
      "parked",
      "parked",
      b"a push behind a held turn\n",
      None,
      "default",
    ),
    (
      "refused",
      "refused",
      b"a push past its patience\n",
      Some(0),
      "default",
    ),
  ]);
  let counted = |name: &'static str| {
    crate::daemon::observe_at(&daemon, REPOSITORY, move |state| {
      state.refusals.get(name).copied().unwrap_or(0)
    })
  };
  hold_turn(&daemon, &first, 0);
  let other = hg_unbundle((port, &certificate), &second, &history_bundle(&made, &[0]));
  let other_waits = counted(crate::repository::hg_run::TURN_WAITS);
  let parked = {
    let (bundle, certificate, first) = (
      history_bundle(&made, &[0]),
      certificate.clone(),
      first.clone(),
    );
    std::thread::spawn(move || hg_unbundle((port, &certificate), &first, &bundle))
  };
  // Seen parked, then held [`TURN_HELD`] more: the push's wait runs from before it was seen to past the release, whatever
  // its intake took on a busy machine before it parked.
  let deadline = std::time::Instant::now() + Duration::from_nanos(patience_ns());
  while counted(crate::repository::hg_run::TURN_WAITS) == 0 && std::time::Instant::now() < deadline
  {
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(Duration::from_millis(5));
  }
  let seen_parked = std::time::Instant::now();
  // The test holds the landing turn while the parked push waits for it; the daemon under test waits on its drivers.
  #[allow(clippy::disallowed_methods)]
  std::thread::sleep(TURN_HELD);
  let waiting = (
    counted(crate::repository::hg_run::TURN_WAITS),
    parked.is_finished(),
  );
  // Read before the release begins: the push waits from before it was seen to past the release, while the release's own
  // round trip back to this thread (`give_back_turn`) may end after the push was woken (TSan diag 3, run 45527257702:
  // read after it, the parked interval came out 0.56 ms past the push's wait under the lane's slowdown).
  let held_after_park = seen_parked.elapsed();
  give_back_turn(&daemon);
  let parked = parked.join().expect("the parked push answered");
  let waited_ns = counted(crate::repository::hg_run::TURN_WAITED_NS);
  let heads = hg_command((port, &certificate), &first, "heads");
  hold_turn(&daemon, &first, 50_000_000);
  let refused = hg_unbundle((port, &certificate), &first, &history_bundle(&made, &[1]));
  let given_up = counted(crate::repository::hg_run::TURN_GIVEN_UP);
  give_back_turn(&daemon);
  let unmoved = hg_command((port, &certificate), &first, "heads");
  daemon.stop();
  println!("(i) a push parked {waited_ns} ns behind a held landing turn");
  assert_hg_answered(&other);
  assert_eq!(other_waits, 0, "another repository's push never waits");
  assert_eq!(waiting, (1, false), "the first's push parked, unanswered");
  assert_hg_answered(&parked);
  let tip = crate::verbs::hex_of(&made[0].node);
  assert_eq!(heads, (200, format!("{tip}\n").into_bytes()));
  assert!(
    u128::from(waited_ns) >= held_after_park.as_nanos(),
    "waited {waited_ns} ns, parked {held_after_park:?} before the release at least"
  );
  assert_refused_past_its_patience(&refused, given_up, &unmoved, &heads);
}

/// The last push of [`a_mercurial_push_parks_for_its_repositorys_landing_turn_and_no_other_waits`]: `refused` in
/// slates' words for a turn held past its patience, counted once given up (`given_up`), and the repository's heads
/// after it (`unmoved`) still `heads`.
fn assert_refused_past_its_patience(
  refused: &(u16, String, Vec<u8>),
  given_up: u64,
  unmoved: &(u16, Vec<u8>),
  heads: &(u16, Vec<u8>),
) {
  let words = crate::repository::hg_run::TURN_PAST_PATIENCE;
  assert!(
    refused.2.windows(words.len()).any(|at| at == words),
    "{}",
    String::from_utf8_lossy(&refused.2)
  );
  assert_eq!(given_up, 1);
  assert_eq!(unmoved, heads, "the refused push moved nothing");
}

/// Format: the segment bound a long push of [`a_one_changeset_push_lands_during_a_long_push_and_readers_see_only_named`]
/// and [`a_long_push_stale_twice_is_re_based_once_holding_the_turn_and_lands`] is cut at, so its run lands segments
/// from its first few changesets.
const LONG_SEGMENT_BYTES: u64 = 32 * 1024;

/// Starts a push of a line of [`FRESH_CHANGESETS`] changesets to `name` on a thread of its own, cut at
/// [`LONG_SEGMENT_BYTES`] and parked once its run has landed two segments (`hg_run::TEST_PARK_AFTER_SEGMENTS`), its
/// segments unnamed; returns the line's tip and the push's thread once it has parked.
fn a_long_push_parked(
  daemon: &Daemon,
  (port, certificate): (u16, &CertificateDer<'static>),
  name: &str,
) -> (
  slates_hg::Node,
  std::thread::JoinHandle<(u16, String, Vec<u8>)>,
) {
  crate::daemon::observe_at(daemon, REPOSITORY, |_| {
    crate::repository::hg_run::TEST_SEGMENT_BYTES.with(|bound| bound.set(LONG_SEGMENT_BYTES));
    crate::repository::hg_run::TEST_PARK_AFTER_SEGMENTS.with(|park| park.set(2));
    crate::repository::hg_run::TEST_RELEASE.with(|release| release.set(false));
  });
  let (made, bundle) = a_line_of(FRESH_CHANGESETS);
  let patience = long_push_patience(&bundle);
  let long = {
    let (certificate, name) = (certificate.clone(), name.to_owned());
    std::thread::spawn(move || hg_unbundle_within((port, &certificate), &name, &bundle, patience))
  };
  while counted_on(
    daemon,
    REPOSITORY,
    crate::repository::hg_run::SEGMENTS_LANDED,
  ) < 2
  {
    assert!(
      !long.is_finished(),
      "the long push answered before it parked"
    );
    // The harness paces its looks; the daemon under test waits on its wheel.
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(Duration::from_millis(5));
  }
  (made[FRESH_CHANGESETS - 1].node, long)
}

/// Derived: how long a test's client waits for a long push's answer ([`a_long_push_parked`]): a client's patience
/// ([`patience_ns`]) and one of the daemon's liveness budgets for each segment the push lands at
/// [`LONG_SEGMENT_BYTES`], each a durable write of the store's that a busy machine draws out, and one more for its
/// entry's commit (main 01a20182's order of 2026-11-22 16:30: a busy machine is the norm, so a bound from the push's
/// own size, never a constant).
fn long_push_patience(bundle: &[u8]) -> Duration {
  let segments = u64::try_from(bundle.len())
    .unwrap_or(u64::MAX)
    .div_ceil(LONG_SEGMENT_BYTES)
    .saturating_add(1);
  Duration::from_nanos(
    patience_ns().saturating_add(segments.saturating_mul(crate::daemon::LIVENESS_BUDGET_NS)),
  )
}

/// Lets a long push parked by [`a_long_push_parked`] go on.
fn release_long_push(daemon: &Daemon) {
  crate::daemon::observe_at(daemon, REPOSITORY, |_| {
    crate::repository::hg_run::release_parked();
  });
}

/// Main 01a20182's rulings of 2026-11-22 07:32 and 12:00 (staging: a push lands its segments apart from its
/// repository's landing turn, which it takes only for its entry's commit; readers see only named sequences). Do: start a
/// two-shard daemon over loopback TLS; create a Mercurial repository; start a push of [`FRESH_CHANGESETS`] changesets
/// and park it once two of its segments have landed; ask `heads`; push one unrelated changeset with no bookmark; ask
/// `heads` and `lookup` the long push's tip; let the long push go on; ask `heads`. Expect the one-changeset push `200`
/// with no wait for the landing turn (before: it waited out the long push's run, 69 to 80% of its span in release);
/// `heads` the empty repository's, then the small push's tip alone, the long push's tip unknown, though its segments
/// are on the store; the long push then `200`, its fork re-based over the small push's entry, and `heads` both tips.
#[test]
fn a_one_changeset_push_lands_during_a_long_push_and_readers_see_only_named() {
  let (identity, certificate) = identity();
  let (daemon, _instance) = daemon_named(Some(identity));
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = hg_name_on_the_repositorys_partition();
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  create_empty_hg(&daemon, &name);
  let edge = (port, &certificate);
  let (long_tip, long) = a_long_push_parked(&daemon, edge, &name);
  let empty = hg_command(edge, &name, "heads");
  let small = history_of(&[("lonely", "lonely", b"a small push\n", None, "default")]);
  let began = std::time::Instant::now();
  let pushed = hg_unbundle(edge, &name, &bundle_of(&small, &[0], None));
  let small_took = began.elapsed();
  let waits = counted_on(&daemon, REPOSITORY, crate::repository::hg_run::TURN_WAITS);
  let during = hg_command(edge, &name, "heads");
  let unknown = hg_command(
    edge,
    &name,
    &format!("lookup&key={}", crate::verbs::hex_of(&long_tip)),
  );
  release_long_push(&daemon);
  let long = long.join().expect("the long push answered");
  let rebased = counted_on(&daemon, REPOSITORY, crate::repository::hg_run::REBASED);
  let lookups = counted_on(
    &daemon,
    REPOSITORY,
    crate::repository::hg_push::INDEX_LOOKUPS,
  );
  let after = hg_command(edge, &name, "heads");
  daemon.stop();
  println!(
    "staging: a one-changeset push took {small_took:?} during a {FRESH_CHANGESETS}-changeset push"
  );
  assert_hg_answered(&pushed);
  assert_eq!(waits, 0, "the small push never waited for the landing turn");
  assert_eq!(empty, (200, format!("{}\n", "0".repeat(40)).into_bytes()));
  let small_tip = crate::verbs::hex_of(&small[0].node);
  assert_eq!(
    during,
    (200, format!("{small_tip}\n").into_bytes()),
    "only the named push"
  );
  assert!(
    !String::from_utf8_lossy(&unknown.1).starts_with("1 "),
    "the long push's tip unknown: {}",
    String::from_utf8_lossy(&unknown.1)
  );
  assert_the_long_push_re_based_and_landed(
    &long,
    rebased,
    lookups,
    &after,
    [crate::verbs::hex_of(&long_tip), small_tip],
  );
}

/// The long push of [`a_one_changeset_push_lands_during_a_long_push_and_readers_see_only_named`] once let go on: `long`
/// answered `200`, its fork re-based once over the small push's entry and its index lookups counted (`rebased`,
/// `lookups`), and the repository's heads after it (`after`) naming both `tips`.
fn assert_the_long_push_re_based_and_landed(
  long: &(u16, String, Vec<u8>),
  rebased: u64,
  lookups: u64,
  after: &(u16, Vec<u8>),
  tips: [String; 2],
) {
  assert_hg_answered(long);
  assert_eq!(
    rebased, 1,
    "the long push's fork re-based over the small push's entry"
  );
  assert!(
    lookups > 0,
    "a settled push counts its fork's index lookups (goal 93, arena-20's counters)"
  );
  let after = String::from_utf8_lossy(&after.1).into_owned();
  for tip in tips {
    assert!(after.contains(&tip), "{after}");
  }
}

/// Main 01a20182's ruling of 2026-11-22 07:32 (a stale fork's re-base bounded, banned item 8: after one re-base off the
/// landing turn, the next is taken holding it, so another push can never extend the retry). Do: start a push of
/// [`FRESH_CHANGESETS`] changesets and park it once two segments have landed; push one unrelated changeset; have the
/// repository's index counted changed once more right after the long push's first re-base
/// (`hg_run::TEST_STALE_AFTER_REBASE`, as another push landing then would); let the long push go on. Expect the long
/// push `200`, re-based twice, the second holding the turn (`vcs.hg.push_rebased_in_turn` 1), and `heads` both tips.
#[test]
fn a_long_push_stale_twice_is_re_based_once_holding_the_turn_and_lands() {
  let (identity, certificate) = identity();
  let (daemon, _instance) = daemon_named(Some(identity));
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = hg_name_on_the_repositorys_partition();
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  create_empty_hg(&daemon, &name);
  let edge = (port, &certificate);
  let (long_tip, long) = a_long_push_parked(&daemon, edge, &name);
  let small = history_of(&[("lonely", "lonely", b"a small push\n", None, "default")]);
  let pushed = hg_unbundle(edge, &name, &bundle_of(&small, &[0], None));
  crate::daemon::observe_at(&daemon, REPOSITORY, |_| {
    crate::repository::hg_run::TEST_STALE_AFTER_REBASE.with(|stale| stale.set(true));
  });
  release_long_push(&daemon);
  let long = long.join().expect("the long push answered");
  let rebased = (
    counted_on(&daemon, REPOSITORY, crate::repository::hg_run::REBASED),
    counted_on(
      &daemon,
      REPOSITORY,
      crate::repository::hg_run::REBASED_IN_TURN,
    ),
  );
  let after = hg_command(edge, &name, "heads");
  daemon.stop();
  assert_hg_answered(&pushed);
  assert_hg_answered(&long);
  assert_eq!(
    rebased,
    (2, 1),
    "re-based off the turn, then once holding it"
  );
  let after = String::from_utf8_lossy(&after.1).into_owned();
  for tip in [
    crate::verbs::hex_of(&long_tip),
    crate::verbs::hex_of(&small[0].node),
  ] {
    assert!(after.contains(&tip), "{after}");
  }
}

/// Shape: the bytes of [`a_clone_of_a_pushed_pack_let_go_is_whole_holding_at_most_a_turns_windows`]'s one file: windows
/// enough that a clone holding its whole pack would hold many more than a turn's look-ahead.
pub(crate) const LET_GO_FILE: usize = 3 << 20;

/// A-864 piece 3 (main 01a20182's ruling of 2026-11-22: a clone of a let-go pack is whole and holds at most a turn's
/// windows, never its pack's). Do: start a two-shard store-backed daemon with its edge; push a create of `main` at a
/// commit whose one file is [`LET_GO_FILE`] bytes, stored whole in its pack; mark every window of the pushed pack landed
/// and let go those no home holds on the repository's shard (`vcs_store::let_go_read`), four or more; then clone `main`
/// over the edge. Expect: the
/// clone `200`, its pack the push's three objects; the stream stopped at a let-go window and brought windows back, the
/// most it held at once (`git_resident.peak_windows`) a turn's look-ahead and the window it stopped at, three, far
/// fewer than the pack's windows; and windows let go again behind its cursor. Red before: the clone made no stop, the
/// pack's windows unread.
#[test]
fn a_clone_of_a_pushed_pack_let_go_is_whole_holding_at_most_a_turns_windows() {
  let (identity, certificate) = identity();
  let daemon = daemon(identity);
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = name_on_the_repositorys_partition();
  create(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let edge = (port, &certificate);
  let (request, commit) = large_create(LET_GO_FILE);
  let created = pushed(edge, &name, &request);
  assert_git_pushed(&created);
  let (_, commands_end) = packets_from(&request, 0);
  let pack = request
    .get(commands_end..)
    .expect("the push's pack")
    .to_vec();
  let window = crate::daemon::observe_at(&daemon, REPOSITORY, |state| {
    state
      .vcs_stores
      .content()
      .map(slates_vcs::ContentStore::window)
  })
  .expect("the repository's content store");
  let windows: Vec<slates_vcs::ContentId> =
    pack.chunks(window).map(slates_vcs::ContentId::of).collect();
  // Each window marked landed, as the landed pack's let-go marks it, and let go where no home holds it here (a two-shard
  // daemon pins about half to their home). The pack's home here, where it holds the pack for the region, records its
  // windows and lets them go first, as a landed pack's let-go tells it (A-724 piece 8c, goal 93 item 2b(b)): a window of
  // an object the region holds is pinned until then (`vcs_store::landed_pin`).
  let ids = windows.clone();
  let pack_identity = slates_vcs::ContentId::of(&pack);
  let (let_go, refused) = crate::daemon::observe_at(&daemon, REPOSITORY, move |state| {
    crate::vcs_store::homes::landed_pack_here(state, pack_identity);
    crate::vcs_store::within_budget(state, 0, |stores| {
      let content = stores.content_mut().expect("the content store");
      for id in &ids {
        content.mark_landed(id);
      }
    });
    let outcomes: Vec<_> = ids
      .iter()
      .map(|id| crate::vcs_store::let_go_read(state, id))
      .collect();
    // A window let go already (`Away`) is as good as one let go now.
    let gone = |outcome: &Result<u64, slates_vcs::LetGoRefused>| {
      matches!(outcome, Ok(_) | Err(slates_vcs::LetGoRefused::Away))
    };
    let went = outcomes.iter().filter(|outcome| gone(outcome)).count();
    let why: Vec<String> = outcomes
      .iter()
      .filter(|outcome| !gone(outcome))
      .filter_map(|outcome| outcome.as_ref().err())
      .map(|refused| format!("{refused:?}"))
      .collect();
    (went, why)
  });
  let fetched = uploaded(edge, &name, &fetch_of(commit));
  let counted = |kind| counted_on(&daemon, REPOSITORY, kind);
  let observed = (
    counted("vcs_store.git_upload.stops"),
    counted("vcs_store.git_resident.peak_windows"),
    counted("vcs_store.git_resident.let_go_behind"),
  );
  daemon.stop();
  assert!(
    let_go >= 1,
    "{let_go} of {} windows let go: {refused:?}",
    windows.len()
  );
  assert_eq!(fetched.0, 200);
  assert_eq!(
    ids_in(&fetched_pack(&fetched.1)).len(),
    3,
    "the clone whole"
  );
  // The clone may copy the encoding's kept parts rather than read the let-go pack; where it reads the pack, at most a
  // turn's windows are held at once (`t_a864_a_fetch_lets_go_again_the_windows_its_cursor_has_passed` holds the
  // mechanism to it whatever the clone reads).
  let (_, peak, _) = observed;
  assert!(
    peak <= 3,
    "at most a turn's windows held: {observed:?} of {}",
    windows.len()
  );
}

/// A pushed history whose one file is chained as offset deltas ([`chained_create`]): the request creating `main` at its
/// last commit, and each commit's id, tree and blob, oldest first.
pub(crate) struct Chained {
  /// The push's request.
  pub(crate) request: Vec<u8>,
  /// Each commit, oldest first: its id, its tree's bytes and its blob's bytes.
  pub(crate) commits: Vec<(ObjectId, Vec<u8>, Vec<u8>)>,
  /// The commits' contents, oldest first.
  pub(crate) commit_bytes: Vec<Vec<u8>>,
}

/// Stock git's request creating `main` at the last of `depth + 1` commits, each a child of the one before, whose one
/// file is `bytes` long ([`large_create`]'s bytes), each commit's file the one before with one byte changed. The pack
/// holds the first blob whole and each later blob as an offset delta against the one before it, as `git pack-objects`
/// stores a file's revisions, so the last blob's chain is `depth` deltas over the first, every base earlier in the pack.
pub(crate) fn chained_create(bytes: usize, depth: usize) -> Chained {
  use slates_git::entry::{self, Header, Representation};
  use slates_git::object_name::Kind;
  let mut state = 0x9e37_79b9_7f4a_7c15u64;
  let first: Vec<u8> = (0..bytes)
    .map(|_| {
      state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
      state.to_be_bytes()[0]
    })
    .collect();
  let mut blobs = vec![first];
  for revision in 1..=depth {
    let mut next = blobs[revision - 1].clone();
    let at = (revision * bytes / (depth + 1)).min(bytes - 1);
    next[at] = next[at].wrapping_add(1);
    blobs.push(next);
  }
  let mut commits = Vec::new();
  let mut commit_bytes = Vec::new();
  let mut parent: Option<ObjectId> = None;
  for blob in &blobs {
    let mut tree = b"100644 large\0".to_vec();
    tree.extend_from_slice(git_name(Kind::Blob, blob).as_bytes());
    let tree_id = git_name(Kind::Tree, &tree);
    let parent_line = parent
      .map(|id| format!("parent {id}\n"))
      .unwrap_or_default();
    let commit = format!(
      "tree {tree_id}\n{parent_line}author A <a@example.com> 1700000000 +0000\n\
       committer A <a@example.com> 1700000000 +0000\n\nchained\n"
    )
    .into_bytes();
    let commit_id = git_name(Kind::Commit, &commit);
    parent = Some(commit_id);
    commits.push((commit_id, tree, blob.clone()));
    commit_bytes.push(commit);
  }
  // The blobs first, the first whole and each later one an offset delta against the one before; then the trees and
  // the commits, whole.
  let count = u32::try_from(blobs.len() * 3).expect("a count");
  let mut pack = Vec::new();
  let mut writer = slates_git::pack::Writer::new(GitFormat::Sha1, count, &mut pack);
  let mut at = entry::PACK_HEADER_LEN;
  let mut put = |representation: Representation, data: &[u8], size: usize, at: &mut u64| {
    let header = Header {
      representation,
      size: u64::try_from(size).expect("a size"),
    };
    let zlib = stored_zlib(data);
    let began = *at;
    entry::write(header, began, &mut writer).expect("the entry's header");
    slates_git::out::Out::put(&mut writer, &zlib);
    *at = began
      + entry::written_len(header, began).expect("the header's length")
      + u64::try_from(zlib.len()).expect("a length");
    began
  };
  let mut base = put(
    Representation::Whole(Kind::Blob),
    &blobs[0],
    blobs[0].len(),
    &mut at,
  );
  for revision in 1..blobs.len() {
    let delta =
      slates_git::store::delta_of(&blobs[revision - 1], &blobs[revision]).expect("a delta");
    base = put(
      Representation::OfsDelta { base },
      &delta,
      delta.len(),
      &mut at,
    );
  }
  for ((_, tree, _), commit) in commits.iter().zip(&commit_bytes) {
    put(Representation::Whole(Kind::Tree), tree, tree.len(), &mut at);
    put(
      Representation::Whole(Kind::Commit),
      commit,
      commit.len(),
      &mut at,
    );
  }
  writer.finish().expect("the trailer");
  let tip = commits.last().expect("a commit").0;
  let mut request = Vec::new();
  let line = format!("{ZERO} {tip} refs/heads/main\0 report-status");
  pktline::write(Packet::Data(line.as_bytes()), &mut request).expect("the command");
  pktline::write(Packet::Flush, &mut request).expect("the flush");
  request.extend_from_slice(&pack);
  Chained {
    request,
    commits,
    commit_bytes,
  }
}

/// Shape: the chain [`a_shallow_clone_of_a_chained_file_let_go_is_whole_at_each_depth`] pushes first: one delta.
const CHAIN_SHALLOW: usize = 1;

/// Shape: the deeper chain it pushes after: deep enough that the tip's read needs many bases' windows at once, each
/// brought back in its own turn.
const CHAIN_DEEP: usize = 8;

/// Shape: the bytes of each revision of the chained file: several windows, so each base's read spans windows.
const CHAIN_FILE: usize = 1 << 20;

/// Format: a turn's look-ahead and the window it stopped at, as `vcs_store::upload::read_ahead` brings them back (its
/// `LOOK_AHEAD_WINDOWS`, two, and one).
const LOOK_AHEAD_AND_STOP: usize = 3;

/// What a shallow clone of a chained file let go answered, at `depth` ([`chained_create`]): the push's and the shallow
/// clone's statuses, whether the clone held the tip's objects as pushed, the windows let go, the most windows the clone
/// held at once (`git_resident.peak_windows`) and the stopped windows held for their object
/// (`git_resident.pinned_held`), and the store's window.
fn shallow_of_a_chain_let_go(depth: usize) -> ((u16, u16), bool, usize, (u64, u64), usize) {
  use slates_git::object_name::Kind;
  let (identity, certificate) = identity();
  let daemon = daemon(identity);
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = name_on_the_repositorys_partition();
  create(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let edge = (port, &certificate);
  let chained = chained_create(CHAIN_FILE, depth);
  let created = pushed(edge, &name, &chained.request);
  let (tip, tree, blob) = chained.commits.last().expect("a tip").clone();
  let (_, commands_end) = packets_from(&chained.request, 0);
  let pack = chained
    .request
    .get(commands_end..)
    .expect("the push's pack")
    .to_vec();
  let window = crate::daemon::observe_at(&daemon, REPOSITORY, |state| {
    state
      .vcs_stores
      .content()
      .map(slates_vcs::ContentStore::window)
  })
  .expect("the repository's content store");
  let ids: Vec<slates_vcs::ContentId> =
    pack.chunks(window).map(slates_vcs::ContentId::of).collect();
  let pack_identity = slates_vcs::ContentId::of(&pack);
  // Each window marked landed and let go as [`a_clone_of_a_pushed_pack_let_go_is_whole_holding_at_most_a_turns_windows`]
  // lets its pack's go.
  let let_go = crate::daemon::observe_at(&daemon, REPOSITORY, move |state| {
    crate::vcs_store::homes::landed_pack_here(state, pack_identity);
    crate::vcs_store::within_budget(state, 0, |stores| {
      let content = stores.content_mut().expect("the content store");
      for id in &ids {
        content.mark_landed(id);
      }
    });
    ids
      .iter()
      .map(|id| crate::vcs_store::let_go_read(state, id))
      .filter(|outcome| matches!(outcome, Ok(_) | Err(slates_vcs::LetGoRefused::Away)))
      .count()
  });
  // The fetch answers without waiting for the encoding, as git/git's did with its history unencoded: its stream sends
  // the tip's objects through its loose stage, reading the pushed pack.
  crate::daemon::observe_at(&daemon, REPOSITORY, |_| {
    crate::vcs_store::git_encode::NO_FETCH_WAIT.with(|off| off.set(true));
  });
  let peak_before = counted_on(&daemon, REPOSITORY, "vcs_store.git_resident.peak_windows");
  let shallow = uploaded(edge, &name, &shallow_fetch_of(tip));
  let peak =
    counted_on(&daemon, REPOSITORY, "vcs_store.git_resident.peak_windows").max(peak_before);
  let pinned = counted_on(&daemon, REPOSITORY, "vcs_store.git_resident.pinned_held");
  daemon.stop();
  let mut expected = std::collections::BTreeMap::new();
  let tip_bytes = chained.commit_bytes.last().expect("a tip").clone();
  expected.insert(git_name(Kind::Commit, &tip_bytes).to_string(), tip_bytes);
  expected.insert(git_name(Kind::Tree, &tree).to_string(), tree);
  expected.insert(git_name(Kind::Blob, &blob).to_string(), blob);
  let whole_tip = shallow.0 == 200
    && std::panic::catch_unwind(|| objects_of(&fetched_pack_after(&shallow.1)))
      .is_ok_and(|objects| objects == expected);
  (
    (created.0, shallow.0),
    whole_tip,
    let_go,
    (peak, pinned),
    window,
  )
}

/// Goal 93, r130's shallow refusal (main 01a20182's order of 2026-11-23 16:25: a stock `git clone --depth 1` with the
/// landed pack let go answered `Pack(Store(Unreadable))` for a 785,414-byte blob of git/git). Do: at chains of
/// [`CHAIN_SHALLOW`] and [`CHAIN_DEEP`] deltas, each in a daemon of its own, push a create of `main` whose one file
/// ([`CHAIN_FILE`] bytes) is stored as offset deltas over its first revision ([`chained_create`]); let every window of
/// the pushed pack go; then clone it one commit deep, the fetch answering without waiting for the encoding
/// (`git_encode::NO_FETCH_WAIT`), as git/git's did with its history unencoded. Expect: each shallow clone `200` holding the tip's
/// commit, tree and blob as pushed. A one-commit-deep pack sends the tip's blob whole (its base is not sent), so the read
/// resolves its chain from the let-go pack, each base earlier in it than the cursor. Red before the change: the window a
/// base's read stopped at was brought back and let go again behind the cursor in the same turn
/// (`vcs_store::upload::let_go_behind`), the read met it again, and the answer was refused `Unreadable`.
#[test]
fn a_shallow_clone_of_a_chained_file_let_go_is_whole_at_each_depth() {
  for depth in [CHAIN_SHALLOW, CHAIN_DEEP] {
    let ((created, shallow), whole_tip, let_go, (peak, pinned), window) =
      shallow_of_a_chain_let_go(depth);
    assert_eq!(created, 200, "depth {depth}: the push");
    assert!(let_go >= 1, "depth {depth}: some window let go");
    assert!(
      shallow == 200 && whole_tip,
      "depth {depth}: the shallow clone {shallow}, whole {whole_tip}, peak {peak} windows of {window} bytes"
    );
    // The bound the pin keeps to: the windows the tip's chain lies in (each of its entries, the first blob whole and a
    // delta a revision, spans at most its bytes' windows and one more), beside a turn's look-ahead and its stop.
    assert!(
      pinned >= 1,
      "depth {depth}: a stopped window held for its object"
    );
    let chain_windows = (depth + 1) * (CHAIN_FILE.div_ceil(window) + 1);
    assert!(
      peak <= u64::try_from(chain_windows + LOOK_AHEAD_AND_STOP).expect("a bound"),
      "depth {depth}: held {peak} windows at once, past the chain's {chain_windows} and a turn's"
    );
  }
}

/// Shape: the version of `main`'s green the barrier is told a push owes in
/// [`a_push_whose_barrier_sees_no_progress_is_reported_ng_not_cut_off`]: one no record ever places.
const NEVER_PLACED: u64 = 1 << 40;

/// The push barrier's give-up (main 01a20182's ruling of 2026-11-23 18:55: an expired barrier answers git in its own
/// words, a typed refusal it can act on, never a dropped connection). Do: start a store-backed daemon with its edge;
/// push a create of `main` ([`large_create`]); tell the repository's barrier that an operation moved `main`'s green to a
/// version no record ever places ([`NEVER_PLACED`], `StateLogs::note_greens`); then push a create of `other` at the same
/// commit, asking a report. Expect: the first create `200` with `ok refs/heads/main`; the second `200` too, its report
/// `unpack ok` and `ng refs/heads/other unable to commit` once the barrier's window passed with nothing moving, the push
/// refused uncommitted counted. Red before: the barrier's give-up was answered with no report at all (a cut connection).
#[test]
fn a_push_whose_barrier_sees_no_progress_is_reported_ng_not_cut_off() {
  let (identity, certificate) = identity();
  let daemon = daemon(identity);
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = name_on_the_repositorys_partition();
  create(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let edge = (port, &certificate);
  let (request, commit) = large_create(1 << 10);
  let created = pushed(edge, &name, &request);
  let owed_name = name.clone();
  crate::daemon::observe_at(&daemon, REPOSITORY, move |state| {
    let repositories = state.db.partition().repositories();
    let repository = repositories
      .repository_named(&owed_name)
      .expect("the repository")
      .id;
    let main = repositories.branch(repository, "main").expect("main");
    let owed = slates_db::catalog::StateLogRecord {
      repository,
      log: slates_db::catalog::StateLog::Names,
      from: 0,
      to: 0,
      identity: [0; 32],
      spans: vec![slates_db::catalog::LogSpan {
        log: slates_db::catalog::LogName::Green { green: main },
        from: 0,
        to: NEVER_PLACED,
      }],
      changes: Vec::new(),
    };
    state.state_logs.note_greens(&owed);
  });
  let mut other = Vec::new();
  let line = format!("{ZERO} {commit} refs/heads/other\0 report-status");
  pktline::write(Packet::Data(line.as_bytes()), &mut other).expect("the command");
  pktline::write(Packet::Flush, &mut other).expect("the flush");
  other.extend_from_slice(&stored_pack(&[]));
  let answered = pushed(edge, &name, &other);
  let uncommitted = counted_on(&daemon, REPOSITORY, crate::state_log::PUSH_UNCOMMITTED);
  daemon.stop();
  assert_eq!(created.0, 200);
  assert!(
    String::from_utf8_lossy(&created.1).contains("ok refs/heads/main"),
    "{}",
    String::from_utf8_lossy(&created.1)
  );
  assert_eq!(answered.0, 200);
  let report = String::from_utf8_lossy(&answered.1).into_owned();
  assert!(
    report.contains("unpack ok") && report.contains("ng refs/heads/other unable to commit"),
    "{report}"
  );
  assert_eq!(uncommitted, 1);
}

/// A v2 `ls-refs` request for every ref, peeled, as `git ls-remote` asks.
fn ls_refs_all() -> Vec<u8> {
  let mut request = Vec::new();
  pktline::write(Packet::Data(b"command=ls-refs\n"), &mut request).expect("the command");
  pktline::write(Packet::Delim, &mut request).expect("the delimiter");
  for argument in [b"peel\n".as_slice(), b"symrefs\n", b"ref-prefix refs/\n"] {
    pktline::write(Packet::Data(argument), &mut request).expect("an argument");
  }
  pktline::write(Packet::Flush, &mut request).expect("the flush");
  request
}

/// Goal 93 (main 01a20182's r405: stock `git ls-remote` right after git/git's whole push waited 29 to 54 s in every
/// run, where stock lists its 4,451 refs in under a second). Do: start a store-backed daemon with its edge; push a
/// create of `main` at [`chained_create`]'s tip, a version neither derived nor encoded yet (neither is made ahead while
/// no fetch is answered, A-856); list its refs over v2 `ls-refs`; then fetch `main`. Expect: the listing `200`, naming
/// `main` at the tip, with the derivation's and the encoding's turns unmoved by it; and the fetch after it moving them,
/// so the work the listing skipped is still done for the fetch that reads it. Red before: a listing waited for the
/// repository's derivation, whole, and its encoding, as a fetch does.
#[test]
fn a_ref_listing_after_a_push_waits_for_no_derivation_or_encoding() {
  let (identity, certificate) = identity();
  let daemon = daemon(identity);
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = name_on_the_repositorys_partition();
  create(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let edge = (port, &certificate);
  let chained = chained_create(CHAIN_FILE, CHAIN_SHALLOW);
  let created = pushed(edge, &name, &chained.request);
  let tip = chained.commits.last().expect("a tip").0;
  let turns = || {
    counted_on(&daemon, REPOSITORY, "vcs_store.git_derive.turns")
      + counted_on(&daemon, REPOSITORY, "vcs_store.git_encode.turns")
  };
  let before = turns();
  let listed = uploaded(edge, &name, &ls_refs_all());
  let after_listing = turns();
  let fetched = uploaded(edge, &name, &fetch_of(tip));
  let after_fetch = turns();
  daemon.stop();
  assert_git_pushed(&created);
  assert_eq!(listed.0, 200);
  let main = format!("{tip} refs/heads/main");
  assert!(
    String::from_utf8_lossy(&listed.1).contains(&main),
    "the listing names main at the tip: {}",
    String::from_utf8_lossy(&listed.1)
  );
  assert_eq!(
    after_listing, before,
    "the listing waited for the derivation or the encoding"
  );
  assert_eq!(fetched.0, 200);
  assert!(
    after_fetch > after_listing,
    "the fetch readied the store: {before} {after_listing} {after_fetch}"
  );
}

/// The two clones of [`a_pushed_pack_is_let_go_by_its_commit_in_slices_and_a_clone_after_is_whole`]: the one commit
/// deep one (`shallow`) and the whole one (`fetched`) each `200`, each pack's objects the `pushed` bytes.
fn assert_both_clones_whole(
  shallow: &(u16, Vec<u8>),
  fetched: &(u16, Vec<u8>),
  pushed: &std::collections::BTreeMap<String, Vec<u8>>,
) {
  assert_eq!(shallow.0, 200);
  assert_eq!(
    objects_of(&fetched_pack_after(&shallow.1)),
    *pushed,
    "the shallow clone whole, its one commit's objects the pushed bytes"
  );
  assert_eq!(fetched.0, 200);
  assert_eq!(
    objects_of(&fetched_pack(&fetched.1)),
    *pushed,
    "the clone whole, each object the pushed bytes"
  );
}

/// Goal 93 item 2b, lifted again (main 01a20182's order of 2026-11-23, after r165's `Pack(Store(Unreadable))`): a landed
/// push's pack let go by its own commit, sliced off the owing step, and a whole clone after it. Do: start a two-shard
/// store-backed daemon with its edge; push a create of `main` at a commit whose one file is [`LET_GO_FILE`] bytes, a
/// pack of many windows; with nothing let go by hand, wait until the repository's shard counts landed bytes let go (by
/// the commit's own let-go, or first by the pack's home there); then clone `main` over the edge, clone it one commit
/// deep (r165: the shallow row failed too), and list `HEAD`. Expect: the push `200`; bytes let go, in more than one
/// slice (`vcs_store.landed.let_go_slices`); both clones `200`, each pack the push's three objects, each the pushed
/// bytes; and the listing naming `HEAD` at the commit. Red with the guard
/// (`LANDED_LET_GO` false): nothing let go.
#[test]
#[ignore = "the landed let-go is withdrawn (vcs_store::LANDED_LET_GO false, main 01a20182 2026-11-23): a re-lift's red, run with it on"]
fn a_pushed_pack_is_let_go_by_its_commit_in_slices_and_a_clone_after_is_whole() {
  let (identity, certificate) = identity();
  let daemon = daemon(identity);
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = name_on_the_repositorys_partition();
  create(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let edge = (port, &certificate);
  let (request, commit) = large_create(LET_GO_FILE);
  let created = pushed(edge, &name, &request);
  // Let go a slice a turn by a task of the repository's shard once the push ends: by the owner's let-go, or first by the
  // pack's home where that is the repository's shard, which ranks by the pack's identity.
  let let_go_bytes = || {
    counted_on(&daemon, REPOSITORY, crate::vcs_store::LANDED_LET_GO_BYTES)
      + counted_on(
        &daemon,
        REPOSITORY,
        crate::vcs_store::homes::LANDED_HOME_LET_GO_BYTES,
      )
  };
  let waited = std::time::Instant::now();
  while let_go_bytes() == 0 && waited.elapsed() < Duration::from_secs(10) {
    // The harness paces its looks; the daemon under test waits on its wheel.
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(Duration::from_millis(20));
  }
  let let_go = let_go_bytes();
  let slices = counted_on(&daemon, REPOSITORY, crate::vcs_store::LET_GO_SLICES);
  let fetched = uploaded(edge, &name, &fetch_of(commit));
  let shallow = uploaded(edge, &name, &shallow_fetch_of(commit));
  let listed = uploaded(edge, &name, LS_REFS_HEAD);
  daemon.stop();
  assert_git_pushed(&created);
  assert!(
    let_go > 0,
    "the push's commit let its landed pack go: {let_go} bytes"
  );
  assert!(slices > 1, "let go in slices: {slices}");
  let (_, commands_end) = packets_from(&request, 0);
  let pushed_objects = objects_of(request.get(commands_end..).expect("the push's pack"));
  assert_both_clones_whole(&shallow, &fetched, &pushed_objects);
  assert_eq!(listed.0, 200);
  assert!(
    String::from_utf8_lossy(&listed.1)
      .contains(&format!("{commit} HEAD symref-target:refs/heads/main")),
    "{}",
    String::from_utf8_lossy(&listed.1)
  );
}

/// Shape: the annotated tags of [`an_advertisement_over_a_let_go_pack_is_the_one_over_it_held`]'s push, each laid in its
/// pack between two of its files, so their entries lie in windows of their own.
const LET_GO_TAGS: usize = 6;

/// Stock git's request creating `main` at a root commit of [`LET_GO_TAGS`] files of a MiB each and an annotated tag of
/// that commit after each file in its pack, `refs/tags/v<n>`: the request, the commit and the tags' names.
fn tagged_spread_create() -> (Vec<u8>, ObjectId) {
  use slates_git::object_name::Kind;
  let mut state = 0x2545_f491_4f6c_dd1du64;
  let files: Vec<Vec<u8>> = (0..LET_GO_TAGS)
    .map(|_| {
      (0..1 << 20)
        .map(|_| {
          state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
          state.to_be_bytes()[0]
        })
        .collect()
    })
    .collect();
  let mut tree = Vec::new();
  for (at, file) in files.iter().enumerate() {
    tree.extend_from_slice(format!("100644 f{at}\0").as_bytes());
    tree.extend_from_slice(git_name(Kind::Blob, file).as_bytes());
  }
  let tree_id = git_name(Kind::Tree, &tree);
  let commit = format!(
    "tree {tree_id}\nauthor A <a@example.com> 1700000000 +0000\n\
     committer A <a@example.com> 1700000000 +0000\n\ntagged\n"
  )
  .into_bytes();
  let commit_id = git_name(Kind::Commit, &commit);
  let tags: Vec<Vec<u8>> = (0..LET_GO_TAGS)
    .map(|at| {
      format!(
        "object {commit_id}\ntype commit\ntag v{at}\ntagger A <a@example.com> 1700000000 +0000\n\nv{at}\n"
      )
      .into_bytes()
    })
    .collect();
  let mut objects: Vec<(Kind, &[u8])> = vec![(Kind::Commit, &commit), (Kind::Tree, &tree)];
  for (file, tag) in files.iter().zip(&tags) {
    objects.push((Kind::Blob, file));
    objects.push((Kind::Tag, tag));
  }
  let pack = stored_pack(&objects);
  let mut request = Vec::new();
  let line = format!("{ZERO} {commit_id} refs/heads/main\0 report-status");
  pktline::write(Packet::Data(line.as_bytes()), &mut request).expect("the command");
  for (at, tag) in tags.iter().enumerate() {
    let line = format!("{ZERO} {} refs/tags/v{at}", git_name(Kind::Tag, tag));
    pktline::write(Packet::Data(line.as_bytes()), &mut request).expect("a command");
  }
  pktline::write(Packet::Flush, &mut request).expect("the flush");
  request.extend_from_slice(&pack);
  (request, commit_id)
}

/// Repository `name`'s upload-pack advertisement, as stock git asks it for a v0 clone: its tags peeled.
fn upload_advertised(edge: (u16, &CertificateDer<'static>), name: &str) -> (u16, Vec<u8>) {
  let (port, certificate) = edge;
  let request = format!(
    "GET /{name}/info/refs?service=git-upload-pack HTTP/1.1\r\nHost: localhost\r\n{}\r\n\r\n",
    owner_bearer()
  );
  exchange(port, certificate, request.as_bytes())
}

/// A-864's readers (main 01a20182's r52: after git/git's push landed and its pack's windows were let go, every fetch and
/// ls-remote answered "possible repository corruption", the advertisement's tag peel reading a let-go window). Do: push
/// a create of `main` and [`LET_GO_TAGS`] annotated tags, each tag's entry between two MiB files in the pack; read the
/// upload-pack advertisement; let the pack's home on the repository's shard record its windows and let them go, as a
/// landed pack's let-go tells it, then mark every window of the pack landed and let go those no home holds there; read
/// the advertisement again; clone `main`. Expect: the second advertisement the first, byte for byte, its tags
/// peeled; windows brought back from the node's store to answer it; the clone whole. Red before: refused.
#[test]
fn an_advertisement_over_a_let_go_pack_is_the_one_over_it_held() {
  let (identity, certificate) = identity();
  let daemon = daemon(identity);
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = name_on_the_repositorys_partition();
  create(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let edge = (port, &certificate);
  let (request, commit) = tagged_spread_create();
  let created = pushed(edge, &name, &request);
  let report = String::from_utf8_lossy(&created.1).into_owned();
  assert_eq!(
    (created.0, report.matches("ok refs/").count()),
    (200, 1 + LET_GO_TAGS),
    "{report}"
  );
  let held = upload_advertised(edge, &name);
  let (_, commands_end) = packets_from(&request, 0);
  let pack = request
    .get(commands_end..)
    .expect("the push's pack")
    .to_vec();
  let window = crate::daemon::observe_at(&daemon, REPOSITORY, |state| {
    state
      .vcs_stores
      .content()
      .map(slates_vcs::ContentStore::window)
  })
  .expect("the repository's content store");
  let ids: Vec<slates_vcs::ContentId> =
    pack.chunks(window).map(slates_vcs::ContentId::of).collect();
  let pack_identity = slates_vcs::ContentId::of(&pack);
  let let_go = crate::daemon::observe_at(&daemon, REPOSITORY, move |state| {
    // The pack's home here, where it holds the pack for the region, records where each window lies on the node's store
    // and lets it go first (A-724 piece 8c, goal 93 item 2b(b)), as a landed pack's let-go tells it: a window of an
    // object the region holds is pinned until then (`vcs_store::landed_pin`).
    crate::vcs_store::homes::landed_pack_here(state, pack_identity);
    crate::vcs_store::within_budget(state, 0, |stores| {
      let content = stores.content_mut().expect("the content store");
      for id in &ids {
        content.mark_landed(id);
      }
    });
    ids
      .iter()
      .filter(|id| {
        matches!(
          crate::vcs_store::let_go_read(state, id),
          Ok(_) | Err(slates_vcs::LetGoRefused::Away)
        )
      })
      .count()
  });
  let let_go_ok = let_go > 0;
  let again = upload_advertised(edge, &name);
  let brought = counted_on(&daemon, REPOSITORY, "vcs_store.read_through.brought_back");
  let fetched = uploaded(edge, &name, &fetch_of(commit));
  daemon.stop();
  assert!(let_go_ok, "windows let go");
  assert_eq!(held.0, 200);
  assert!(
    again == held,
    "the advertisement over the let-go pack: {}",
    String::from_utf8_lossy(&again.1)
  );
  assert!(
    String::from_utf8_lossy(&held.1).contains("^{}"),
    "its tags peeled: {}",
    String::from_utf8_lossy(&held.1)
  );
  // Whether its tags' windows were among those let go (the rest pinned by their home) decides whether any was brought
  // back to answer it; either way it is the same answer.
  eprintln!("windows brought back to answer the advertisement: {brought}");
  assert_eq!(fetched.0, 200);
  assert_eq!(
    ids_in(&fetched_pack(&fetched.1)).len(),
    2 + LET_GO_TAGS,
    "the clone of main whole: its commit, tree and files"
  );
}

/// Format: the changesets and the bytes of each one's file in
/// [`a_long_pushs_file_texts_leave_the_content_store_once_their_segments_land`]: a history whose file texts are many
/// segments' worth.
const TEXTS_CHANGESETS: usize = 120;
/// Format: the bytes of each file of that history.
const TEXT_BYTES: usize = 8 * 1024;

/// A push of own-text files ([`a_push_of_own_texts`]): the changesets, the files' texts, and the push's answer.
type OwnTextsPush = (Vec<Made>, &'static [Vec<u8>], (u16, String, Vec<u8>));

/// Pushes into `name`, through `edge`, a line of [`TEXTS_CHANGESETS`] changesets each adding a file of its own
/// [`TEXT_BYTES`] (each text its own, so the store keeps every one), cut at [`LONG_SEGMENT_BYTES`]: the changesets, the
/// files' texts, and the push's answer.
fn a_long_push_of_own_texts(
  daemon: &Daemon,
  edge: (u16, &CertificateDer<'static>),
  name: &str,
) -> OwnTextsPush {
  a_push_of_own_texts(daemon, edge, (name, TEXTS_CHANGESETS))
}

/// [`a_long_push_of_own_texts`] of a line of `changesets`.
fn a_push_of_own_texts(
  daemon: &Daemon,
  edge: (u16, &CertificateDer<'static>),
  (name, changesets): (&str, usize),
) -> OwnTextsPush {
  crate::daemon::observe_at(daemon, REPOSITORY, |_| {
    crate::repository::hg_run::TEST_SEGMENT_BYTES.with(|bound| bound.set(LONG_SEGMENT_BYTES));
  });
  let paths: &'static [String] = Box::leak(
    (0..changesets)
      .map(|at| format!("file-{at}"))
      .collect::<Vec<_>>()
      .into_boxed_slice(),
  );
  // Each file's text its own, so the store keeps every one (a content the store holds once, by its bytes).
  let texts: &'static [Vec<u8>] = Box::leak(
    (0..changesets)
      .map(|at| {
        let mut text = Vec::with_capacity(TEXT_BYTES);
        let mut line = 0usize;
        while text.len() < TEXT_BYTES {
          text.extend_from_slice(format!("file {at} line {line}\n").as_bytes());
          line += 1;
        }
        text.truncate(TEXT_BYTES);
        text
      })
      .collect::<Vec<_>>()
      .into_boxed_slice(),
  );
  let commits: Vec<Commit> = (0..changesets)
    .map(|at| {
      (
        paths[at].as_str(),
        paths[at].as_str(),
        texts[at].as_slice(),
        at.checked_sub(1),
        "default",
      )
    })
    .collect();
  let made = history_of(&commits);
  let revs: Vec<usize> = (0..changesets).collect();
  let pushed = hg_unbundle(edge, name, &history_bundle(&made, &revs));
  (made, texts, pushed)
}

/// Shape: the two histories [`a_pushs_texts_high_water_is_a_segments_whatever_its_history`] pushes, changesets each.
const HELD_SHORT_HISTORY: usize = 60;
/// Shape: the longer of them.
const HELD_LONG_HISTORY: usize = 240;

/// What a daemon's push of a line of `changesets` own-text files held at most, its file texts' share of it, and its
/// fork's rows' bytes ([`crate::repository::hg_push::PUSH_HELD_HIGH_BYTES`], `PUSH_TEXTS_HIGH_BYTES`, `PUSH_ROWS_BYTES`),
/// on a fresh two-shard daemon over loopback TLS.
fn held_and_rows_of(changesets: usize) -> (u64, u64, u64) {
  let (identity, certificate) = identity();
  let (daemon, _instance) = daemon_named(Some(identity));
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = hg_name_on_the_repositorys_partition();
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  create_empty_hg(&daemon, &name);
  let (_, _, pushed) = a_push_of_own_texts(&daemon, (port, &certificate), (&name, changesets));
  assert_hg_answered(&pushed);
  let held = counted_on(
    &daemon,
    REPOSITORY,
    crate::repository::hg_push::PUSH_HELD_HIGH_BYTES,
  );
  let rows = counted_on(
    &daemon,
    REPOSITORY,
    crate::repository::hg_push::PUSH_ROWS_BYTES,
  );
  let texts = counted_on(
    &daemon,
    REPOSITORY,
    crate::repository::hg_push::PUSH_TEXTS_HIGH_BYTES,
  );
  daemon.stop();
  println!("{changesets} changesets: held at most {held}, its texts {texts}, rows {rows}");
  (held, texts, rows)
}

/// Goal 93, the 410 MiB bar (main's ruling of 2026-11-23: attack the push-all peak first, judged by its VmHWM, with
/// `vcs.hg.push_held_high_bytes` as the non-vacuity gauge). Do: push a line of [`HELD_SHORT_HISTORY`] changesets each
/// adding a file of its own [`TEXT_BYTES`], cut at [`LONG_SEGMENT_BYTES`], on a fresh daemon, then [`HELD_LONG_HISTORY`]
/// on another; read each push's held high-water, its file texts' share, and its fork's rows. Expect each held above zero;
/// the texts' share equal within a segment's slack whatever the history (each segment's texts let go as it lands); the
/// rows growing with it (a row a revision of every log: 35,860 and 143,780 bytes in debug). Measured, not yet bounded:
/// the rest of the held high-water, the push's admission, grows about 843 bytes a changeset (56,809 and 208,768), its
/// bookkeeping kept to the end: the next piece's red.
#[test]
fn a_pushs_texts_high_water_is_a_segments_whatever_its_history() {
  let (short_held, short_texts, short_rows) = held_and_rows_of(HELD_SHORT_HISTORY);
  let (long_held, long_texts, long_rows) = held_and_rows_of(HELD_LONG_HISTORY);
  let slack = LONG_SEGMENT_BYTES;
  println!(
    "held at most {short_held} of {HELD_SHORT_HISTORY} changesets and {long_held} of {HELD_LONG_HISTORY}; texts \
     {short_texts} and {long_texts}; rows {short_rows} and {long_rows}; a segment {slack}"
  );
  assert!(short_held > 0 && long_held > 0, "the gauge moved");
  assert!(
    short_texts > 0 && long_texts.abs_diff(short_texts) <= slack,
    "the texts held within a segment's slack: {short_texts} and {long_texts}"
  );
  assert!(long_rows > short_rows, "the rows grow with the history");
}

/// Each file's window as [`a_long_pushs_file_texts_leave_the_content_store_once_their_segments_land`] looks at it,
/// `(here, (noted, pinned), entry)` a file: every entry kept, every window let go, and none noted or pinned, since no
/// home holds any.
fn assert_every_files_window_let_go(windows: &[(bool, (bool, bool), bool)]) {
  for (at, (here, (noted, pinned), entry)) in windows.iter().enumerate() {
    assert!(entry, "file {at}'s entry kept");
    assert!(!here, "file {at}'s window let go");
    assert!(
      !*noted && !*pinned,
      "file {at}'s window neither noted nor pinned: no home holds it"
    );
  }
}

/// Main 01a20182's order of 2026-11-22 14:00 (CI 45065911852: 4.7 GB of arena held on the repository's shard after
/// Mercurial's whole history landed, the content store keeping every revision's text). Do: start a two-shard daemon over
/// loopback TLS; create a Mercurial repository; push a line of [`TEXTS_CHANGESETS`] changesets each adding a file of its
/// own [`TEXT_BYTES`], cut at [`LONG_SEGMENT_BYTES`]; look at each file's window in its shard's content store; clone it
/// whole (`getbundle`). Expect the push `200`; every file's window let go of the content store, its entry kept, each
/// pinned by none, and noted for none, since no home holds any (goal 93 item 2b(b): before, the 71 a home held here
/// were pinned; then every window was noted, hundreds of thousands in a whole history, w-shared's 1ecb57cf4); the bytes
/// let go counted; and the clone carrying the last file's bytes, brought back from the changeset
/// store. Red before: the store held every text.
#[test]
fn a_long_pushs_file_texts_leave_the_content_store_once_their_segments_land() {
  let (identity, certificate) = identity();
  let (daemon, _instance) = daemon_named(Some(identity));
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = hg_name_on_the_repositorys_partition();
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  create_empty_hg(&daemon, &name);
  let edge = (port, &certificate);
  let (made, texts, pushed) = a_long_push_of_own_texts(&daemon, edge, &name);
  // Each file's window: whether its shard's content store reads its bytes (none for one let go), whether a home pins it
  // there, and whether its entry stays.
  let windows = crate::daemon::observe_at(&daemon, REPOSITORY, move |state| {
    crate::vcs_store::lent_state(state, |state| {
      let content = state.vcs_stores.content()?;
      Some(
        texts
          .iter()
          .map(|text| {
            let window = slates_vcs::ContentId::of(text);
            let pinned = crate::vcs_store::landed_pin(&window).is_some();
            let noted = crate::vcs_store::homes::landed(&window).is_some();
            (
              content
                .object_bytes(&window, u64::try_from(text.len()).unwrap())
                .is_some(),
              (noted, pinned),
              content.len_of(&window).is_some(),
            )
          })
          .collect::<Vec<_>>(),
      )
    })
  })
  .expect("the repository's content store");
  let let_go = counted_on(
    &daemon,
    REPOSITORY,
    crate::repository::hg_run::TEXT_BYTES_LET_GO,
  );
  let (turns, longest, quantum) = crate::daemon::observe_at(&daemon, REPOSITORY, |state| {
    let counted = |kind| state.refusals.get(kind).copied().unwrap_or(0);
    (
      counted(crate::repository::hg_run::LET_GO_TURNS),
      counted(crate::repository::hg_run::LET_GO_LONGEST_TURN_NS),
      state.config.step_quantum_ns(),
    )
  });
  let tip = crate::verbs::hex_of(&made[TEXTS_CHANGESETS - 1].node);
  let cloned = hg_getbundle(edge, &name, &tip);
  daemon.stop();
  let gone = windows.iter().filter(|(here, ..)| !here).count();
  println!("let go: {let_go} bytes; {gone} of {TEXTS_CHANGESETS} files' windows gone");
  assert_hg_answered(&pushed);
  assert_every_files_window_let_go(&windows);
  assert!(gone > 0, "some window let go");
  // At the derived budget (half the shard's step quantum) the commit's let-go is spread over turns too.
  println!(
    "the commit's let-go: {turns} turns, the longest {longest} ns of a {quantum} ns quantum"
  );
  assert!(turns > 1, "the commit's let-go spread over turns: {turns}");
  assert!(
    let_go >= u64::try_from(gone * TEXT_BYTES).unwrap(),
    "{let_go} bytes let go"
  );
  assert_eq!(cloned.0, 200);
  let last = &texts[TEXTS_CHANGESETS - 1];
  assert!(
    cloned.2.windows(64).any(|window| window == &last[..64]),
    "the clone carries the last file's bytes"
  );
}

/// Main's order of 2026-11-22 (CI 45110563116: on x86, shard 0 spun 1,045 ms of CPU in one poll of `hg_push::unbundle`
/// as a whole-history push's committed file revisions were noted and let go in one step, and the daemon was killed).
/// Do: start a two-shard daemon over loopback TLS; push [`a_long_push_of_own_texts`]'s line with the let-go's turn
/// budget at a nanosecond (`hg_run::TEST_LET_GO_TURN_NS`); read the let-go's counters. Expect the push `200`; its file
/// revisions let go over a turn each, one at a time ([`TEXTS_CHANGESETS`] times [`LET_GO_UNITS_A_CHANGESET`] turns: a
/// turn ends at the first revision past its budget, a revision being the let-go's unit), the longest counted; every
/// file's window let go. Red before: one turn took them all (1 turn, 642,500 ns in debug).
#[test]
fn a_committed_pushs_file_revisions_are_let_go_a_turn_at_a_time() {
  let (identity, certificate) = identity();
  let (daemon, _instance) = daemon_named(Some(identity));
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = hg_name_on_the_repositorys_partition();
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  create_empty_hg(&daemon, &name);
  let quantum = crate::daemon::observe_at(&daemon, REPOSITORY, |state| {
    crate::repository::hg_run::TEST_LET_GO_TURN_NS.with(|budget| budget.set(1));
    state.config.step_quantum_ns()
  });
  let (_, texts, pushed) = a_long_push_of_own_texts(&daemon, (port, &certificate), &name);
  let turns = counted_on(&daemon, REPOSITORY, crate::repository::hg_run::LET_GO_TURNS);
  let longest = counted_on(
    &daemon,
    REPOSITORY,
    crate::repository::hg_run::LET_GO_LONGEST_TURN_NS,
  );
  let held = windows_held(&daemon, texts);
  daemon.stop();
  println!("let go over {turns} turns, the longest {longest} ns of a {quantum} ns quantum");
  assert_hg_answered(&pushed);
  assert_eq!(
    turns,
    u64::try_from(TEXTS_CHANGESETS * LET_GO_UNITS_A_CHANGESET).unwrap(),
    "a revision a turn"
  );
  assert!(longest > 0, "the longest turn counted");
  assert!(held.iter().all(|here| !here), "every file's window let go");
}

/// Shape: the revisions a changeset of [`a_long_push_of_own_texts`] lets go, each its own unit of the let-go: its file's,
/// its changeset's text and its manifest's (goal 93, C1 (1) P5: the logs' texts let go with the files').
const LET_GO_UNITS_A_CHANGESET: usize = 3;

/// A region object one byte longer than a window kept on the repository's shard, and a landed changeset whose file is
/// exactly its first window ([`a_region_object_sharing_a_landed_files_window`]): the daemon, its shards, the object and
/// its identity, the push's answer, and whether the shared window was held once landed.
struct SharedWindow {
  /// The daemon.
  daemon: crate::test_store::StoredDaemon,
  /// Its shards.
  shards: Vec<u16>,
  /// The region's object.
  object: Vec<u8>,
  /// Its identity.
  object_id: slates_vcs::ContentId,
  /// The push's answer.
  pushed: (u16, String, Vec<u8>),
  /// Whether the repository's content store held the shared window once the push landed.
  window_held: Option<bool>,
}

/// codex-review's region-object pin (2026-11-22, main's order to w-hg) through a real push: start a two-shard daemon over
/// loopback TLS; on the repository's shard, keep for the region an object one byte longer than a window, its first
/// window ranking on the other shard (so no hold here names it); push one changeset whose file is exactly that first
/// window. What it left.
fn a_region_object_sharing_a_landed_files_window() -> SharedWindow {
  let (identity, certificate) = identity();
  let (daemon, _instance) = daemon_named(Some(identity));
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = hg_name_on_the_repositorys_partition();
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  create_empty_hg(&daemon, &name);
  let (shards, here, window) = crate::daemon::observe_at(&daemon, REPOSITORY, |state| {
    (
      state.shards.clone(),
      state.shard,
      state.store.content.chunk_bytes(),
    )
  });
  let home_of = |bytes: &[u8]| {
    crate::vcs_store::homes::home_of(&shards, &slates_vcs::ContentId::of(bytes)).unwrap()
  };
  // The file: a window of lines, its last line chosen so the window ranks on the other shard.
  let mut file = Vec::with_capacity(window);
  let mut line = 0usize;
  while file.len() < window {
    file.extend_from_slice(format!("shared line {line}\n").as_bytes());
    line += 1;
  }
  file.truncate(window);
  let last = window - 1;
  file[last] = (b'a'..=b'z')
    .find(|tail| {
      file[last] = *tail;
      home_of(&file) != here
    })
    .expect("a window ranking on the other shard");
  // The object: the window and one byte more, homed on the repository's shard.
  let mut object = file.clone();
  object.push(0);
  object[window] = (u8::MIN..=u8::MAX)
    .find(|tail| {
      object[window] = *tail;
      home_of(&object) == here
    })
    .expect("an object homed on the repository's shard");
  let object_id = slates_vcs::ContentId::of(&object);
  let held = object.clone();
  let kept = crate::daemon::observe_at(&daemon, REPOSITORY, move |state| {
    crate::vcs_store::homes::keep_for_region(state, &held, &[crate::vcs_store::homes::TEST_OWNER])
      .map(|(identity, _)| identity)
  });
  assert_eq!(
    kept.ok(),
    Some(object_id),
    "the region's object kept on its home"
  );
  let text: &'static [u8] = Box::leak(file.clone().into_boxed_slice());
  let made = history_of(&[("shared", "shared", text, None, "default")]);
  let pushed = hg_unbundle((port, &certificate), &name, &history_bundle(&made, &[0]));
  let window_held = crate::daemon::observe_at(&daemon, REPOSITORY, move |state| {
    crate::vcs_store::lent_state(state, |state| {
      state.vcs_stores.content().map(|content| {
        content
          .object_bytes(
            &slates_vcs::ContentId::of(text),
            u64::try_from(text.len()).unwrap(),
          )
          .is_some()
      })
    })
  });
  SharedWindow {
    daemon,
    shards,
    object,
    object_id,
    pushed,
    window_held,
  }
}

/// Reads `object_id` from its home among `shards` on the listener's shard (`homes::read`), answered or refused.
fn home_read(
  daemon: &Daemon,
  shards: &[u16],
  object_id: slates_vcs::ContentId,
) -> Result<Vec<u8>, crate::vcs_store::homes::HomeReadRefused> {
  let reading = shards.to_vec();
  crate::test_store::run_at(
    daemon,
    LISTENER,
    move || async move {
      let origin = reading.first().copied().unwrap();
      crate::vcs_store::homes::read(origin, &reading, object_id, u64::MAX).await
    },
    Duration::from_secs(60),
  )
}

/// codex-review's region-object pin through a real push, and goal 93 item 2b(b). Do: as
/// [`a_region_object_sharing_a_landed_files_window`]; once landed, look at the window, then read the object from its home
/// (`homes::read`). Expect the push `200`; the shared window let go of the content store; the home's read the whole
/// object, brought back by the file revision noted with it (`vcs.hg.landed_texts_brought_back` 1). Red before: the
/// window was let go unnoted and the home read none.
#[test]
fn a_region_object_sharing_a_landed_files_window_is_read_whole_from_its_home() {
  let shared = a_region_object_sharing_a_landed_files_window();
  let read = home_read(&shared.daemon, &shared.shards, shared.object_id);
  let brought = counted_on(
    &shared.daemon,
    REPOSITORY,
    crate::repository::hg_push::LANDED_TEXTS_BROUGHT_BACK,
  );
  shared.daemon.stop();
  assert_eq!(
    shared.pushed.0,
    200,
    "{}",
    String::from_utf8_lossy(&shared.pushed.2)
  );
  assert_eq!(
    shared.window_held,
    Some(false),
    "the shared window let go once landed"
  );
  assert_eq!(
    read.map(|bytes| bytes == shared.object),
    Ok(true),
    "the home reads its object whole"
  );
  assert_eq!(
    brought, 1,
    "brought back by the file revision noted with it"
  );
}

/// Each kind of `hg_push::bring_back`'s refusal as the repository's shard counted it, in `BringBackRefused`'s order:
/// not held, store unanswered, no text, content refused, not whole.
fn bring_back_refusals(daemon: &Daemon) -> [u64; 5] {
  use crate::repository::hg_push::{
    BRING_BACK_CONTENT_REFUSED, BRING_BACK_NO_TEXT, BRING_BACK_NOT_HELD, BRING_BACK_NOT_WHOLE,
    BRING_BACK_STORE_UNANSWERED,
  };
  [
    BRING_BACK_NOT_HELD,
    BRING_BACK_STORE_UNANSWERED,
    BRING_BACK_NO_TEXT,
    BRING_BACK_CONTENT_REFUSED,
    BRING_BACK_NOT_WHOLE,
  ]
  .map(|kind| counted_on(daemon, REPOSITORY, kind))
}

/// The typed refusal a home's read passes on (main 01a20182's ruling of 2026-11-23 21:25; §2 item 9). Do: as
/// [`a_region_object_sharing_a_landed_files_window_is_read_whole_from_its_home`], then hold back the repository's
/// shard's whole room, every byte it holds or could claim (§4.2: a host under memory pressure), read the object from its
/// home, release the hold and read it again. Expect: the first read refused `NotBroughtBack` with the revision's
/// bring-back refused by the content store's room (`OverBudget`, a window's need), counted once under that kind and
/// under no other; the second read the whole object, brought back once, nothing more counted refused. Red before: the
/// read answered a bare none and no counter named the kind.
#[test]
fn a_homes_read_whose_bring_back_is_refused_room_fails_with_that_refusal_counted_once() {
  use crate::repository::hg_push::BringBackRefused;
  use crate::vcs_store::homes::{HomeReadRefused, LandedRefused};
  let shared = a_region_object_sharing_a_landed_files_window();
  crate::daemon::observe_at(&shared.daemon, REPOSITORY, |state| {
    state.pressure_pinned = true;
    let claimable = u64::try_from(state.store.content.arena().claimable()).unwrap();
    let all = state.store.budget.capacity().saturating_add(claimable);
    state.store.budget.set_hold(all);
  });
  let refused = home_read(&shared.daemon, &shared.shards, shared.object_id);
  let counted_refused = bring_back_refusals(&shared.daemon);
  crate::daemon::observe_at(&shared.daemon, REPOSITORY, |state| {
    state.store.budget.set_hold(0);
  });
  let read = home_read(&shared.daemon, &shared.shards, shared.object_id);
  let counted_after = bring_back_refusals(&shared.daemon);
  let brought = counted_on(
    &shared.daemon,
    REPOSITORY,
    crate::repository::hg_push::LANDED_TEXTS_BROUGHT_BACK,
  );
  shared.daemon.stop();
  let window = u64::try_from(shared.object.len() - 1).unwrap();
  assert!(
    matches!(
      refused,
      Err(HomeReadRefused::NotBroughtBack(LandedRefused::Revision(
        BringBackRefused::Content(slates_vcs::ContentRefused::OverBudget { need, .. })
      ))) if need == window
    ),
    "the home's read refused with the bring-back's room refusal: {:?}",
    refused.map(|bytes| bytes.len())
  );
  assert_eq!(
    counted_refused,
    [0, 0, 0, 1, 0],
    "counted once, under the content store's kind alone"
  );
  assert_eq!(
    read.map(|bytes| bytes == shared.object),
    Ok(true),
    "the hold released, the home reads its object whole"
  );
  assert_eq!(
    counted_after, counted_refused,
    "the read after the release refused nothing"
  );
  assert_eq!(brought, 1, "brought back once, after the release");
}

/// w-shared's seam 1 of goal 93 item 2b(b) (main's ruling of 2026-11-22 18:43): a home reads a landed file revision by
/// its (path, node). Do: start a two-shard daemon over loopback TLS; push [`a_long_push_of_own_texts`]'s line; publish
/// the repository's shard, which gives the arena's idle extents back to the pool as the sweep's publication may at any
/// moment ([`published_down_to_demand`]); on that shard, bring each file revision back by its path and node
/// (`hg_push::bring_back`), then look at its window again. Expect the push `200`; some window let go; every bring-back
/// answered `Ok`; every window held whole after, each let go read from the changeset store and counted once; and every
/// bring-back's room made outside a lend, where the pool is reachable: each let go brought back once
/// (`vcs.hg.landed_texts_brought_back`, the non-vacuity count) and no room asked inside a lend
/// (`vcs_store.room_out_of_reach` 0), whatever slack this machine's publication left. Red before: none could be brought
/// back by its (path, node); then, with the publication (w-tsan, 2026-11-23), 10 runs of 10 refused
/// `OverBudget { need: 8192, room: 5869 }` from the 49th file, the room asked inside `lent_state`, where the pool is out
/// of reach (plainly 2 runs of 100 at a load of 47, at the 49th and the 81st). A first form asserted the arena grew past
/// the publication, which held only where the slack was short: on w-hg's MacBook the publication left 16 MiB, room for
/// all 120, and it failed 3 runs of 3 with every bring-back answered.
#[test]
fn a_landed_file_revisions_let_go_windows_are_brought_back_whole_by_its_path_and_node() {
  let (identity, certificate) = identity();
  let (daemon, _instance) = daemon_named(Some(identity));
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = hg_name_on_the_repositorys_partition();
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  create_empty_hg(&daemon, &name);
  let (made, texts, pushed) = a_long_push_of_own_texts(&daemon, (port, &certificate), &name);
  assert_hg_answered(&pushed);
  let named = name.clone();
  let repository = crate::daemon::observe_at(&daemon, REPOSITORY, move |state| {
    state
      .db
      .partition()
      .repositories()
      .repository_named(&named)
      .map(|record| record.id)
  })
  .expect("the repository's record");
  let before = windows_held(&daemon, texts);
  let published = published_down_to_demand(&daemon);
  let files: Vec<(Vec<u8>, slates_hg::Node)> = made
    .iter()
    .map(|made| (made.file.0.clone(), made.file.1))
    .collect();
  let brought = crate::test_store::run_at(
    &daemon,
    REPOSITORY,
    move || async move {
      let mut brought = Vec::with_capacity(files.len());
      for file in files {
        brought.push(crate::repository::hg_push::bring_back(repository, file).await);
      }
      brought
    },
    Duration::from_secs(60),
  );
  let after = windows_held(&daemon, texts);
  let counted = counted_on(
    &daemon,
    REPOSITORY,
    crate::repository::hg_push::LANDED_TEXTS_BROUGHT_BACK,
  );
  let grown = crate::daemon::observe_at(&daemon, REPOSITORY, |state| state.store.budget.capacity());
  let out_of_reach = counted_on(&daemon, REPOSITORY, crate::vcs_store::ROOM_OUT_OF_REACH);
  daemon.stop();
  let gone = before.iter().filter(|here| !**here).count();
  println!(
    "{gone} of {TEXTS_CHANGESETS} files' windows let go before; {counted} brought back; the shard's capacity \
     {published} after the publication, {grown} after"
  );
  assert!(gone > 0, "some window let go");
  for (at, (brought, here)) in brought.iter().zip(&after).enumerate() {
    assert_eq!(*brought, Ok(()), "file {at} brought back");
    assert!(here, "file {at}'s window held whole after");
  }
  assert_eq!(
    out_of_reach, 0,
    "no room asked inside a lend: each bring-back's room made where the pool is reachable"
  );
  assert_eq!(
    counted,
    u64::try_from(gone).unwrap(),
    "each let go brought back once"
  );
}

/// Goal 93, C1 (1) P1 (main's ruling of 2026-11-23 20:53: the changelog's and the manifests' texts off the content
/// store, brought back through the node's changeset store; a manifest delta made again from its two texts). Do: start a
/// two-shard daemon over loopback TLS; push [`a_long_push_of_own_texts`]'s line; on the repository's shard, let go every
/// object the repository's changelog and manifest rows name; bring each revision back by its log's path and its node
/// (`hg_push::bring_back`) after a publication gave the arena's idle extents back ([`published_down_to_demand`]); read
/// every text again. Expect the push `200`; objects let go (the non-vacuity count); every bring-back answered `Ok`;
/// every changeset's and manifest's text read back hashing to its node (the landing let them go before any was read
/// here, since P5); each let go counted once (`vcs.hg.landed_log_texts_brought_back`); no room asked inside a lend. Red
/// before: a bring-back by those paths looked for a file revision.
#[test]
fn a_landed_changelog_and_manifest_revision_let_go_is_brought_back_by_its_log_and_node() {
  let (identity, certificate) = identity();
  let (daemon, _instance) = daemon_named(Some(identity));
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = hg_name_on_the_repositorys_partition();
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  create_empty_hg(&daemon, &name);
  let (_, _, pushed) = a_long_push_of_own_texts(&daemon, (port, &certificate), &name);
  assert_hg_answered(&pushed);
  let named = name.clone();
  let repository = crate::daemon::observe_at(&daemon, REPOSITORY, move |state| {
    state
      .db
      .partition()
      .repositories()
      .repository_named(&named)
      .map(|record| record.id)
  })
  .expect("the repository's record");
  let before = log_texts(&daemon, repository);
  let let_go = let_go_logs(&daemon, repository);
  published_down_to_demand(&daemon);
  let asks: Vec<(Vec<u8>, slates_hg::Node)> = before
    .0
    .iter()
    .map(|(node, _)| (slates_hg::push::intake::CHANGELOG_PATH.to_vec(), *node))
    .chain(
      before
        .1
        .iter()
        .map(|(node, _)| (slates_hg::push::intake::MANIFEST_PATH.to_vec(), *node)),
    )
    .collect();
  let brought = crate::test_store::run_at(
    &daemon,
    REPOSITORY,
    move || async move {
      let mut brought = Vec::with_capacity(asks.len());
      for ask in asks {
        brought.push(crate::repository::hg_push::bring_back(repository, ask).await);
      }
      brought
    },
    Duration::from_secs(60),
  );
  let after = log_texts(&daemon, repository);
  let daemon_rows = log_rows(&daemon, repository);
  let counted = counted_on(
    &daemon,
    REPOSITORY,
    crate::repository::hg_push::LANDED_LOG_TEXTS_BROUGHT_BACK,
  );
  let out_of_reach = counted_on(&daemon, REPOSITORY, crate::vcs_store::ROOM_OUT_OF_REACH);
  daemon.stop();
  println!(
    "{let_go} of {} changelog and manifest objects let go; {counted} brought back",
    brought.len()
  );
  assert!(let_go > 0, "objects let go");
  assert!(brought.iter().all(Result::is_ok), "{brought:?}");
  assert_eq!(out_of_reach, 0, "no room asked inside a lend");
  let read = |texts: &LogTexts| texts.iter().filter(|(_, text)| text.is_some()).count();
  assert_eq!(
    (read(&after.0), read(&after.1)),
    (before.0.len(), before.1.len()),
    "every text read back"
  );
  assert_eq!(
    log_texts_hashing_to_their_nodes(&daemon_rows),
    before.0.len() + before.1.len(),
    "every text read back hashes to its node"
  );
  assert_eq!(
    counted,
    u64::try_from(let_go).unwrap(),
    "each let go brought back once"
  );
}

/// Shape: the line the let-go work test pushes: all but its last changeset first, then its last alone.
const LET_GO_LINE: usize = 7;

/// Goal 93, C1 (1) P3 (main's ruling of 2026-11-23 20:53: the changelog's and the manifests' texts off the content
/// store, a push's work reading one not at hand through the changeset store). Do: start a two-shard daemon over
/// loopback TLS; push a line of [`LET_GO_LINE`] changesets but its last; let go every object the repository's changelog
/// and manifest rows name; push the last changeset alone, its work's base the line's head, whose changeset and manifest
/// were let go. Expect both pushes `200`; objects let go; the second push's work read texts from the changeset store
/// (`vcs.hg.push_work_log_texts_read`) and the line's head the last changeset after. Red before: the second push
/// refused in its reply, `Index(Unread { rev: 5 })`: its intake could not read the previous manifest it deltas
/// against (P4), and past that its work's layout would find no manifest (P3).
#[test]
fn a_push_whose_works_base_was_let_go_reads_it_from_the_changeset_store() {
  let (identity, certificate) = identity();
  let (daemon, _instance) = daemon_named(Some(identity));
  let port = daemon.vcs_http_port().expect("the edge started");
  let edge = (port, &certificate);
  let name = hg_name_on_the_repositorys_partition();
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  create_empty_hg(&daemon, &name);
  let paths: &'static [String] = Box::leak(
    (0..LET_GO_LINE)
      .map(|at| format!("line-{at}"))
      .collect::<Vec<_>>()
      .into_boxed_slice(),
  );
  let commits: Vec<Commit> = (0..LET_GO_LINE)
    .map(|at| {
      (
        paths[at].as_str(),
        paths[at].as_str(),
        paths[at].as_bytes(),
        at.checked_sub(1),
        "default",
      )
    })
    .collect();
  let made = history_of(&commits);
  let first: Vec<usize> = (0..LET_GO_LINE - 1).collect();
  let pushed = hg_unbundle(edge, &name, &history_bundle(&made, &first));
  let named = name.clone();
  let repository = crate::daemon::observe_at(&daemon, REPOSITORY, move |state| {
    state
      .db
      .partition()
      .repositories()
      .repository_named(&named)
      .map(|record| record.id)
  })
  .expect("the repository's record");
  let let_go = let_go_logs(&daemon, repository);
  let last = hg_unbundle(edge, &name, &history_bundle(&made, &[LET_GO_LINE - 1]));
  let read = counted_on(
    &daemon,
    REPOSITORY,
    crate::repository::hg_push::WORK_LOG_TEXTS_READ,
  );
  let heads = hg_command(edge, &name, "heads");
  daemon.stop();
  assert_hg_answered(&pushed);
  assert!(let_go > 0, "objects let go");
  assert_hg_answered(&last);
  assert!(
    !String::from_utf8_lossy(&last.2).contains("ERROR:ABORT"),
    "the last push accepted: {}",
    String::from_utf8_lossy(&last.2)
  );
  assert!(read > 0, "the work read its base from the changeset store");
  let tip = crate::verbs::hex_of(&made[LET_GO_LINE - 1].node);
  assert_eq!(heads, (200, format!("{tip}\n").into_bytes()));
}

/// Goal 93, C1 (1) P5 (main's order of 2026-11-24: the changelog's and the manifests' texts leave the content store once
/// landed, an import's as a push's). Do: start a two-shard daemon over loopback TLS; import the `states` repository into
/// one Mercurial repository and land it; into another, empty, push a line of [`LET_GO_LINE`] changesets but its last,
/// then its last alone; look at every changelog and manifest row's object in the repositories' shard's content store;
/// clone each whole (`getbundle`), and pull the second's last changeset over its line before it. Expect every such
/// object let go (no window of it held), before the clones and after; each clone's and the pull's changegroup verified
/// by the push's own verifier, every text hashing to its node (the pull's over what the line's clone brought); the
/// second push's intake asked its texts of the node's changeset store, the parent manifest's read-back counted apart;
/// the pull's changelog and manifest texts read whole counted as such (`vcs_store.hg_pull_log_texts`), never as file
/// revisions sent whole (`vcs_store.hg_pull_whole_texts` unmoved: main's hg_interop round at a30ffbdb8,
/// `a_pull_of_one_changeset_sends_its_large_files_stored_delta_as_stock_does`, counted 3). Red before: the import and the
/// push kept every changelog and manifest object whole.
#[test]
fn landed_changelog_and_manifest_texts_leave_the_content_store_and_a_clone_and_a_pull_verify() {
  let (identity, certificate) = identity();
  let (daemon, _instance) = daemon_named(Some(identity));
  let port = daemon.vcs_http_port().expect("the edge started");
  let edge = (port, &certificate);
  let names = hg_names_on_the_repositorys_partition(2);
  let (imported, pushed) = (names[0].clone(), names[1].clone());
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  create_hg_from_states(&daemon, &imported);
  create_empty_hg(&daemon, &pushed);
  let paths: &'static [String] = Box::leak(
    (0..LET_GO_LINE)
      .map(|at| format!("line-{at}"))
      .collect::<Vec<_>>()
      .into_boxed_slice(),
  );
  let commits: Vec<Commit> = (0..LET_GO_LINE)
    .map(|at| {
      (
        paths[at].as_str(),
        paths[at].as_str(),
        paths[at].as_bytes(),
        at.checked_sub(1),
        "default",
      )
    })
    .collect();
  let made = history_of(&commits);
  let first: Vec<usize> = (0..LET_GO_LINE - 1).collect();
  let line = hg_unbundle(edge, &pushed, &history_bundle(&made, &first));
  let last = hg_unbundle(edge, &pushed, &history_bundle(&made, &[LET_GO_LINE - 1]));
  let ids = [imported.clone(), pushed.clone()].map(|name| repository_named(&daemon, name));
  let held_before = ids.map(|id| log_windows_held(&daemon, id));
  let heads = hg_command(edge, &imported, "heads");
  let imported_heads = String::from_utf8_lossy(&heads.1)
    .split_whitespace()
    .collect::<Vec<_>>()
    .join("+");
  let null = "0".repeat(40);
  let (before, tip) = (
    crate::verbs::hex_of(&made[LET_GO_LINE - 2].node),
    crate::verbs::hex_of(&made[LET_GO_LINE - 1].node),
  );
  let imported_clone = hg_getbundle_over(edge, &imported, (&null, &imported_heads));
  let line_clone = hg_getbundle_over(edge, &pushed, (&null, &before));
  let clone_before = clone_reads(&daemon);
  let whole_clone = hg_getbundle_over(edge, &pushed, (&null, &tip));
  let clone_read = clone_reads(&daemon);
  let whole_before = counted_on(
    &daemon,
    REPOSITORY,
    crate::vcs_store::hg_view::PULL_WHOLE_TEXTS,
  );
  let logs_before = counted_on(
    &daemon,
    REPOSITORY,
    crate::vcs_store::hg_view::PULL_LOG_TEXTS,
  );
  let pull = hg_getbundle_over(edge, &pushed, (&before, &tip));
  let pulled_whole = counted_on(
    &daemon,
    REPOSITORY,
    crate::vcs_store::hg_view::PULL_WHOLE_TEXTS,
  )
  .saturating_sub(whole_before);
  let pulled_logs = counted_on(
    &daemon,
    REPOSITORY,
    crate::vcs_store::hg_view::PULL_LOG_TEXTS,
  )
  .saturating_sub(logs_before);
  let held_after = ids.map(|id| log_windows_held(&daemon, id));
  let files_after = file_windows_held(&daemon, ids[1]);
  let work_texts = counted_on(
    &daemon,
    REPOSITORY,
    crate::repository::hg_push::WORK_TEXTS_READ,
  );
  let asked = counted_on(
    &daemon,
    REPOSITORY,
    crate::repository::hg_push::LOG_TEXTS_ASKED,
  );
  let previous = counted_on(
    &daemon,
    REPOSITORY,
    crate::repository::hg_push::PREVIOUS_MANIFESTS_ASKED,
  );
  daemon.stop();
  println!(
    "log objects held before {held_before:?}, after {held_after:?}; the pushes asked {asked} texts, {previous} of them \
     the parent manifest"
  );
  assert_hg_answered(&line);
  assert_hg_answered(&last);
  assert_log_rows_kept_and_no_text_held(&held_before, &held_after);
  let nothing = NothingHeld;
  let imported_verified = verified_clone(&imported_clone, &nothing);
  assert!(
    !imported_verified.changesets.is_empty(),
    "the import's clone brings changesets"
  );
  let whole = verified_clone(&whole_clone, &nothing);
  assert_eq!(
    whole.changesets.len(),
    LET_GO_LINE,
    "the line's clone brings every changeset"
  );
  let line_verified = verified_clone(&line_clone, &nothing);
  let pulled = verified_clone(&pull, &ClonedHeld(&line_verified));
  assert_eq!(
    pulled
      .changesets
      .iter()
      .map(|arrived| arrived.node)
      .collect::<Vec<_>>(),
    vec![made[LET_GO_LINE - 1].node],
    "the pull brings the last changeset"
  );
  assert!(
    asked > 0,
    "the last push's intake asked its texts of the store"
  );
  assert_eq!(pulled_whole, 0, "no file revision the pull sent whole");
  assert!(
    pulled_logs > 0,
    "the pull read its changeset's and manifests' texts whole from the store"
  );
  assert_clone_reads_its_texts_in_batches(clone_before, clone_read);
  assert_the_pushes_file_texts_let_go(work_texts, files_after);
}

/// Every repository keeps changelog and manifest rows, and holds none of their texts, before the clones and after
/// (`log_windows_held`'s rows, windows held and bytes, one a repository).
fn assert_log_rows_kept_and_no_text_held(
  before: &[(usize, usize, u64)],
  after: &[(usize, usize, u64)],
) {
  for (rows, held, bytes) in before.iter().chain(after) {
    assert!(
      *rows > 0,
      "the repository keeps changelog and manifest rows"
    );
    assert_eq!(
      (*held, *bytes),
      (0, 0),
      "no changelog or manifest text held"
    );
  }
}

/// The last push's work read a let-go file text back (`work_texts`, `hg_push::WORK_TEXTS_READ`), and no landed file
/// text is held once the pushes end (`file_windows_held`'s windows and bytes).
fn assert_the_pushes_file_texts_let_go(work_texts: u64, files_after: (usize, usize, u64)) {
  println!(
    "file objects held after the pushes {files_after:?}; the works read {work_texts} texts back"
  );
  assert!(
    work_texts > 0,
    "the last push's work read a let-go file text back"
  );
  assert_eq!(
    (files_after.1, files_after.2),
    (0, 0),
    "no landed file text held once the pushes end"
  );
}

/// What a daemon's fetches have read of the changeset store so far: the changelog's and manifests' texts read whole
/// (`vcs_store.hg_pull_log_texts`) and the store's asks (`vcs_store.hg_stream_store_asks`).
fn clone_reads(daemon: &crate::daemon::Daemon) -> (u64, u64) {
  (
    counted_on(
      daemon,
      REPOSITORY,
      crate::vcs_store::hg_view::PULL_LOG_TEXTS,
    ),
    counted_on(daemon, REPOSITORY, crate::vcs_store::hg_view::STORE_ASKS),
  )
}

/// Goal 93 (main's interop bench at ed71ac0be: `hg clone` of the 57k history after P5 ran 13.5 minutes, stock's 42.4 s;
/// the daemon's sample a store scan a text). A clone of the whole line, its changelog's and manifests' texts let go,
/// reads each of the line's changesets and manifests once, and asks the store in batches: a want of rows and one of texts
/// (a file's, its chunks) for each of the changelog, the manifests and the files, within the fetch's room. Red before:
/// 30 asks, a want of a row and one of its text for every changeset and manifest.
fn assert_clone_reads_its_texts_in_batches(before: (u64, u64), after: (u64, u64)) {
  let texts = after.0.saturating_sub(before.0);
  let asks = after.1.saturating_sub(before.1);
  println!("a clone of {LET_GO_LINE} changesets read {texts} log texts in {asks} store asks");
  assert_eq!(
    texts,
    2 * u64::try_from(LET_GO_LINE).unwrap(),
    "each changeset's and manifest's text read once"
  );
  assert!(
    asks <= 6,
    "a clone asked the store {asks} times for {texts} texts: a batch of rows and of texts a log"
  );
}

/// A repository holding nothing: what a clone's changegroup is verified against.
struct NothingHeld;

impl slates_hg::push::verify::Held for NothingHeld {
  fn changeset_text(&self, _node: &slates_hg::Node) -> Option<std::borrow::Cow<'_, [u8]>> {
    None
  }

  fn manifest_text(&self, _node: &slates_hg::Node) -> Option<std::borrow::Cow<'_, [u8]>> {
    None
  }

  fn file_text(&self, _path: &[u8], _node: &slates_hg::Node) -> Option<std::borrow::Cow<'_, [u8]>> {
    None
  }

  fn stores_bytes(&self) -> bool {
    false
  }
}

/// What a verified clone brought, as a pull over it is verified against.
struct ClonedHeld<'v>(&'v slates_hg::push::verify::Verified);

impl slates_hg::push::verify::Held for ClonedHeld<'_> {
  fn changeset_text(&self, node: &slates_hg::Node) -> Option<std::borrow::Cow<'_, [u8]>> {
    self
      .0
      .changeset(node)
      .map(|arrived| std::borrow::Cow::Borrowed(arrived.text.as_slice()))
  }

  fn manifest_text(&self, node: &slates_hg::Node) -> Option<std::borrow::Cow<'_, [u8]>> {
    self
      .0
      .manifest(node)
      .map(|arrived| std::borrow::Cow::Borrowed(arrived.text.as_slice()))
  }

  fn file_text(&self, path: &[u8], node: &slates_hg::Node) -> Option<std::borrow::Cow<'_, [u8]>> {
    self
      .0
      .file(path, node)
      .map(|arrived| std::borrow::Cow::Borrowed(arrived.text.as_slice()))
  }

  fn stores_bytes(&self) -> bool {
    false
  }
}

/// The changegroup of `answer` (a `getbundle` answered `200`, uncompressed) verified against `held` by the push's own
/// verifier: every text rebuilt and hashed to its node.
fn verified_clone(
  answer: &(u16, String, Vec<u8>),
  held: &dyn slates_hg::push::verify::Held,
) -> slates_hg::push::verify::Verified {
  assert_eq!(answer.0, 200, "{}", String::from_utf8_lossy(&answer.2));
  let stream = answer
    .2
    .strip_prefix(b"\x04none")
    .expect("an uncompressed answer");
  let mut room = slates_hg::Admission::new(u64::MAX);
  let bundle = Bundle2::decode(stream, &mut room).expect("a bundle2");
  let (index, part) = bundle
    .top_level()
    .find(|(_, part)| part.is(b"changegroup"))
    .expect("a changegroup part");
  let version =
    slates_hg::changegroup::Version::from_part_param(part.param(b"version")).expect("a version");
  let part = slates_hg::push::ChangegroupPart {
    version,
    changesets: None,
    target_phase: None,
    tree_manifests: false,
    payload: bundle.payload_bytes(index, &mut room).expect("its payload"),
  };
  slates_hg::push::verify::verify(&part, held, &mut room).expect("every text hashes to its node")
}

/// The id of the repository named `name` on [`REPOSITORY`]'s partition.
fn repository_named(daemon: &Daemon, name: String) -> slates_db::catalog::RepositoryId {
  crate::daemon::observe_at(daemon, REPOSITORY, move |state| {
    state
      .db
      .partition()
      .repositories()
      .repository_named(&name)
      .map(|record| record.id)
  })
  .expect("the repository's record")
}

/// `repository`'s changelog and manifest rows on its shard: how many, how many of their objects with any bytes have any
/// window held in its content store, and those objects' bytes. The empty window (a manifest kept as an empty delta) is
/// never let go and holds no bytes (`LetGoRefused::Empty`), so it is not counted.
fn log_windows_held(
  daemon: &Daemon,
  repository: slates_db::catalog::RepositoryId,
) -> (usize, usize, u64) {
  crate::daemon::observe_at(daemon, REPOSITORY, move |state| {
    crate::vcs_store::lent_state(state, |state| {
      let (content, store) = state.vcs_stores.mercurial_ref(repository)?;
      let index = &store.index;
      let changesets = (0..index.len(content, slates_hg::index::Log::Changelog))
        .map(|rev| index.changeset(content, rev).map(|row| (row.text, row.len)));
      let manifests = (0..index.len(content, slates_hg::index::Log::Manifest)).map(|rev| {
        index
          .revision(content, slates_hg::index::Log::Manifest, rev)
          .map(|row| (row.bytes, row.len))
      });
      let objects = changesets.chain(manifests).collect::<Option<Vec<_>>>()?;
      let held: Vec<u64> = objects
        .iter()
        .filter(|(object, _)| content.len_of(object).is_some_and(|len| len > 0))
        .filter(|(object, _)| {
          content
            .object_layout(object)
            .iter()
            .any(|(window, _)| !content.is_away(window))
        })
        .map(|(_, len)| *len)
        .collect();
      Some((objects.len(), held.len(), held.iter().sum()))
    })
  })
  .expect("the repository's rows")
}

/// `repository`'s file revisions on its shard: how many, how many of their bytes' objects with any bytes have a window
/// held in its content store, and those objects' bytes (as [`log_windows_held`] counts the logs').
fn file_windows_held(
  daemon: &Daemon,
  repository: slates_db::catalog::RepositoryId,
) -> (usize, usize, u64) {
  crate::daemon::observe_at(daemon, REPOSITORY, move |state| {
    crate::vcs_store::lent_state(state, |state| {
      let (content, store) = state.vcs_stores.mercurial_ref(repository)?;
      let index = &store.index;
      let mut objects = Vec::new();
      let paths: Vec<Vec<u8>> = index.file_logs(content).map(<[u8]>::to_vec).collect();
      for path in &paths {
        let log = slates_hg::index::Log::File(path);
        for rev in 0..index.len(content, log) {
          let row = index.revision(content, log, rev)?;
          objects.push(row.bytes);
        }
      }
      let held: Vec<u64> = objects
        .iter()
        .filter(|object| content.len_of(object).is_some_and(|len| len > 0))
        .filter(|object| {
          content
            .object_layout(object)
            .iter()
            .any(|(window, _)| !content.is_away(window))
        })
        .map(|object| content.len_of(object).unwrap_or(0))
        .collect();
      Some((objects.len(), held.len(), held.iter().sum()))
    })
  })
  .expect("the repository's file rows")
}

/// Lets go every object `repository`'s changelog and manifest rows name on its shard: how many of those with any bytes
/// are let go after, by the landing (goal 93, C1 (1) P5: once its push committed) or here.
fn let_go_logs(daemon: &Daemon, repository: slates_db::catalog::RepositoryId) -> usize {
  crate::daemon::observe_at(daemon, REPOSITORY, move |state| {
    crate::vcs_store::lent_state(state, |state| {
      crate::vcs_store::within_budget(state, 0, |stores| {
        let (content, store) = stores.mercurial(repository)?;
        let index = &store.index;
        let changesets = (0..index.len(content, slates_hg::index::Log::Changelog))
          .map(|rev| index.changeset(content, rev).map(|row| row.text));
        let manifests = (0..index.len(content, slates_hg::index::Log::Manifest)).map(|rev| {
          index
            .revision(content, slates_hg::index::Log::Manifest, rev)
            .map(|row| row.bytes)
        });
        let identities = changesets.chain(manifests).collect::<Option<Vec<_>>>()?;
        for identity in &identities {
          content.let_go_object(identity);
        }
        Some(
          identities
            .iter()
            .filter(|identity| content.len_of(identity).is_some_and(|len| len > 0))
            .filter(|identity| {
              let layout = content.object_layout(identity);
              !layout.is_empty() && layout.iter().all(|(window, _)| content.is_away(window))
            })
            .count(),
        )
      })
    })
  })
  .expect("the repository's rows")
}

/// One changelog or manifest revision as [`log_rows`] reads it: its node, its parents' nodes, and its text read from the
/// content store (none where not held).
type LogRow = (
  slates_hg::Node,
  (slates_hg::Node, slates_hg::Node),
  Option<Vec<u8>>,
);

/// Each changelog and manifest revision of `repository` as its shard holds it ([`LogRow`]).
fn log_rows(daemon: &Daemon, repository: slates_db::catalog::RepositoryId) -> Vec<LogRow> {
  crate::daemon::observe_at(daemon, REPOSITORY, move |state| {
    crate::vcs_store::lent_state(state, |state| {
      crate::vcs_store::within_budget(state, 0, |stores| {
        let (content, store) = stores.mercurial(repository)?;
        let index = &store.index;
        let mut rows = Vec::new();
        for log in [
          slates_hg::index::Log::Changelog,
          slates_hg::index::Log::Manifest,
        ] {
          let parent = |rev: u32| match log {
            _ if rev == slates_hg::index::NONE => Some(slates_hg::NULL_NODE),
            slates_hg::index::Log::Changelog => index.changeset(content, rev).map(|row| row.node),
            _ => index.revision(content, log, rev).map(|row| row.node),
          };
          for rev in 0..index.len(content, log) {
            let (node, p1, p2, text) = match log {
              slates_hg::index::Log::Changelog => {
                let row = index.changeset(content, rev)?;
                (
                  row.node,
                  row.p1,
                  row.p2,
                  content.object_bytes(&row.text, row.len),
                )
              }
              _ => {
                let row = index.revision(content, log, rev)?;
                (
                  row.node,
                  row.p1,
                  row.p2,
                  index.revision_text(content, log, rev),
                )
              }
            };
            rows.push((node, (parent(p1)?, parent(p2)?), text));
          }
        }
        Some(rows)
      })
    })
  })
  .expect("the repository's rows")
}

/// How many of `rows` ([`log_rows`]) have a text that hashes, with their parents, to their node.
fn log_texts_hashing_to_their_nodes(rows: &[LogRow]) -> usize {
  rows
    .iter()
    .filter(|(node, (p1, p2), text)| {
      text
        .as_deref()
        .is_some_and(|text| slates_hg::node::compute(p1, p2, text).ok() == Some(*node))
    })
    .count()
}

/// A log's revisions as [`log_texts`] reads them: each one's node and its text, none where it is not held.
type LogTexts = Vec<(slates_hg::Node, Option<Vec<u8>>)>;

/// Every changeset's and every manifest's (node, text) of `repository`, read from its shard's content store (a text
/// not held reads as none).
fn log_texts(
  daemon: &Daemon,
  repository: slates_db::catalog::RepositoryId,
) -> (LogTexts, LogTexts) {
  crate::daemon::observe_at(daemon, REPOSITORY, move |state| {
    crate::vcs_store::lent_state(state, |state| {
      crate::vcs_store::within_budget(state, 0, |stores| {
        let (content, store) = stores.mercurial(repository)?;
        let index = &store.index;
        let changesets = (0..index.len(content, slates_hg::index::Log::Changelog))
          .map(|rev| {
            let row = index.changeset(content, rev)?;
            Some((row.node, content.object_bytes(&row.text, row.len)))
          })
          .collect::<Option<Vec<_>>>()?;
        let manifests = (0..index.len(content, slates_hg::index::Log::Manifest))
          .map(|rev| {
            let row = index.revision(content, slates_hg::index::Log::Manifest, rev)?;
            Some((
              row.node,
              index.revision_text(content, slates_hg::index::Log::Manifest, rev),
            ))
          })
          .collect::<Option<Vec<_>>>()?;
        Some((changesets, manifests))
      })
    })
  })
  .expect("the repository's rows")
}

/// Publishes the repository's shard and answers its capacity after. A committed publication gives the arena's wholly free
/// extents back to the pool (`Store::release_idle`), as the sweep's may at any moment, so what follows grows from the
/// pool for its room, never only from the slack a push left behind (w-tsan, 2026-11-23: a bring-back that met the slack
/// alone was refused at the 49th of 120 files).
fn published_down_to_demand(daemon: &Daemon) -> u64 {
  crate::daemon::observe_at(daemon, REPOSITORY, |state| {
    crate::verbs::publish_shard(state).expect("the repository's shard published");
    state.store.budget.capacity()
  })
}

/// Whether the repository's shard's content store reads each of `texts`' windows (none for one let go).
fn windows_held(daemon: &Daemon, texts: &'static [Vec<u8>]) -> Vec<bool> {
  crate::daemon::observe_at(daemon, REPOSITORY, move |state| {
    crate::vcs_store::lent_state(state, |state| {
      let content = state.vcs_stores.content()?;
      Some(
        texts
          .iter()
          .map(|text| {
            content
              .object_bytes(
                &slates_vcs::ContentId::of(text),
                u64::try_from(text.len()).unwrap(),
              )
              .is_some()
          })
          .collect::<Vec<_>>(),
      )
    })
  })
  .expect("the repository's content store")
}

/// Main 01a20182's ruling of 2026-11-22 02:52 (C1 2b, an import keeping no file text once its segments are durable) and
/// order of 14:00 (the arena a landed history left held). Do: start a two-shard daemon over loopback TLS; import the
/// `states` repository into a Mercurial repository and land it; look at each of its file revisions' windows in its
/// shard's content store; clone it whole (`getbundle`). Expect every file revision's window let go of the content store,
/// its entry kept, none pinned and none noted, since no home holds any (goal 93 item 2b(b): before, those a home held
/// here were pinned, 1 of 5; then every window was noted); counted; and the clone `200` with a changegroup, its file texts brought back
/// from the changeset store. Red before: the import kept every text.
#[test]
fn an_imported_repositorys_file_texts_leave_the_content_store_once_its_segments_land() {
  let (identity, certificate) = identity();
  let (daemon, _instance) = daemon_named(Some(identity));
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = hg_name_on_the_repositorys_partition();
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  create_hg_from_states(&daemon, &name);
  let looked = {
    let name = name.clone();
    crate::daemon::observe_at(&daemon, REPOSITORY, move |state| {
      let id = state
        .db
        .partition()
        .repositories()
        .repository_named(&name)?
        .id;
      crate::vcs_store::lent_state(state, |state| {
        let (content, store) = state.vcs_stores.mercurial_ref(id)?;
        let mut seen = Vec::new();
        let mut at = (Vec::new(), 0_u32);
        while let Some((path, rev, row)) = store.index.file_revision_from(content, (&at.0, at.1)) {
          let noted = content
            .object_layout(&row.bytes)
            .iter()
            .all(|(window, _)| crate::vcs_store::homes::landed(window).is_some());
          let pinned = crate::vcs_store::landed_pin(&row.bytes).is_some();
          let len = content.len_of(&row.bytes);
          let here = len.is_some_and(|len| content.object_bytes(&row.bytes, len).is_some());
          seen.push((here, (noted, pinned), len.is_some()));
          at = (path, rev.saturating_add(1));
        }
        Some(seen)
      })
    })
  }
  .expect("the imported repository's store");
  let let_go = counted_on(
    &daemon,
    REPOSITORY,
    crate::vcs_store::hg_import::FILE_TEXT_BYTES_LET_GO,
  );
  let heads = hg_command((port, &certificate), &name, "heads");
  let tip = String::from_utf8_lossy(&heads.1)
    .split_whitespace()
    .next()
    .unwrap_or_default()
    .to_owned();
  let cloned = hg_getbundle((port, &certificate), &name, &tip);
  daemon.stop();
  let gone = looked.iter().filter(|(here, ..)| !here).count();
  println!(
    "an import's file texts: {gone} of {} windows let go, {let_go} bytes",
    looked.len()
  );
  assert!(!looked.is_empty(), "the import keeps file revisions");
  for (at, (here, (noted, pinned), entry)) in looked.iter().enumerate() {
    assert!(entry, "file revision {at}'s entry kept");
    assert!(!here, "file revision {at}'s window let go");
    assert!(
      !*noted && !*pinned,
      "file revision {at}'s windows neither noted nor pinned: no home holds them"
    );
  }
  assert!(
    gone > 0 && let_go > 0,
    "some window let go: {gone}, {let_go} bytes"
  );
  assert_eq!(cloned.0, 200);
  assert!(
    cloned
      .2
      .windows(b"CHANGEGROUP".len())
      .any(|window| window.eq_ignore_ascii_case(b"CHANGEGROUP")),
    "the clone carries a changegroup"
  );
}

/// Shape: the files of [`a_thin_push_whose_base_was_let_go_lands`]'s first commit, each about a content window long so
/// each lies in windows of its own.
const THIN_BASES: usize = 8;

/// A pack of `objects`, each whole but the deltas `deltas` names, each `(index, base, delta)` a REF_DELTA against
/// `base`, and where each object's entry begins.
fn pack_with_offsets(
  objects: &[(slates_git::object_name::Kind, &[u8])],
  deltas: &[(usize, ObjectId, Vec<u8>)],
) -> (Vec<u8>, Vec<u64>) {
  use slates_git::entry::{self, Header, Representation};
  let mut out = Vec::new();
  let count = u32::try_from(objects.len()).expect("a count");
  let mut writer = slates_git::pack::Writer::new(GitFormat::Sha1, count, &mut out);
  let mut at = entry::PACK_HEADER_LEN;
  let mut offsets = Vec::new();
  for (index, (kind, content)) in objects.iter().enumerate() {
    offsets.push(at);
    let (header, data) = match deltas.iter().find(|(delta, _, _)| *delta == index) {
      Some((_, base, delta)) => (
        Header {
          representation: Representation::RefDelta { base: *base },
          size: u64::try_from(delta.len()).expect("a size"),
        },
        stored_zlib(delta),
      ),
      None => (
        Header {
          representation: Representation::Whole(*kind),
          size: u64::try_from(content.len()).expect("a size"),
        },
        stored_zlib(content),
      ),
    };
    entry::write(header, at, &mut writer).expect("the entry's header");
    slates_git::out::Out::put(&mut writer, &data);
    at += entry::written_len(header, at).expect("the header's length")
      + u64::try_from(data.len()).expect("a length");
  }
  writer.finish().expect("the trailer");
  (out, offsets)
}

/// A-864's push readers (main 01a20182's ruling of 2026-11-22, reader (b)): a thin push's REF_DELTA base read from a
/// let-go window. Do: push a create of `main` at a commit of [`THIN_BASES`] files, each a window long; mark every window
/// of its pack landed and let go those no home holds on the repository's shard; push, as stock git sends a thin pack, a
/// commit over `main` changing one file whose window was let go, its new version a REF_DELTA against the old. Expect:
/// both pushes `200` with every ref `ok`, the second's base brought back from the node's store. Red before: the second
/// declined, its base unread ("an object it names cannot be read").
#[test]
fn a_thin_push_whose_base_was_let_go_lands() {
  use slates_git::object_name::Kind;
  let (identity, certificate) = identity();
  let daemon = daemon(identity);
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = name_on_the_repositorys_partition();
  create(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let edge = (port, &certificate);
  let window = crate::daemon::observe_at(&daemon, REPOSITORY, |state| {
    state
      .vcs_stores
      .content()
      .map(slates_vcs::ContentStore::window)
  })
  .unwrap_or(1 << 18);
  let mut state = 0x9e37_79b9_7f4a_7c15u64;
  let files: Vec<Vec<u8>> = (0..THIN_BASES)
    .map(|_| {
      (0..window)
        .map(|_| {
          state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
          state.to_be_bytes()[0]
        })
        .collect()
    })
    .collect();
  let tree_of = |files: &[Vec<u8>]| {
    let mut tree = Vec::new();
    for (at, file) in files.iter().enumerate() {
      tree.extend_from_slice(format!("100644 f{at}\0").as_bytes());
      tree.extend_from_slice(git_name(Kind::Blob, file).as_bytes());
    }
    tree
  };
  let tree = tree_of(&files);
  let first = format!(
    "tree {}\nauthor A <a@example.com> 1700000000 +0000\ncommitter A <a@example.com> 1700000000 +0000\n\nfirst\n",
    git_name(Kind::Tree, &tree)
  )
  .into_bytes();
  let first_id = git_name(Kind::Commit, &first);
  let mut objects: Vec<(Kind, &[u8])> = vec![(Kind::Commit, &first), (Kind::Tree, &tree)];
  objects.extend(files.iter().map(|file| (Kind::Blob, file.as_slice())));
  let (pack, offsets) = pack_with_offsets(&objects, &[]);
  let mut request = Vec::new();
  let line = format!("{ZERO} {first_id} refs/heads/main\0 report-status");
  pktline::write(Packet::Data(line.as_bytes()), &mut request).expect("the command");
  pktline::write(Packet::Flush, &mut request).expect("the flush");
  request.extend_from_slice(&pack);
  let created = pushed(edge, &name, &request);
  let ids: Vec<slates_vcs::ContentId> =
    pack.chunks(window).map(slates_vcs::ContentId::of).collect();
  let pack_identity = slates_vcs::ContentId::of(&pack);
  let gone: Vec<bool> = crate::daemon::observe_at(&daemon, REPOSITORY, move |state| {
    // The pack's home here, where it holds the pack for the region, records its windows and lets them go first, as a
    // landed pack's let-go tells it (A-724 piece 8c, goal 93 item 2b(b)): a window of an object the region holds is
    // pinned until then (`vcs_store::landed_pin`).
    crate::vcs_store::homes::landed_pack_here(state, pack_identity);
    crate::vcs_store::within_budget(state, 0, |stores| {
      let content = stores.content_mut().expect("the content store");
      for id in &ids {
        content.mark_landed(id);
      }
    });
    ids
      .iter()
      .map(|id| {
        matches!(
          crate::vcs_store::let_go_read(state, id),
          Ok(_) | Err(slates_vcs::LetGoRefused::Away)
        )
      })
      .collect()
  });
  // A file whose entry begins in a window let go: the thin push's base.
  let window_u64 = u64::try_from(window).expect("a window");
  let changed = (0..THIN_BASES).find(|file| {
    let at = usize::try_from(offsets[2 + file] / window_u64).expect("an index");
    gone.get(at).copied().unwrap_or(false)
  });
  let Some(changed) = changed else {
    daemon.stop();
    panic!("no file's window let go: {gone:?}");
  };
  let mut next = files.clone();
  next[changed].extend_from_slice(b"one line more\n");
  let next_tree = tree_of(&next);
  let second = format!(
    "tree {}\nparent {first_id}\nauthor A <a@example.com> 1700000001 +0000\ncommitter A <a@example.com> 1700000001 +0000\n\nsecond\n",
    git_name(Kind::Tree, &next_tree)
  )
  .into_bytes();
  let second_id = git_name(Kind::Commit, &second);
  let delta = slates_git::store::delta_of(&files[changed], &next[changed]).expect("a delta");
  let base = git_name(Kind::Blob, &files[changed]);
  let thin: Vec<(Kind, &[u8])> = vec![
    (Kind::Commit, &second),
    (Kind::Tree, &next_tree),
    (Kind::Blob, &next[changed]),
  ];
  let (thin_pack, _) = pack_with_offsets(&thin, &[(2, base, delta)]);
  let mut request = Vec::new();
  let line = format!("{first_id} {second_id} refs/heads/main\0 report-status");
  pktline::write(Packet::Data(line.as_bytes()), &mut request).expect("the command");
  pktline::write(Packet::Flush, &mut request).expect("the flush");
  request.extend_from_slice(&thin_pack);
  let updated = pushed(edge, &name, &request);
  let brought = counted_on(&daemon, REPOSITORY, "vcs_store.read_through.brought_back");
  eprintln!(
    "thin push: brought {brought}, refused {}, past patience {}",
    counted_on(&daemon, REPOSITORY, "vcs_store.git_resident.refused"),
    counted_on(
      &daemon,
      REPOSITORY,
      "vcs_store.git_resident.bring_back_past_patience"
    )
  );
  daemon.stop();
  for (what, answer) in [("the create", &created), ("the thin push", &updated)] {
    let report = String::from_utf8_lossy(&answer.1).into_owned();
    assert_eq!(
      (answer.0, report.contains("ok refs/heads/main")),
      (200, true),
      "{what}: {report}"
    );
  }
  assert!(brought > 0, "the thin push's base brought back");
}

/// A-864's push readers (main 01a20182's ruling of 2026-11-22, reader (c)): a verdict's read of an object the repository
/// holds in a let-go window. Do: push a line of [`THIN_BASES`] commits, each adding a file a content window long; mark
/// every window of the pack landed and let go those no home holds on the repository's shard; then create `refs/heads/other`
/// at a commit of the line whose entry lies in a window let go, as stock git does with an empty pack. Expect: both
/// pushes `200` with their refs `ok`, the commit's window brought back. The create's work is built before its verdict
/// (A-752), through the green build's read-through (reader (a), w-shared's), which brings the commit back, so the
/// verdict reads it held: this holds (c) covered by (a) and (b).
#[test]
fn a_create_at_a_commit_let_go_lands() {
  use slates_git::object_name::Kind;
  let (identity, certificate) = identity();
  let daemon = daemon(identity);
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = name_on_the_repositorys_partition();
  create(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let edge = (port, &certificate);
  let window = crate::daemon::observe_at(&daemon, REPOSITORY, |state| {
    state
      .vcs_stores
      .content()
      .map(slates_vcs::ContentStore::window)
  })
  .unwrap_or(1 << 18);
  let mut state = 0x6a09_e667_f3bc_c908u64;
  let mut owned: Vec<(Kind, Vec<u8>)> = Vec::new();
  let mut commits = Vec::new();
  let mut parent: Option<ObjectId> = None;
  for at in 0..THIN_BASES {
    let file: Vec<u8> = (0..window)
      .map(|_| {
        state = state
          .wrapping_mul(6_364_136_223_846_793_005)
          .wrapping_add(1_442_695_040_888_963_407);
        state.to_be_bytes()[0]
      })
      .collect();
    let mut tree = format!("100644 f{at}\0").into_bytes();
    tree.extend_from_slice(git_name(Kind::Blob, &file).as_bytes());
    let parent_line = parent
      .map(|parent| format!("parent {parent}\n"))
      .unwrap_or_default();
    let commit = format!(
      "tree {}\n{parent_line}author A <a@example.com> 1700000000 +0000\ncommitter A <a@example.com> 1700000000 +0000\n\nc{at}\n",
      git_name(Kind::Tree, &tree)
    )
    .into_bytes();
    let id = git_name(Kind::Commit, &commit);
    commits.push((owned.len(), id));
    owned.push((Kind::Commit, commit));
    owned.push((Kind::Tree, tree));
    owned.push((Kind::Blob, file));
    parent = Some(id);
  }
  let objects: Vec<(Kind, &[u8])> = owned
    .iter()
    .map(|(kind, bytes)| (*kind, bytes.as_slice()))
    .collect();
  let (pack, offsets) = pack_with_offsets(&objects, &[]);
  let head = parent.expect("a head");
  let mut request = Vec::new();
  let line = format!("{ZERO} {head} refs/heads/main\0 report-status");
  pktline::write(Packet::Data(line.as_bytes()), &mut request).expect("the command");
  pktline::write(Packet::Flush, &mut request).expect("the flush");
  request.extend_from_slice(&pack);
  let created = pushed(edge, &name, &request);
  let ids: Vec<slates_vcs::ContentId> =
    pack.chunks(window).map(slates_vcs::ContentId::of).collect();
  let pack_identity = slates_vcs::ContentId::of(&pack);
  let gone: Vec<bool> = crate::daemon::observe_at(&daemon, REPOSITORY, move |state| {
    // The repository goes idle first: the objects its reads keep lately, the line's commits among them, given back, so
    // the create's verdict reads its commit from its window.
    for id in state.vcs_stores.git_repositories() {
      crate::vcs_store::git_encode::give_back_idle(state, id);
    }
    // The pack's home here, where it holds the pack for the region, records its windows and lets them go first, as a
    // landed pack's let-go tells it (A-724 piece 8c, goal 93 item 2b(b)): a window of an object the region holds is
    // pinned until then (`vcs_store::landed_pin`).
    crate::vcs_store::homes::landed_pack_here(state, pack_identity);
    crate::vcs_store::within_budget(state, 0, |stores| {
      let content = stores.content_mut().expect("the content store");
      for id in &ids {
        content.mark_landed(id);
      }
    });
    ids
      .iter()
      .map(|id| {
        matches!(
          crate::vcs_store::let_go_read(state, id),
          Ok(_) | Err(slates_vcs::LetGoRefused::Away)
        )
      })
      .collect()
  });
  let window_u64 = u64::try_from(window).expect("a window");
  let target = commits.iter().find(|(index, _)| {
    let at = usize::try_from(offsets[*index] / window_u64).expect("an index");
    gone.get(at).copied().unwrap_or(false)
  });
  let Some((_, target)) = target.copied() else {
    daemon.stop();
    panic!("no commit's window let go: {gone:?}");
  };
  let (empty, _) = pack_with_offsets(&[], &[]);
  let mut request = Vec::new();
  let line = format!("{ZERO} {target} refs/heads/other\0 report-status");
  pktline::write(Packet::Data(line.as_bytes()), &mut request).expect("the command");
  pktline::write(Packet::Flush, &mut request).expect("the flush");
  request.extend_from_slice(&empty);
  let branched = pushed(edge, &name, &request);
  let brought = counted_on(&daemon, REPOSITORY, "vcs_store.read_through.brought_back");
  daemon.stop();
  for (what, answer, line) in [
    ("the line", &created, "ok refs/heads/main"),
    ("the create", &branched, "ok refs/heads/other"),
  ] {
    let report = String::from_utf8_lossy(&answer.1).into_owned();
    assert_eq!(
      (answer.0, report.contains(line)),
      (200, true),
      "{what}: {report}"
    );
  }
  assert!(brought > 0, "the commit's window brought back");
}

/// A repository name with no `.git` that routes to [`REPOSITORY`]'s partition, as a clone of `/project` names one.
fn bare_name_on_the_repositorys_partition() -> String {
  (0u32..)
    .map(|serial| format!("bare-{serial}"))
    .find(|name| usize::from(crate::verbs::owner_of_name(name, usize::from(SHARDS))) == REPOSITORY)
    .expect("a name routes there")
}

/// The path of an LFS answer's `href` for `action` ("upload", "verify" or "download"): what follows its authority.
fn href_of(body: &str, action: &str) -> Option<String> {
  let at = body.find(&format!("\"{action}\""))?;
  let rest = body.get(at..)?;
  let href = rest.find("\"href\":\"")? + "\"href\":\"".len();
  let rest = rest.get(href..)?;
  let url = rest.get(..rest.find('"')?)?;
  let path = url.get(url.find("//")? + 2..)?;
  Some(path.get(path.find('/')?..)?.to_owned())
}

/// main 01a20182's r95 (stock_git_verbs' lfs row, git-lfs 3.8.1's trace: the batch at `/project.git/info/lfs` answered
/// with upload and verify hrefs at `/project/info/lfs/objects/<oid>?refspec=refs/heads/master`, and every PUT to them
/// answered `404` in 0.3 ms, so the object was never kept and the clone's download found nothing). Do: create `bare-N`,
/// a git repository named with no `.git`; as its owner, ask the batch API at `/bare-N.git/info/lfs/objects/batch` for
/// an upload of a MiB object on `refs/heads/master`, as git-lfs asks a clone of `/bare-N`; PUT the object to the upload
/// href the batch handed back, POST its verify href, then batch a download and GET its href. Expect: the batch `200`
/// with an upload href; the PUT `200`; the verify `200`; the download's GET `200` with the object's bytes. Red before:
/// the PUT `404`, sent to Subversion's WebDAV site as every method git's smart HTTP never sends was; then the download
/// `413`, a MiB past the edge's request room.
#[test]
fn an_lfs_object_uploaded_to_the_href_its_batch_hands_out_is_kept_and_downloaded() {
  let (identity, certificate) = identity();
  let daemon = daemon(identity);
  let port = daemon.vcs_http_port().expect("the edge started");
  let name = bare_name_on_the_repositorys_partition();
  create(&daemon, &name);
  enroll(&daemon, OWNER_TOKEN, OWNER, None);
  let object: Vec<u8> = (0..1u32 << 20)
    .map(|at| u8::try_from(at % 251).unwrap())
    .collect();
  let oid: String = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, &object)
    .as_ref()
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect();
  let lfs_type = "application/vnd.git-lfs+json";
  let batch = |operation: &str| {
    let body = format!(
      "{{\"operation\":\"{operation}\",\"transfers\":[\"basic\"],\"ref\":{{\"name\":\"refs/heads/master\"}},\
       \"objects\":[{{\"oid\":\"{oid}\",\"size\":{}}}],\"hash_algo\":\"sha256\"}}",
      object.len()
    );
    let request = format!(
      "POST /{name}.git/info/lfs/objects/batch HTTP/1.1\r\nHost: localhost\r\n{}\r\nAccept: {lfs_type}\r\n\
       Content-Type: {lfs_type}\r\nContent-Length: {}\r\n\r\n{body}",
      owner_bearer(),
      body.len()
    );
    let (status, body) = exchange(port, &certificate, request.as_bytes());
    (status, String::from_utf8_lossy(&body).into_owned())
  };
  let send = |method: &str, path: &str, kind: &str, body: &[u8]| {
    let mut request = format!(
      "{method} {path} HTTP/1.1\r\nHost: localhost\r\n{}\r\nAccept: {lfs_type}\r\nContent-Type: {kind}\r\n\
       Content-Length: {}\r\n\r\n",
      owner_bearer(),
      body.len()
    )
    .into_bytes();
    request.extend_from_slice(body);
    exchange(port, &certificate, &request)
  };
  let uploading = batch("upload");
  let upload = href_of(&uploading.1, "upload");
  let verify = href_of(&uploading.1, "verify");
  let put = upload
    .as_deref()
    .map(|path| send("PUT", path, "application/octet-stream", &object).0);
  let verified = verify.as_deref().map(|path| {
    let body = format!("{{\"oid\":\"{oid}\",\"size\":{}}}", object.len());
    send("POST", path, lfs_type, body.as_bytes()).0
  });
  let downloading = batch("download");
  let got = href_of(&downloading.1, "download").map(|path| {
    let request = format!(
      "GET {path} HTTP/1.1\r\nHost: localhost\r\n{}\r\nAccept: application/octet-stream\r\n\r\n",
      owner_bearer()
    );
    exchange(port, &certificate, request.as_bytes())
  });
  daemon.stop();
  assert_eq!(uploading.0, 200, "{}", uploading.1);
  assert!(upload.is_some(), "an upload href: {}", uploading.1);
  assert_eq!(put, Some(200), "the PUT to {upload:?}");
  assert_eq!(verified, Some(200), "the verify at {verify:?}");
  assert_eq!(downloading.0, 200, "{}", downloading.1);
  let got = got.expect("a download href");
  assert_eq!((got.0, got.1 == object), (200, true), "the download");
}
