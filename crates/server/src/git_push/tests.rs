//! A-654 part 2's tests (§4.17.2 "A push"; the driver's module docs): stock git 2.50.1's pushes, captured by
//! `crates/git/fixtures/push/make-push-fixtures.sh`, driven through the driver on the owner shard in the order the
//! script pushed them, the advertisement and the report held to git's bytes and each ref's green to the tree its commit
//! names; the repository's policy hiding a ref and refusing changes in git's words; a configured hook refusing every
//! command, typed; and a push with `atomic` all or none, and without it a ref at a time.

use slates_db::HostId;
use slates_db::catalog::{Principal, RepositoryId};
use slates_git::object_name::{ObjectFormat, ObjectId};
use slates_git::pktline::{self, Packet};
use slates_git::receive::{
  ATOMIC_FAILURE, ATOMIC_TRANSACTION_FAILED, Advertised, Declined, FAILED_TO_UPDATE, HIDDEN_DELETE,
  Outcome, Received, protected, write_advertisement,
};
use slates_ipc::protocol::{
  HookConfig, HookPointName, ObjectFormat as WireFormat, PolicyEffect, PolicyNames, PolicyRight,
  PolicyRule, PolicyWho, ReplyBody, RequestBody, VcsTool,
};

use super::{AGENT, HOOK_NOT_RUN, ReceivePack};
use crate::state::ShardState;
use crate::verbs::dispatch;

/// Shape: the room each test push is admitted: far past the largest fixture's bytes and what reading it builds.
const ROOM: u64 = 1 << 24;

/// Shape: the bytes an edge hands the driver at a time: fewer than any request holds and no multiple of a packet's
/// length, so every push arrives over several calls and some packet across two.
const PIECE: usize = 97;

/// Format: the capabilities stock git 2.50.1 sends on a push's first command (the fixtures' requests).
const CAPABILITIES: &[u8] =
  b" report-status-v2 side-band-64k quiet object-format=sha1 agent=git/2.50.1-Darwin";

/// Format: the same with `atomic`, where stock git puts it (the atomic fixture's request).
const ATOMIC: &[u8] =
  b" report-status-v2 side-band-64k quiet atomic object-format=sha1 agent=git/2.50.1-Darwin";

/// Format: what the captured server advertised (`receive.advertisePushOptions`, stock git's agent): what each of its
/// answers begins with.
const CAPTURED: Advertised<'static> = Advertised {
  format: ObjectFormat::Sha1,
  atomic: true,
  ofs_delta: true,
  push_options: true,
  session_id: None,
  agent: b"git/2.50.1-Darwin",
};

/// Format: what this server advertises (the driver's module docs): no push options, its own agent.
const OURS: Advertised<'static> = Advertised {
  format: ObjectFormat::Sha1,
  atomic: true,
  ofs_delta: true,
  push_options: false,
  session_id: None,
  agent: AGENT,
};

/// Format: the commit the fixtures' `create` made `main`, the first of their history.
const FIRST: &str = "b1fac94f2ed5e3cb5850e4b5f89d23fce635601e";

/// Format: the commit `update` moved `main` to, which `copy` and the tag name.
const SECOND: &str = "57f7449d19792b7c799cad411f2de98e9fcc3483";

/// Format: the annotated tag `tag` pushed as `refs/tags/v1`.
const TAG: &str = "acb17d37d8aba173348f74c9de46dd673b50ff72";

/// Format: the lines of the fixtures' text (the script's `seq 1 200`).
const TEXT_LINES: u32 = 200;

/// Format: the line the script's second commit rewrites (`s/^line 100 of/line one hundred of/`).
const REWRITTEN_LINE: u32 = 100;

/// Format: an empty pack's bytes, its twelve-byte header and its twenty-byte SHA-1 trailer.
const EMPTY_PACK: usize = 32;

// ---------------------------------------------------------------------------------------------------------------------
// Stock git's pushes.

