//! A-644 part 2: a protected git branch through the policy's check point, by use. A-321's verifier reads the commit
//! signatures stock git made (`crates/git/fixtures/protected`, split and checked there by `ssh-keygen -Y verify`) under
//! git's namespace alone; and twelve pushes stock git 2.50.1 made to a stock server protecting `main` are answered from
//! A-614's store, judged by a policy of A-622's rules and one trusted key through `vcs_policy::admit`, with every byte
//! git wrote.

use std::collections::BTreeMap;
use std::convert::Infallible;

use slates_db::catalog::{
  ObjectFormat as RecordFormat, PolicyEffect, PolicyNames, PolicyRight, PolicyRule, PolicyWho,
  Principal, RepositoryId, RepositoryPolicy, RepositoryRecord, VcsTool,
};
use slates_git::Admission;
use slates_git::object_name::{Kind, ObjectFormat, ObjectId};
use slates_git::receive::{
  self, Advertised, Command, Declined, Judge, NoneHidden, Pushed, Received, Reported,
  RequestReader, protected, write_advertisement, write_report,
};
use slates_git::store::views::IndexedObjects;
use slates_git::store::{Entries, Incoming, Push, Repository, StoreRefused};
use slates_git::unpack::{self, NoIndex, Object, Outside, Reader};
use slates_vcs::ContentStore;

use super::{StoreBrings, declined, right_of};
use crate::signed_work::{
  GIT_NAMESPACE, SignatureRefusal, TrustedKey, dearmour, verify, verify_in,
};
use crate::vcs_policy::{Asked, Brings, Name, admit};

/// One push the protected fixtures captured.
struct Scenario {
  name: &'static str,
  format: ObjectFormat,
  request: &'static [u8],
  response: &'static [u8],
  before_pack: &'static [u8],
  before_refs: &'static str,
}

/// Includes a scenario's four files from slates-git's protected fixtures.
macro_rules! scenario {
  ($name:literal, $format:expr) => {
    Scenario {
      name: $name,
      format: $format,
      request: include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../git/fixtures/protected/",
        $name,
        ".request.bin"
      )),
      response: include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../git/fixtures/protected/",
        $name,
        ".response.bin"
      )),
      before_pack: include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../git/fixtures/protected/",
        $name,
        ".before.pack"
      )),
      before_refs: include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../git/fixtures/protected/",
        $name,
        ".before.refs"
      )),
    }
  };
}

/// Every scenario, in the order the script pushed them.
fn scenarios() -> [Scenario; 12] {
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

/// The trusted signer's public key, and the other's, as `ssh-keygen` wrote them.
const TRUSTED_PUB: &str = include_str!(concat!(
  env!("CARGO_MANIFEST_DIR"),
  "/../git/fixtures/protected/trusted.pub"
));
/// See [`TRUSTED_PUB`].
const OTHER_PUB: &str = include_str!(concat!(
  env!("CARGO_MANIFEST_DIR"),
  "/../git/fixtures/protected/other.pub"
));

/// What git's verifier read of a SHA-1 commit the trusted key signed, and of a SHA-256 one: each signature and payload.
const SIGNED: [(&[u8], &[u8]); 2] = [
  (
    include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/../git/fixtures/protected/signed.signature"
    )),
    include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/../git/fixtures/protected/signed.payload"
    )),
  ),
  (
    include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/../git/fixtures/protected/sha256-create.signature"
    )),
    include_bytes!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/../git/fixtures/protected/sha256-create.payload"
    )),
  ),
];

/// A key from its OpenSSH public line.
fn key(line: &str) -> TrustedKey {
  TrustedKey::from_openssh(line.trim()).unwrap()
}

