//! A-644's golden vectors: twelve pushes stock git 2.50.1 (Apple Git-155) made to stock `git receive-pack` serving a
//! protected `main` (`receive.denyNonFastForwards`, `receive.denyDeletes`, and an update hook keeping a linear history
//! of commits a trusted key signed), captured by `fixtures/protected/make-protected-fixtures.sh` (its header names the
//! commands), compiled in, never read at run time, and each file held to the SHA-256 the script recorded. Each push is
//! answered here from A-614's store, judged by the store's answers (`store::protect`) in git's words
//! (`receive::protected`), and every byte of every answer compared.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::convert::Infallible;

use aws_lc_rs::digest;
use slates_vcs::ContentStore;

use crate::Admission;
use crate::object_name::{ObjectFormat, ObjectId};
use crate::receive::{
  self, Advertised, Command, Declined, Judge, Outcome, Pushed, Received, Reported, RequestReader,
  protected, write_advertisement, write_report,
};
use crate::store::views::IndexedObjects;
use crate::store::{Change, CommitSigners, Entries, Incoming, Push, Repository, StoreRefused};
use crate::unpack::{self, NoIndex, Object, Outside, Reader};

/// One push the script captured.
pub(crate) struct Scenario {
  /// Its name in the script.
  pub(crate) name: &'static str,
  /// The object format of its server.
  pub(crate) format: ObjectFormat,
  /// What `git push` sent: its commands, then its pack.
  pub(crate) request: &'static [u8],
  /// What `git receive-pack` answered: its advertisement, then its report.
  pub(crate) response: &'static [u8],
  /// Every object the server held before the push, as one pack.
  pub(crate) before_pack: &'static [u8],
  /// The server's refs before the push, `id SP name` a line.
  pub(crate) before_refs: &'static str,
}

/// Includes a scenario's four files.
macro_rules! scenario {
  ($name:literal, $format:expr) => {
    Scenario {
      name: $name,
      format: $format,
      request: include_bytes!(concat!("../../fixtures/protected/", $name, ".request.bin")),
      response: include_bytes!(concat!("../../fixtures/protected/", $name, ".response.bin")),
      before_pack: include_bytes!(concat!("../../fixtures/protected/", $name, ".before.pack")),
      before_refs: include_str!(concat!("../../fixtures/protected/", $name, ".before.refs")),
    }
  };
}

/// Every scenario, in the order the script pushed them.
pub(crate) fn scenarios() -> [Scenario; 12] {
  [
    scenario!("create", ObjectFormat::Sha1),
    scenario!("signed", ObjectFormat::Sha1),
    scenario!("unsigned", ObjectFormat::Sha1),
    scenario!("untrusted", ObjectFormat::Sha1),
    scenario!("openpgp", ObjectFormat::Sha1),
    scenario!("x509", ObjectFormat::Sha1),
    scenario!("merge", ObjectFormat::Sha1),
    scenario!("feature", ObjectFormat::Sha1),
    scenario!("non-ff", ObjectFormat::Sha1),
    scenario!("delete", ObjectFormat::Sha1),
    scenario!("sha256-create", ObjectFormat::Sha256),
    scenario!("sha256-unsigned", ObjectFormat::Sha256),
  ]
}

/// The vectors' SHA-256 sums as the script wrote them, a line a file.
const SUMS: &str = include_str!("../../fixtures/protected/SHA256SUMS");

/// The trusted signer's public key, as `ssh-keygen` wrote it.
pub(crate) const TRUSTED_PUB: &str = include_str!("../../fixtures/protected/trusted.pub");

/// The other key's, which the server does not trust.
pub(crate) const OTHER_PUB: &str = include_str!("../../fixtures/protected/other.pub");

/// What git's verifier read of the `signed` scenario's commit: its signature, and the payload it covers.
pub(crate) const SIGNED_SIGNATURE: &[u8] =
  include_bytes!("../../fixtures/protected/signed.signature");
/// See [`SIGNED_SIGNATURE`].
pub(crate) const SIGNED_PAYLOAD: &[u8] = include_bytes!("../../fixtures/protected/signed.payload");

/// What git's verifier read of the `sha256-create` scenario's head, its signature in `gpgsig-sha256`.
pub(crate) const SHA256_SIGNATURE: &[u8] =
  include_bytes!("../../fixtures/protected/sha256-create.signature");