/// One push stock git 2.50.1 made to stock `git receive-pack`, captured by the fixtures' script.
struct Scenario {
  /// Its name in the script.
  name: &'static str,
  /// What `git push` sent: its commands, then its pack.
  request: &'static [u8],
  /// What `git receive-pack` answered: its advertisement, then its report.
  response: &'static [u8],
  /// The server's refs before the push, `id SP name` a line.
  before: &'static str,
}

/// Includes a scenario's request, response and refs before it.
macro_rules! scenario {
  ($name:literal) => {
    Scenario {
      name: $name,
      request: include_bytes!(concat!("../../../git/fixtures/push/", $name, ".request.bin")),
      response: include_bytes!(concat!("../../../git/fixtures/push/", $name, ".response.bin")),
      before: include_str!(concat!("../../../git/fixtures/push/", $name, ".before.refs")),
    }
  };
}

/// The pushes that move refs, in the script's order, each with the refs the server held after it (the next push's
/// before): `main` created, fast-forwarded by a thin pack, an annotated tag, a branch at a commit held (an empty pack),
/// a branch at a new commit, that branch deleted.
fn chain() -> [(Scenario, &'static str); 6] {
  [
    (
      scenario!("create"),
      include_str!("../../../git/fixtures/push/update.before.refs"),
    ),
    (
      scenario!("update"),
      include_str!("../../../git/fixtures/push/tag.before.refs"),
    ),
    (
      scenario!("tag"),
      include_str!("../../../git/fixtures/push/copy.before.refs"),
    ),
    (
      scenario!("copy"),
      include_str!("../../../git/fixtures/push/side.before.refs"),
    ),
    (
      scenario!("side"),
      include_str!("../../../git/fixtures/push/delete.before.refs"),
    ),
    (
      scenario!("delete"),
      include_str!("../../../git/fixtures/push/options.before.refs"),
    ),
  ]
}

/// The refs of `lines`, `id SP name` a line.
fn refs(lines: &str) -> Vec<(ObjectId, Vec<u8>)> {
  lines
    .lines()
    .map(|line| {
      let (id, name) = line.split_once(' ').unwrap();
      (named(id), name.as_bytes().to_vec())
    })
    .collect()
}

/// The advertisement of `lines`' refs as a server advertising `advertised` writes it.
fn advertisement(lines: &str, advertised: &Advertised<'_>) -> Vec<u8> {
  let refs = refs(lines);
  let listed: Vec<(ObjectId, &[u8])> = refs
    .iter()
    .map(|(id, name)| (*id, name.as_slice()))
    .collect();
  let mut out = Vec::new();
  write_advertisement(&listed, advertised, &mut out).unwrap();
  out
}

/// What git answered `scenario` after its advertisement: its report.
fn gits_report(scenario: &Scenario) -> &'static [u8] {
  let advertised = advertisement(scenario.before, &CAPTURED);
  assert!(
    scenario.response.starts_with(&advertised),
    "{}: git's advertisement",
    scenario.name
  );
  &scenario.response[advertised.len()..]
}

/// The object `hex` names, in SHA-1.
fn named(hex: &str) -> ObjectId {
  ObjectId::from_hex(ObjectFormat::Sha1, hex.as_bytes()).unwrap()
}

/// The zero id: a create's old value, a delete's new one.
fn zero() -> ObjectId {
  ObjectId::from_bytes(ObjectFormat::Sha1, &[0; 20]).unwrap()
}

/// A push request of `commands` (old, new, ref), the first carrying `capabilities` as stock git writes them, then
/// `pack`.
fn request(commands: &[(&str, &str, &[u8])], capabilities: &[u8], pack: &[u8]) -> Vec<u8> {
  let mut out = Vec::new();
  for (at, (old, new, name)) in commands.iter().enumerate() {
    let mut line = format!("{} {} ", id_or_zero(old), id_or_zero(new)).into_bytes();
    line.extend_from_slice(name);
    if at == 0 {
      line.push(0);
      line.extend_from_slice(capabilities);
    }
    pktline::write(Packet::Data(&line), &mut out).unwrap();
  }
  pktline::write(Packet::Flush, &mut out).unwrap();
  out.extend_from_slice(pack);
  out
}