/// A-644 (A-321's verifier under git's namespace). Do: verify each signature stock git made with the trusted key over
/// the payload git's verifier read, in a SHA-1 and a SHA-256 repository: under git's namespace with the trusted key,
/// as signed work's namespace, under the other key alone, and over a payload one byte changed. Expect: the trusted key
/// named; `Namespace` as signed work, since git signs under `git`; `UnregisteredKey`; `BadSignature`.
#[test]
fn a_commit_git_signed_verifies_under_gits_namespace_alone() {
  let (trusted, other) = ([key(TRUSTED_PUB)], [key(OTHER_PUB)]);
  for (armour, payload) in SIGNED {
    let blob = dearmour(std::str::from_utf8(armour).unwrap()).unwrap();
    let signer = verify_in(GIT_NAMESPACE, &blob, payload, &trusted).map(TrustedKey::wire);
    assert_eq!(signer, Ok(trusted[0].wire()));
    let mut changed = payload.to_vec();
    changed[0] ^= 1;
    let refusals = [
      verify(&blob, payload, &trusted).err(),
      verify_in(GIT_NAMESPACE, &blob, payload, &other).err(),
      verify_in(GIT_NAMESPACE, &blob, &changed, &trusted).err(),
    ];
    let expected = [
      SignatureRefusal::Namespace,
      SignatureRefusal::UnregisteredKey,
      SignatureRefusal::BadSignature,
    ];
    assert_eq!(refusals, expected.map(Some));
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

/// `objects` handed to a sealed push in `repository`'s format.
fn sealed(repository: &Repository, store: &mut ContentStore, objects: &[Object]) -> Push {
  let mut push = Push::new(repository.format());
  let mut source = Source { objects, at: 0 };
  push
    .admit_all(store, &repository.naming(), &mut source)
    .unwrap();
  repository.seal(store, &mut push).unwrap();
  push
}

/// A scenario's server in A-614's store: its objects, and each ref its own green.
struct Server {
  repository: Repository,
  store: ContentStore,
  objects: Vec<Object>,
  greens: BTreeMap<Vec<u8>, u64>,
}

/// The refs of an `id SP name` listing.
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

/// `scenario`'s server before its push, every ref one version of its own green.
fn server(scenario: &Scenario) -> Server {
  let mut admission = Admission::new(u64::MAX);
  let unpacked = unpack::read_whole(
    scenario.before_pack,
    scenario.format,
    &NoIndex,
    &mut admission,
  )
  .unwrap();
  let mut store = ContentStore::new(1 << 26, 1 << 10).unwrap();
  let mut repository = Repository::new(scenario.format);
  let refs = refs_of(scenario.format, scenario.before_refs);
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

/// The server's objects, as the bases a thin pack's deltas name.
struct Bases<'o>(&'o [Object]);

impl Outside for Bases<'_> {
  fn object(&self, id: &ObjectId) -> Option<(Kind, &[u8])> {
    let object = self.0.iter().find(|object| object.id == *id)?;
    Some((object.kind, &object.content))
  }
}

/// What the protected server advertised: git's defaults, in its format.
fn advertised(format: ObjectFormat) -> Advertised<'static> {
  Advertised {
    format,
    atomic: true,
    ofs_delta: true,
    push_options: false,
    session_id: None,
    agent: b"git/2.50.1-Darwin",
  }
}

/// Shape: the repository's owner, who pushed every scenario.
const OWNER: u32 = 1000;

/// The owner, as the catalog names a principal.
fn owner() -> Principal {
  Principal::Uid {
    host: slates_db::HostId(0),
    uid: OWNER,
  }
}

/// The record of a git repository protecting `main` as the captured server did: every principal denied its rewrite,
/// its delete, a merge commit and an unsigned commit, the trusted key its one signer.
fn protecting() -> RepositoryRecord {
  let deny = |right| PolicyRule {
    names: PolicyNames::Refs,
    pattern: "refs/heads/main".to_owned(),
    right,
    effect: PolicyEffect::Deny,
    who: PolicyWho::Anyone,
  };
  let rules = [
    PolicyRight::Rewrite,
    PolicyRight::Delete,
    PolicyRight::MergeCommit,
    PolicyRight::UnsignedCommit,
  ];
  RepositoryRecord {
    id: RepositoryId { bytes: [0; 16] },
    name: "protected".to_owned(),
    tool: VcsTool::Git,
    format: RecordFormat::Sha1,
    default_branch: Some("main".to_owned()),
    owner: owner(),
    policy: RepositoryPolicy {
      version: 1,
      rules: rules.into_iter().map(deny).collect(),
      hooks: Vec::new(),
      hooks_env: Vec::new(),
      signers: vec![key(TRUSTED_PUB).wire().to_vec()],
    },
  }
}

/// receive-pack's judge as part 2 composes it: each command's change read from the store, its right and what it
/// brings asked of the check point, a refusal answered in git's words; a store's refusal kept, never unwrapped.
struct Protected<'s> {
  server: &'s Server,
  push: &'s Push,
  record: &'s RepositoryRecord,
  faults: Vec<StoreRefused>,
}

impl Protected<'_> {
  /// The green `name` names: its own, or the next for a ref the server never held.
  fn green(&self, name: &[u8]) -> u64 {
    let next = u64::try_from(self.server.greens.len()).unwrap_or(u64::MAX);
    let green = self.server.greens.get(name).copied();
    green.unwrap_or(next.saturating_add(1))
  }

  /// The check point's verdict on `command`, in git's words; or the store's refusal.
  fn verdict(&self, command: &Command) -> Result<Result<(), Declined>, StoreRefused> {
    let (server, name) = (self.server, command.name.as_slice());
    let green = self.green(name);
    let to = (!command.is_delete()).then_some(command.new);
    let change = server
      .repository
      .change(&server.store, self.push, green, to)?;
    let right = right_of(change);
    let brings = to.map(|to| StoreBrings {
      repository: &server.repository,
      store: &server.store,
      push: self.push,
      green,
      to,
    });
    let asked = Asked {
      name: Name::Ref(name),
      right,
      brings: brings.as_ref().map(|brings| brings as &dyn Brings),
    };
    let verdict = admit(self.record, &owner(), &[asked]);
    Ok(verdict.map_err(|refusal| declined(&refusal, name, right)))
  }
}