/// See [`SHA256_SIGNATURE`].
pub(crate) const SHA256_PAYLOAD: &[u8] =
  include_bytes!("../../fixtures/protected/sha256-create.payload");

/// Every file the script wrote but the scenarios', by its name in `SHA256SUMS`.
const OTHER_FILES: [(&str, &[u8]); 8] = [
  (
    "git-version.txt",
    include_bytes!("../../fixtures/protected/git-version.txt"),
  ),
  (
    "ssh-version.txt",
    include_bytes!("../../fixtures/protected/ssh-version.txt"),
  ),
  ("trusted.pub", TRUSTED_PUB.as_bytes()),
  ("other.pub", OTHER_PUB.as_bytes()),
  ("signed.signature", SIGNED_SIGNATURE),
  ("signed.payload", SIGNED_PAYLOAD),
  ("sha256-create.signature", SHA256_SIGNATURE),
  ("sha256-create.payload", SHA256_PAYLOAD),
];

/// Format: the alphabet of base64 (RFC 4648 §4), in which an SSH signature's armour and an OpenSSH public key are
/// written.
const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// The bytes base64 `text` writes, whitespace skipped and padding ending it; none for a character outside the
/// alphabet.
pub(crate) fn base64_decode(text: &[u8]) -> Option<Vec<u8>> {
  let mut bits: u32 = 0;
  let mut held = 0;
  let mut out = Vec::new();
  for byte in text.iter().filter(|byte| !byte.is_ascii_whitespace()) {
    if *byte == b'=' {
      break;
    }
    let value = BASE64.iter().position(|known| known == byte)?;
    bits = (bits << 6) | u32::try_from(value).ok()?;
    held += 6;
    if held >= 8 {
      held -= 8;
      out.push(u8::try_from((bits >> held) & 0xff).ok()?);
    }
  }
  Some(out)
}

/// `bytes` written in base64 with its padding.
pub(crate) fn base64_encode(bytes: &[u8]) -> Vec<u8> {
  let mut out = Vec::new();
  for chunk in bytes.chunks(3) {
    let mut word = [0u8; 3];
    word[..chunk.len()].copy_from_slice(chunk);
    let bits = (u32::from(word[0]) << 16) | (u32::from(word[1]) << 8) | u32::from(word[2]);
    for at in 0..4 {
      let value = (bits >> (18 - 6 * at)) & 0x3f;
      out.push(if at <= chunk.len() {
        BASE64[usize::try_from(value).unwrap()]
      } else {
        b'='
      });
    }
  }
  out
}

/// The key an OpenSSH public key line names, in SSH wire form: its second field, decoded.
pub(crate) fn key_blob(line: &str) -> Vec<u8> {
  let field = line.split(' ').nth(1).unwrap();
  base64_decode(field.as_bytes()).unwrap()
}

/// Why the tests' verifier refused a signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyRefused {
  /// The armour or the blob inside it is not an SSH signature's.
  Malformed,
  /// The key that made it is not one the verifier trusts.
  Untrusted,
}

/// The tests' verifier: an SSH signature is trusted when its armour holds an SSH signature blob (`SSHSIG`) made by one
/// of `keys`, as A-321's verifier finds its signer, without checking the signature itself (the server's tests verify
/// these very bytes); each question is kept.
pub(crate) struct Keys {
  /// The trusted keys, in SSH wire form.
  pub(crate) keys: Vec<Vec<u8>>,
  /// Each signature and payload asked about, in order.
  pub(crate) asked: RefCell<Vec<(Vec<u8>, Vec<u8>)>>,
}

impl Keys {
  /// A verifier trusting `keys`.
  pub(crate) fn trusting(keys: Vec<Vec<u8>>) -> Keys {
    Keys {
      keys,
      asked: RefCell::new(Vec::new()),
    }
  }
}

/// The key an SSH signature's armour names: the blob's magic, version and first string (`PROTOCOL.sshsig`).
fn signer_of(armour: &[u8]) -> Option<Vec<u8>> {
  let lines: Vec<&[u8]> = armour.split(|byte| *byte == b'\n').collect();
  let inner = lines.get(1..lines.len().checked_sub(2)?)?.concat();
  let blob = base64_decode(&inner)?;
  let rest = blob.strip_prefix(b"SSHSIG\0\0\0\x01")?;
  let (length, rest) = rest.split_at_checked(4)?;
  let length = usize::try_from(u32::from_be_bytes(length.try_into().ok()?)).ok()?;
  Some(rest.get(..length)?.to_vec())
}