/// The id `hex` names, or the zero id for an empty one.
fn id_or_zero(hex: &str) -> ObjectId {
  if hex.is_empty() { zero() } else { named(hex) }
}

/// The empty pack stock git sends with a create at a commit the server holds: the `copy` push's last bytes.
fn empty_pack() -> &'static [u8] {
  let request = scenario!("copy").request;
  &request[request.len() - EMPTY_PACK..]
}

// ---------------------------------------------------------------------------------------------------------------------
// The owner shard.

/// The principal every test pushes as: the repositories' owner.
fn owner() -> Principal {
  Principal::Uid {
    host: HostId(0),
    uid: 1234,
  }
}

/// Makes git repository `name`, SHA-1, its default branch `main`, on the shard: its id.
fn git_repository(state: &mut ShardState, name: &str) -> RepositoryId {
  let reply = dispatch(
    state,
    1,
    &owner(),
    RequestBody::CreateRepository {
      name: name.to_owned(),
      tool: VcsTool::Git,
      format: WireFormat::Sha1,
      default_branch: "main".to_owned(),
      green: None,
    },
  );
  assert!(
    matches!(reply, ReplyBody::GreenCreated { .. }),
    "no repository: {reply:?}"
  );
  let repositories = state.db.partition().repositories();
  repositories.repository_named(name).unwrap().id
}

/// Sets repository `name`'s policy whole, from its version `read`: `rules` and `hooks`.
fn set_policy(
  state: &mut ShardState,
  name: &str,
  read: u64,
  (rules, hooks): (Vec<PolicyRule>, Vec<HookConfig>),
) {
  let reply = dispatch(
    state,
    1,
    &owner(),
    RequestBody::SetRepositoryPolicy {
      repository: name.to_owned(),
      read,
      rules,
      hooks,
      hooks_env: Vec::new(),
    },
  );
  assert_eq!(reply, ReplyBody::Acknowledged);
}

/// A rule denying everyone `right` on the refs `pattern` covers.
fn deny(right: PolicyRight, pattern: &str) -> PolicyRule {
  PolicyRule {
    names: PolicyNames::Refs,
    pattern: pattern.to_owned(),
    right,
    effect: PolicyEffect::Deny,
    who: PolicyWho::Anyone,
  }
}

/// A `pre-receive` hook running this test binary, which no test runs.
fn pre_receive() -> HookConfig {
  HookConfig {
    point: HookPointName::GitPreReceive,
    name: "pre-receive".to_owned(),
    program: std::env::current_exe()
      .unwrap()
      .to_string_lossy()
      .into_owned(),
  }
}

/// A push by the owner to `repository` started on the shard, advertising: the advertisement, and the driver.
fn started(state: &mut ShardState, repository: RepositoryId) -> (Vec<u8>, ReceivePack) {
  let mut advertised = Vec::new();
  let push = ReceivePack::start(state, (&owner(), repository), (ROOM, true), &mut advertised)
    .expect("started");
  (advertised, push)
}

/// `request` handed to `push` a piece at a time as an edge hands it in, then its end, then its answer: what the driver
/// wrote, and what it received.
fn answered(state: &mut ShardState, push: &mut ReceivePack, request: &[u8]) -> (Vec<u8>, Received) {
  for piece in request.chunks(PIECE) {
    push.input(state, piece).expect("taken");
  }
  push.end(state).expect("ended");
  let mut answer = Vec::new();
  let received = push.answer(state, &mut answer).expect("answered");
  (answer, received)
}

/// `request` pushed to `repository` by its owner: what was received.
fn pushed(state: &mut ShardState, repository: RepositoryId, request: &[u8]) -> Received {
  let (_, mut push) = started(state, repository);
  answered(state, &mut push, request).1
}

/// Each command's outcome in a report's words: `ok`, or the reason it was refused.
fn reasons(received: &Received) -> Vec<Vec<u8>> {
  received
    .outcomes
    .iter()
    .map(|outcome| match outcome {
      Outcome::Ok(_) => b"ok".to_vec(),
      Outcome::Ng(declined) => declined.reason.clone(),
    })
    .collect()
}