impl Judge for Protected<'_> {
  type Accepted = ();

  fn judge(&mut self, command: &Command, _pushed: &Pushed<'_>) -> Result<(), Declined> {
    match self.verdict(command) {
      Ok(verdict) => verdict,
      Err(refused) => {
        self.faults.push(refused);
        Err(protected::hook_declined(&command.name, Vec::new()))
      }
    }
  }

  fn append(&mut self, _command: &Command, (): ()) -> Result<Vec<Reported>, Declined> {
    Ok(Vec::new())
  }

  fn append_atomic(
    &mut self,
    accepted: Vec<(&Command, ())>,
  ) -> Result<Vec<Vec<Reported>>, Declined> {
    Ok(accepted.iter().map(|_| Vec::new()).collect())
  }
}

/// `scenario`'s push answered from its server's store, checked against the index, its objects admitted to a sealed
/// push, and decided by the protection; with the store refusals met.
fn answered(scenario: &Scenario, record: &RepositoryRecord) -> (Received, Vec<StoreRefused>) {
  let mut server = server(scenario);
  let advertised = advertised(scenario.format);
  let mut admission = Admission::new(u64::MAX);
  let mut reader = RequestReader::new(&advertised);
  let took = reader.read(scenario.request, &mut admission).unwrap();
  let request = reader.request().unwrap();
  let unpacked = request.wants_pack().then(|| {
    let mut pack = Reader::new(scenario.format, &NoIndex);
    pack
      .read(&scenario.request[took..], &mut admission)
      .unwrap();
    let bases = Bases(&server.objects);
    Ok(pack.finish_with(&bases, &mut admission).unwrap())
  });
  let checked = {
    let view = IndexedObjects::new(&server.repository, &server.store, u64::MAX);
    receive::check(request, unpacked, &view, &NoneHidden, &mut admission).unwrap()
  };
  let push = sealed(&server.repository, &mut server.store, checked.objects());
  let mut judge = Protected {
    server: &server,
    push: &push,
    record,
    faults: Vec::new(),
  };
  let received = receive::decide(checked, &mut judge, &mut admission).unwrap();
  (received, judge.faults)
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

/// A-644 (§4.17.2 "Protected branches, in git's terms", end to end; golden vectors: twelve pushes stock git 2.50.1
/// made to a stock server protecting `main`). Do: answer each push from A-614's store holding the captured server's
/// objects and refs, every command judged through `vcs_policy::admit` by a git repository's record denying everyone
/// `main`'s rewrite, delete, merge commits and unsigned commits and trusting the fixtures' signing key, its signatures
/// verified by A-321's code under git's namespace; write the answer after the advertisement. Expect: every byte stock
/// git wrote: the signed create and fast-forward, the merge on an unprotected branch and the SHA-256 create applied;
/// git's own `non-fast-forward` and `deletion prohibited`; `hook declined` after the words naming the unsigned commit,
/// another key's, OpenPGP's, X.509's, the merge commit and the SHA-256 unsigned commit; and no store refusal met.
#[test]
fn every_protected_push_is_answered_through_the_policy_with_the_bytes_git_answered() {
  let record = protecting();
  for scenario in scenarios() {
    let advertised = advertisement(&scenario);
    let (head, tail) = scenario.response.split_at(advertised.len());
    assert_eq!(advertised, head, "{}: the advertisement", scenario.name);
    let (received, faults) = answered(&scenario, &record);
    assert_eq!(faults, Vec::new(), "{}: the store refused", scenario.name);
    let mut answer = Vec::new();
    write_report(&received, &mut answer).unwrap();
    assert_eq!(answer, tail, "{}: the answer", scenario.name);
  }
}

/// A-644 (the requirements bind only where a rule imposes them). Do: answer the merge and the unsigned pushes with a
/// record that trusts the key but denies nothing beyond `main`'s rewrite and delete. Expect: both applied, as stock git
/// with no update hook would apply them: a requirement no rule names asks nothing of a push.
#[test]
fn a_push_no_requirement_names_is_applied() {
  let mut record = protecting();
  record.policy.rules.truncate(2);
  let [_, _, unsigned, _, _, _, merge, ..] = scenarios();
  for scenario in [unsigned, merge] {
    let (received, faults) = answered(&scenario, &record);
    assert_eq!(faults, Vec::new());
    let applied = received
      .outcomes
      .iter()
      .all(|outcome| matches!(outcome, receive::Outcome::Ok(_)));
    assert!(applied, "{}: {:?}", scenario.name, received.outcomes);
  }
}