impl CommitSigners for Keys {
  type Refused = KeyRefused;

  fn verify(&self, signature: &[u8], payload: &[u8]) -> Result<(), KeyRefused> {
    self
      .asked
      .borrow_mut()
      .push((signature.to_vec(), payload.to_vec()));
    let signer = signer_of(signature).ok_or(KeyRefused::Malformed)?;
    if self.keys.contains(&signer) {
      Ok(())
    } else {
      Err(KeyRefused::Untrusted)
    }
  }

  fn words(refused: &KeyRefused) -> &'static [u8] {
    match refused {
      KeyRefused::Malformed => b"its signature is malformed",
      KeyRefused::Untrusted => b"its signer is not trusted",
    }
  }
}

/// A pack's objects handed to a push in pack order, each named as the reader named it.
struct Source<'o> {
  objects: &'o [Object],
  at: usize,
}

impl Entries for Source<'_> {
  type Refused = Infallible;

  fn next_entry(&mut self) -> Option<Result<Incoming<'_>, Infallible>> {
    let object = self.objects.get(self.at)?;
    self.at += 1;
    Some(Ok(Incoming {
      kind: object.kind,
      claimed: Some(object.id),
      content: &object.content,
      depth: object.chain.map_or(0, |chain| chain.depth),
    }))
  }
}

/// A scenario's server as A-614's store holds it: its objects, each ref a green, and the objects themselves, which a
/// thin pack's deltas take their bases from.
pub(crate) struct Server {
  pub(crate) repository: Repository,
  pub(crate) store: ContentStore,
  pub(crate) objects: Vec<Object>,
  /// Each ref's green, by name; a ref the server never held gets the next.
  pub(crate) greens: BTreeMap<Vec<u8>, u64>,
}

/// The refs of a `id SP name` listing.
fn refs_of(format: ObjectFormat, lines: &str) -> Vec<(ObjectId, Vec<u8>)> {
  lines
    .lines()
    .map(|line| {
      let (hex, name) = line.split_once(' ').unwrap();
      let id = ObjectId::from_hex(format, hex.as_bytes()).unwrap();
      (id, name.as_bytes().to_vec())
    })
    .collect()
}

/// The objects `objects` handed to a sealed push in `repository`'s format.
pub(crate) fn sealed(
  repository: &Repository,
  store: &mut ContentStore,
  objects: &[Object],
) -> Push {
  let mut push = Push::new(repository.format());
  push
    .admit_all(store, &repository.naming(), &mut Source { objects, at: 0 })
    .unwrap();
  repository.seal(store, &mut push).unwrap();
  push
}

/// `scenario`'s server before its push, every ref one version of its own green.
pub(crate) fn server(scenario: &Scenario) -> Server {
  let format = scenario.format;
  let unpacked = unpack::read_whole(
    scenario.before_pack,
    format,
    &NoIndex,
    &mut Admission::new(u64::MAX),
  )
  .unwrap();
  let mut store = ContentStore::new(1 << 26, 1 << 10).unwrap();
  let mut repository = Repository::new(format);
  let refs = refs_of(format, scenario.before_refs);
  let greens: BTreeMap<Vec<u8>, u64> = refs
    .iter()
    .zip(1..)
    .map(|((_, name), green)| (name.clone(), green))
    .collect();
  let mut push = sealed(&repository, &mut store, &unpacked.objects);
  let moves: Vec<_> = refs
    .iter()
    .map(|(id, name)| {
      repository
        .admit(&store, &push, greens[name], Some(*id))
        .unwrap()
    })
    .collect();
  if moves.is_empty() {
    push.abandon(&mut store).unwrap();
  } else {
    repository.commit(&mut store, &mut push, &moves).unwrap();
  }
  Server {
    repository,
    store,
    objects: unpacked.objects,
    greens,
  }
}

/// The server's objects, as the bases a thin pack's deltas name (`index-pack --fix-thin`).
struct Bases<'o>(&'o [Object]);