/// The records `repository`'s register holds, one a push appended (A-662).
fn records(state: &ShardState, repository: RepositoryId) -> usize {
  state.db.partition().repositories().objects(repository).count()
}

/// Whether `repository`'s namespace binds `name`.
fn bound(state: &ShardState, repository: RepositoryId, name: &str) -> bool {
  let repositories = state.db.partition().repositories();
  repositories.branch(repository, name).is_some()
}

/// The files the green `repository`'s branch `name` binds holds, with their bytes.
fn files(state: &ShardState, repository: RepositoryId, name: &str) -> Vec<(String, Vec<u8>)> {
  let green = state
    .db
    .partition()
    .repositories()
    .branch(repository, name)
    .unwrap();
  let engine = state.greens.get(&green).unwrap();
  engine
    .files()
    .map(|(path, bytes)| (path, bytes.to_vec()))
    .collect()
}

/// The fixtures' text: line `n` of two hundred, line one hundred rewritten when `rewritten` (the script's `sed`).
fn text(rewritten: bool) -> Vec<u8> {
  (1..=TEXT_LINES)
    .flat_map(|n| {
      let line = if rewritten && n == REWRITTEN_LINE {
        "line one hundred of a text long enough to be stored as a delta\n".to_owned()
      } else {
        format!("line {n} of a text long enough to be stored as a delta\n")
      };
      line.into_bytes()
    })
    .collect()
}

/// Asserts that each of `names` binds a green holding one file, `text`, as the fixtures' second commit names it.
fn assert_hold_the_second_text(state: &ShardState, repository: RepositoryId, names: &[&str]) {
  let second = vec![("text".to_owned(), text(true))];
  for name in names {
    assert_eq!(files(state, repository, name), second, "{name}");
  }
}

/// Pushes the fixtures' scenarios `names` in order to `repository`, each answered `ok` for every command.
fn pushed_from_the_fixtures(state: &mut ShardState, repository: RepositoryId, names: &[&str]) {
  let mut taken = Vec::new();
  for (scenario, _) in chain() {
    if names.contains(&scenario.name) {
      let received = pushed(state, repository, scenario.request);
      assert!(
        reasons(&received).iter().all(|reason| reason == b"ok"),
        "{}: {:?}",
        scenario.name,
        received.outcomes
      );
      taken.push(scenario.name);
    }
  }
  assert_eq!(taken, names);
}

/// A-654 part 2 (§4.17.2 "A push" items 1 to 7; golden vectors: six pushes stock git 2.50.1 made, captured by
/// `make-push-fixtures.sh`). Do: make a git repository on the owner shard; then, in the script's order, start each
/// push advertising, hand it the request git sent a few bytes at a time, end it and answer it: `main` created in the
/// empty repository, fast-forwarded by a thin pack, an annotated tag, a branch at a commit held (an empty pack), a
/// branch at a new commit, that branch deleted. Expect: each advertisement the refs git's server held then, their ids
/// and names in git's order under this server's capabilities; each answer after it the very bytes `git receive-pack`
/// wrote; then the advertisement of the refs git's server held after the last; `main` at its second version, the first
/// text it was created with and the second the update made, and `main`, `copy` and the tag each holding the second;
/// one record in the register a push.
#[test]
fn t_a654_stock_gits_pushes_are_answered_as_git_answered_and_each_ref_holds_its_tree() {
  crate::daemon::audit_on_shard(|state| {
    let repository = git_repository(state, "fixtures");
    let mut after = "";
    for (scenario, next) in chain() {
      let (advertised, mut push) = started(state, repository);
      let expected = advertisement(scenario.before, &OURS);
      assert_eq!(advertised, expected, "{}: the advertisement", scenario.name);
      let (answer, _) = answered(state, &mut push, scenario.request);
      assert_eq!(answer, gits_report(&scenario), "{}: the answer", scenario.name);
      if scenario.name == "create" {
        let first = vec![("text".to_owned(), text(false))];
        assert_eq!(files(state, repository, "main"), first);
      }
      after = next;
    }
    let (advertised, _) = started(state, repository);
    assert_eq!(advertised, advertisement(after, &OURS));
    assert_hold_the_second_text(state, repository, &["main", "copy", "refs/tags/v1"]);
    let main = state
      .db
      .partition()
      .repositories()
      .branch(repository, "main");
    assert_eq!(main.map(|green| state.db.partition().green_head(green)), Some(2));
    assert_eq!(records(state, repository), chain().len());
  });
}