impl Outside for Bases<'_> {
  fn object(&self, id: &ObjectId) -> Option<(crate::object_name::Kind, &[u8])> {
    let object = self.0.iter().find(|object| object.id == *id)?;
    Some((object.kind, &object.content))
  }
}

/// What the protected server advertised: git's defaults (no push options), in its format.
pub(crate) fn advertised(format: ObjectFormat) -> Advertised<'static> {
  Advertised {
    format,
    atomic: true,
    ofs_delta: true,
    push_options: false,
    session_id: None,
    agent: b"git/2.50.1-Darwin",
  }
}

/// The server's protection of `refs/heads/main`, judged from the store's answers in git's words; a store's refusal is
/// kept, never unwrapped, and the command refused.
struct ProtectedMain<'s> {
  server: &'s Server,
  push: &'s Push,
  keys: Keys,
  /// Each store refusal met, in order.
  faults: Vec<StoreRefused>,
}

/// Format: the one ref the captured server protects.
const MAIN: &[u8] = b"refs/heads/main";

impl ProtectedMain<'_> {
  /// The green `name` names: its own, or a fresh one for a ref the server never held.
  fn green(&self, name: &[u8]) -> u64 {
    let next = u64::try_from(self.server.greens.len()).unwrap_or(u64::MAX);
    self
      .server
      .greens
      .get(name)
      .copied()
      .unwrap_or(next.saturating_add(1))
  }

  /// The protection's verdict on `command`, or the store's refusal.
  fn verdict(&self, command: &Command) -> Result<Option<Declined>, StoreRefused> {
    let (server, push, name) = (self.server, self.push, command.name.as_slice());
    let green = self.green(name);
    let to = (!command.is_delete()).then_some(command.new);
    match server.repository.change(&server.store, push, green, to)? {
      Change::Delete => return Ok(Some(protected::deletion_prohibited(name))),
      Change::Rewrite => return Ok(Some(protected::non_fast_forward(name))),
      Change::Create | Change::FastForward => {}
    }
    let repository = &server.repository;
    if let Some(merge) = repository.merge_commit(&server.store, push, green, command.new)? {
      return Ok(Some(protected::merge_commit(name, &merge)));
    }
    let unsigned =
      repository.unsigned_commit(&server.store, push, green, command.new, &self.keys)?;
    Ok(unsigned.map(|(commit, why)| protected::unsigned_commit::<Keys>(name, &commit, &why)))
  }
}

impl Judge for ProtectedMain<'_> {
  fn judge(&mut self, command: &Command, _pushed: &Pushed<'_>) -> Result<Vec<Reported>, Declined> {
    if command.name != MAIN {
      return Ok(Vec::new());
    }
    match self.verdict(command) {
      Ok(None) => Ok(Vec::new()),
      Ok(Some(declined)) => Err(declined),
      Err(refused) => {
        self.faults.push(refused);
        Err(protected::hook_declined(&command.name, Vec::new()))
      }
    }
  }
}

/// `scenario`'s push read, its pack (if any) read with the server's objects as bases.
fn read_push(scenario: &Scenario, server: &Server) -> (receive::Request, Option<unpack::Unpacked>) {
  let advertised = advertised(scenario.format);
  let mut admission = Admission::new(u64::MAX);
  let mut reader = RequestReader::new(&advertised);
  let took = reader.read(scenario.request, &mut admission).unwrap();
  let request = reader.request().unwrap();
  let unpacked = request.wants_pack().then(|| {
    let mut reader = Reader::new(scenario.format, &NoIndex);
    let pack = &scenario.request[took..];
    assert_eq!(reader.read(pack, &mut admission).unwrap(), pack.len());
    reader
      .finish_with(&Bases(&server.objects), &mut admission)
      .unwrap()
  });
  (request, unpacked)
}

/// `scenario`'s push answered from its server's store, judged by the protection; with the store refusals met.
pub(crate) fn answered(scenario: &Scenario) -> (Received, Vec<StoreRefused>) {
  let mut server = server(scenario);
  let (request, unpacked) = read_push(scenario, &server);
  let objects = unpacked
    .as_ref()
    .map_or(&[][..], |unpacked| &unpacked.objects);
  let push = sealed(&server.repository, &mut server.store, objects);
  let keys = Keys::trusting(vec![key_blob(TRUSTED_PUB)]);
  let mut judge = ProtectedMain {
    server: &server,
    push: &push,
    keys,
    faults: Vec::new(),
  };
  let view = IndexedObjects::new(&server.repository, &server.store, u64::MAX);
  let unpacked: Option<Result<unpack::Unpacked, unpack::Refusal>> = unpacked.map(Ok);
  let mut admission = Admission::new(u64::MAX);
  let outcomes = receive::outcomes(
    &request,
    unpacked.as_ref(),
    &view,
    &mut judge,
    &mut admission,
  )
  .unwrap();
  let faults = judge.faults;
  let received = Received {
    request,
    unpacked,
    outcomes,
  };
  (received, faults)
}

/// The advertisement written for `scenario`'s server.
fn advertisement(scenario: &Scenario) -> Vec<u8> {
  let refs = refs_of(scenario.format, scenario.before_refs);
  let named: Vec<(ObjectId, &[u8])> = refs
    .iter()
    .map(|(id, name)| (*id, name.as_slice()))
    .collect();
  let mut out = Vec::new();
  write_advertisement(&named, &advertised(scenario.format), &mut out).unwrap();
  out
}

/// A-644, golden vectors: do hash every file the script wrote with SHA-256 (aws-lc-rs) and compare each with the sum
/// the script recorded. Expect: every sum equal and every file listed, so the bytes the tests read are the ones stock
/// git 2.50.1 and OpenSSH 10.2p1 wrote.
#[test]
fn every_protected_vector_is_the_file_the_script_recorded() {
  let mut files: Vec<(String, &[u8])> = OTHER_FILES
    .iter()
    .map(|(name, bytes)| ((*name).to_owned(), *bytes))
    .collect();
  for scenario in scenarios() {
    let name = scenario.name;
    files.push((format!("{name}.request.bin"), scenario.request));
    files.push((format!("{name}.response.bin"), scenario.response));
    files.push((format!("{name}.before.pack"), scenario.before_pack));
    files.push((
      format!("{name}.before.refs"),
      scenario.before_refs.as_bytes(),
    ));
  }
  let sums: BTreeMap<&str, &str> = SUMS
    .lines()
    .map(|line| {
      let (sum, name) = line.split_once("  ").unwrap();
      (name, sum)
    })
    .collect();
  assert_eq!(
    sums.len(),
    files.len(),
    "every file the script summed is read"
  );
  for (name, bytes) in &files {
    let sum: String = digest::digest(&digest::SHA256, bytes)
      .as_ref()
      .iter()
      .map(|byte| format!("{byte:02x}"))
      .collect();
    assert_eq!(sums.get(name.as_str()), Some(&sum.as_str()), "{name}");
  }
}

/// A-644 (§4.17.2 "Protected branches, in git's terms"; golden vectors: twelve pushes stock git 2.50.1 made to a stock
/// server protecting `main`). Do: answer each push from A-614's store holding the server's objects and refs, judging
/// `main`'s commands by the store's answers (the change; the first merge commit brought; the first commit brought no
/// trusted key signed) in `receive::protected`'s words, and write the answer after the advertisement. Expect: every
/// byte stock git wrote: `ok` for the protected create of a signed history, the signed fast-forward, the merge pushed
/// to an unprotected branch and the SHA-256 create; git's own `non-fast-forward` and `deletion prohibited` with their
/// band-2 lines; and `hook declined` for the unsigned, untrusted, OpenPGP, X.509 and merge commits and the SHA-256
/// unsigned one, each after its words in git's 128-byte pieces and git's `hook declined to update` line; and no store
/// refusal met.
#[test]
fn every_protected_push_stock_git_made_is_answered_with_the_bytes_git_answered() {
  for scenario in scenarios() {
    let advertised = advertisement(&scenario);
    let (head, tail) = scenario.response.split_at(advertised.len());
    assert_eq!(advertised, head, "{}: the advertisement", scenario.name);
    let (received, faults) = answered(&scenario);
    assert_eq!(faults, Vec::new(), "{}: the store refused", scenario.name);
    let mut answer = Vec::new();
    write_report(&received, &mut answer).unwrap();
    assert_eq!(answer, tail, "{}: the answer", scenario.name);
  }
}