/// A-654 part 2 (§4.17.1 "Policy"; §4.17.2 "Access", "Protected branches, in git's terms" and "Hooks, in git's
/// terms"; A-622). Do: push the fixtures' `create`, `update` and `copy`; then delete `copy` under a policy denying
/// everyone its reading, then its deletion; create `other` at a commit held under a rule denying its submission; delete
/// `copy` with a `pre-receive` hook configured; and with the policy cleared. Expect: `copy` left out of the
/// advertisement while unreadable and its delete refused as git refuses a hidden ref's; `deletion prohibited` with
/// git's band-2 line; the create refused as an update hook refusing it, git's line after slates' naming the rule;
/// `hook not run` naming the hook while one is configured; each refused push recording nothing and `copy` still bound;
/// then the delete applied, one record more.
#[test]
fn t_a654_the_policy_hides_and_refuses_in_gits_words_and_a_configured_hook_refuses_typed() {
  crate::daemon::audit_on_shard(|state| {
    let repository = git_repository(state, "policy");
    pushed_from_the_fixtures(state, repository, &["create", "update", "copy"]);
    let held = records(state, repository);
    let copy: &[u8] = b"refs/heads/copy";
    let delete = request(&[(SECOND, "", copy)], CAPABILITIES, &[]);
    set_policy(state, "policy", 0, (vec![deny(PolicyRight::Read, "refs/heads/copy")], Vec::new()));
    let (advertised, mut push) = started(state, repository);
    let main_alone = format!("{SECOND} refs/heads/main\n");
    assert_eq!(advertised, advertisement(&main_alone, &OURS));
    let (_, received) = answered(state, &mut push, &delete);
    assert_eq!(reasons(&received), [HIDDEN_DELETE]);
    set_policy(state, "policy", 1, (vec![deny(PolicyRight::Delete, "refs/heads/copy")], Vec::new()));
    let received = pushed(state, repository, &delete);
    let prohibited = Outcome::Ng(protected::deletion_prohibited(copy));
    assert_eq!(received.outcomes, [prohibited]);
    let other: &[u8] = b"refs/heads/other";
    let create = request(&[("", SECOND, other)], CAPABILITIES, empty_pack());
    set_policy(state, "policy", 2, (vec![deny(PolicyRight::Submit, "refs/heads/other")], Vec::new()));
    assert_declined_as_a_hook(&pushed(state, repository, &create), other, b"rule 0");
    set_policy(state, "policy", 3, (Vec::new(), vec![pre_receive()]));
    let received = pushed(state, repository, &delete);
    assert_eq!(reasons(&received), [HOOK_NOT_RUN]);
    assert_first_line_names(&received, b"pre-receive");
    assert_eq!(records(state, repository), held);
    assert!(bound(state, repository, "copy") && !bound(state, repository, "other"));
    set_policy(state, "policy", 4, (Vec::new(), Vec::new()));
    assert_eq!(reasons(&pushed(state, repository, &delete)), [b"ok"]);
    assert!(!bound(state, repository, "copy"));
    assert_eq!(records(state, repository), held + 1);
  });
}

/// Asserts that `received` refused its one command, `name`, as git answers an update hook refusing it (`hook
/// declined`, git's own line), the hook's words slates' and naming `why`.
fn assert_declined_as_a_hook(received: &Received, name: &[u8], why: &[u8]) {
  let [Outcome::Ng(declined)] = received.outcomes.as_slice() else {
    panic!("not one refusal: {:?}", received.outcomes);
  };
  let gits = protected::hook_declined(name, Vec::new());
  assert_eq!((&declined.reason, &declined.message), (&gits.reason, &gits.message));
  assert!(
    declined.hook.windows(why.len()).any(|words| words == why),
    "{}",
    String::from_utf8_lossy(&declined.hook)
  );
}