/// The outcome the protection gave each scenario's one command: its reason, or none where it applied.
fn reasons() -> Vec<(&'static str, Option<Vec<u8>>)> {
  scenarios()
    .iter()
    .map(|scenario| {
      let (received, _) = answered(scenario);
      let reason = match received.outcomes.first() {
        Some(Outcome::Ng(declined)) => Some(declined.reason.clone()),
        _ => None,
      };
      (scenario.name, reason)
    })
    .collect()
}

/// A-644 (golden vectors, read). Do: answer each push and read its one command's outcome. Expect: what each scenario's
/// name says: applied for create, signed, feature and sha256-create; `hook declined` for the six the hook refused;
/// git's own reasons for the non-fast-forward and the delete.
#[test]
fn each_protected_push_is_refused_for_what_its_scenario_names() {
  let hook: Option<Vec<u8>> = Some(protected::HOOK_DECLINED.to_vec());
  let expected = vec![
    ("create", None),
    ("signed", None),
    ("unsigned", hook.clone()),
    ("untrusted", hook.clone()),
    ("openpgp", hook.clone()),
    ("x509", hook.clone()),
    ("merge", hook.clone()),
    ("feature", None),
    ("non-ff", Some(protected::NON_FAST_FORWARD.to_vec())),
    ("delete", Some(protected::DELETION_PROHIBITED.to_vec())),
    ("sha256-create", None),
    ("sha256-unsigned", hook),
  ];
  assert_eq!(reasons(), expected);
}

/// The signatures and payloads the verifier was asked about in `scenario`'s push to `main`.
fn asked(scenario: &Scenario) -> Vec<(Vec<u8>, Vec<u8>)> {
  let mut server = server(scenario);
  let (request, unpacked) = read_push(scenario, &server);
  let objects = unpacked
    .as_ref()
    .map_or(&[][..], |unpacked| &unpacked.objects);
  let push = sealed(&server.repository, &mut server.store, objects);
  let keys = Keys::trusting(vec![key_blob(TRUSTED_PUB)]);
  let command = &request.commands[0];
  let green = server.greens.get(MAIN).copied().unwrap_or(1);
  let found = server
    .repository
    .unsigned_commit(&server.store, &push, green, command.new, &keys)
    .unwrap();
  assert_eq!(found, None, "{}: every commit trusted", scenario.name);
  keys.asked.into_inner()
}

/// A-644 (golden vectors: what git's verifier reads, split by the script as commit.c's `parse_buffer_signed_by_header`
/// splits it and checked there by `ssh-keygen -Y verify`). Do: judge the signed fast-forward of a SHA-1 `main`, and the
/// SHA-256 create of a two-commit signed history, recording what the verifier is handed. Expect: the SHA-1 push's one
/// new commit handed with the very signature and payload git verified; the SHA-256 create handing both commits, the
/// head first, the head's pair the one git verified from `gpgsig-sha256`.
#[test]
fn the_verifier_is_handed_the_signature_and_payload_git_verified() {
  let [_, signed, ..] = scenarios();
  let asked_signed = asked(&signed);
  assert_eq!(
    asked_signed,
    vec![(SIGNED_SIGNATURE.to_vec(), SIGNED_PAYLOAD.to_vec())]
  );
  let sha256 = scenarios().into_iter().nth(10).unwrap();
  let asked_sha256 = asked(&sha256);
  assert_eq!(asked_sha256.len(), 2, "a create brings its whole history");
  assert_eq!(
    asked_sha256[0],
    (SHA256_SIGNATURE.to_vec(), SHA256_PAYLOAD.to_vec())
  );
}

/// A-644 (the tests' verifier). Do: find the signer of the golden signature and decode both public keys. Expect: the
/// signature names the trusted key and not the other, so a test trusting a key trusts what it signed.
#[test]
fn the_golden_signature_names_the_trusted_key() {
  let trusted = key_blob(TRUSTED_PUB);
  assert_eq!(signer_of(SIGNED_SIGNATURE), Some(trusted.clone()));
  assert_ne!(key_blob(OTHER_PUB), trusted);
  assert_eq!(
    base64_decode(&base64_encode(SIGNED_PAYLOAD)),
    Some(SIGNED_PAYLOAD.to_vec())
  );
}