/// Asserts that the first command `received` refused has a band-2 line naming `what` (an atomic push's refusal gives
/// its line once, with its first command).
fn assert_first_line_names(received: &Received, what: &[u8]) {
  let line = received.outcomes.iter().find_map(|outcome| match outcome {
    Outcome::Ng(Declined {
      message: Some(line),
      ..
    }) => Some(line),
    _ => None,
  });
  let line = line.expect("a band-2 line");
  assert!(
    line.windows(what.len()).any(|words| words == what),
    "{}",
    String::from_utf8_lossy(line)
  );
}

/// A-654 part 2 (§4.17.2 "A push" item 7; A-635, A-642). Do: push the fixtures' `create`, `update`, `tag` and `copy`;
/// then, with `atomic`, delete `copy` and the tag, the tag's old value one it never held; make branch `cli` with the
/// CLI's verb, which git sees unborn; with `atomic`, delete the tag and create `cli` at a commit held; without
/// `atomic`, delete `copy` and the tag, the tag's old value wrong again; with `atomic`, delete the tag and create
/// `again` at a commit held. Expect: the first refused all or none (`atomic push failure` for `copy`, the tag's
/// `failed to update ref`); the second refused whole at its append (`atomic transaction failed` for both, git's line
/// naming `cli`), the tag still bound; neither recording anything; the third applying `copy`'s delete alone, one record
/// more; the fourth applying both as one entry, one record more, `again` holding the second commit's text.
#[test]
fn t_a654_a_push_with_atomic_is_all_or_none_and_without_it_a_ref_at_a_time() {
  crate::daemon::audit_on_shard(|state| {
    let repository = git_repository(state, "atomic");
    pushed_from_the_fixtures(state, repository, &["create", "update", "tag", "copy"]);
    let held = records(state, repository);
    let (copy, tag): (&[u8], &[u8]) = (b"refs/heads/copy", b"refs/tags/v1");
    let stale = [(SECOND, "", copy), (FIRST, "", tag)];
    let received = pushed(state, repository, &request(&stale, ATOMIC, &[]));
    assert_eq!(reasons(&received), [ATOMIC_FAILURE, FAILED_TO_UPDATE]);
    let reply = dispatch(
      state,
      1,
      &owner(),
      RequestBody::CreateBranch {
        repository: "atomic".to_owned(),
        name: "cli".to_owned(),
        from: "main".to_owned(),
        version: None,
      },
    );
    assert!(matches!(reply, ReplyBody::GreenCreated { .. }), "{reply:?}");
    let cli = [(TAG, "", tag), ("", SECOND, b"refs/heads/cli".as_slice())];
    let received = pushed(state, repository, &request(&cli, ATOMIC, empty_pack()));
    assert_eq!(reasons(&received), [ATOMIC_TRANSACTION_FAILED, ATOMIC_TRANSACTION_FAILED]);
    assert_first_line_names(&received, b"refs/heads/cli");
    assert_eq!(records(state, repository), held);
    assert!(bound(state, repository, "copy") && bound(state, repository, "refs/tags/v1"));
    let received = pushed(state, repository, &request(&stale, CAPABILITIES, &[]));
    assert_eq!(reasons(&received), [b"ok".as_slice(), FAILED_TO_UPDATE]);
    assert_eq!(records(state, repository), held + 1);
    assert!(!bound(state, repository, "copy") && bound(state, repository, "refs/tags/v1"));
    let again = [(TAG, "", tag), ("", SECOND, b"refs/heads/again".as_slice())];
    let received = pushed(state, repository, &request(&again, ATOMIC, empty_pack()));
    assert_eq!(reasons(&received), [b"ok", b"ok"]);
    assert_eq!(records(state, repository), held + 2);
    assert!(!bound(state, repository, "refs/tags/v1"));
    assert_hold_the_second_text(state, repository, &["again"]);
  });
}
