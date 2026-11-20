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

use super::{AGENT, Answering, HOOK_NOT_RUN, ReceivePack, Unpresented};
use crate::state::ShardState;
use crate::verbs::dispatch;

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
pub(crate) struct Scenario {
  /// Its name in the script.
  pub(crate) name: &'static str,
  /// What `git push` sent: its commands, then its pack.
  pub(crate) request: &'static [u8],
  /// What `git receive-pack` answered: its advertisement, then its report.
  pub(crate) response: &'static [u8],
  /// The server's refs before the push, `id SP name` a line.
  pub(crate) before: &'static str,
}

/// Includes a scenario's request, response and refs before it.
macro_rules! scenario {
  ($name:literal) => {
    Scenario {
      name: $name,
      request: include_bytes!(concat!(
        "../../../git/fixtures/push/",
        $name,
        ".request.bin"
      )),
      response: include_bytes!(concat!(
        "../../../git/fixtures/push/",
        $name,
        ".response.bin"
      )),
      before: include_str!(concat!(
        "../../../git/fixtures/push/",
        $name,
        ".before.refs"
      )),
    }
  };
}

/// The pushes that move refs, in the script's order, each with the refs the server held after it (the next push's
/// before): `main` created, fast-forwarded by a thin pack, an annotated tag, a branch at a commit held (an empty pack),
/// a branch at a new commit, that branch deleted.
pub(crate) fn chain() -> [(Scenario, &'static str); 6] {
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
pub(crate) fn gits_report(scenario: &Scenario) -> &'static [u8] {
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
pub(crate) fn owner() -> Principal {
  Principal::Uid {
    host: HostId(0),
    uid: 1234,
  }
}

/// Makes git repository `name`, SHA-1, its default branch `main`, on the shard: its id.
pub(crate) fn git_repository(state: &mut ShardState, name: &str) -> RepositoryId {
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
      signers: Vec::new(),
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

/// A push by the owner to `repository` started on the shard, advertising, under the daemon's
/// presenter, which shows no hook's view: the advertisement, and the driver.
fn started(state: &mut ShardState, repository: RepositoryId) -> (Vec<u8>, ReceivePack) {
  let mut advertised = Vec::new();
  let push = ReceivePack::start(
    state,
    (&owner(), repository),
    true,
    &mut advertised,
    Unpresented,
  )
  .expect("started");
  (advertised, push)
}

/// `request` handed to `push` a piece at a time as an edge hands it in, then its end, then its
/// answer, a step at a time while its checks turn (A-719) under a presenter that shows no view:
/// what the driver wrote, and what it received.
fn answered(state: &mut ShardState, push: &mut ReceivePack, request: &[u8]) -> (Vec<u8>, Received) {
  for piece in request.chunks(PIECE) {
    push.input(state, piece).expect("taken");
  }
  push.end(state).expect("ended");
  let mut answer = Vec::new();
  loop {
    match push.answer_step(state, &mut answer).expect("answered") {
      Answering::Answered(received) => return (answer, *received),
      Answering::Turned => {}
      #[cfg(unix)]
      step @ Answering::Hook(_) => panic!("no hook runs where no view is shown: {step:?}"),
    }
  }
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
  state
    .db
    .partition()
    .repositories()
    .objects(repository)
    .count()
}

/// Whether `repository`'s namespace binds `name`.
fn bound(state: &ShardState, repository: RepositoryId, name: &str) -> bool {
  let repositories = state.db.partition().repositories();
  repositories.branch(repository, name).is_some()
}

/// The files the green `repository`'s branch `name` binds holds, with their bytes.
pub(crate) fn files(
  state: &ShardState,
  repository: RepositoryId,
  name: &str,
) -> Vec<(String, Vec<u8>)> {
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
pub(crate) fn text(rewritten: bool) -> Vec<u8> {
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
pub(crate) fn pushed_from_the_fixtures(
  state: &mut ShardState,
  repository: RepositoryId,
  names: &[&str],
) {
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
      assert_eq!(
        answer,
        gits_report(&scenario),
        "{}: the answer",
        scenario.name
      );
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
    assert_eq!(
      main.map(|green| state.db.partition().green_head(green)),
      Some(2)
    );
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
    set_policy(
      state,
      "policy",
      0,
      (vec![deny(PolicyRight::Read, "refs/heads/copy")], Vec::new()),
    );
    let (advertised, mut push) = started(state, repository);
    let main_alone = format!("{SECOND} refs/heads/main\n");
    assert_eq!(advertised, advertisement(&main_alone, &OURS));
    let (_, received) = answered(state, &mut push, &delete);
    assert_eq!(reasons(&received), [HIDDEN_DELETE]);
    set_policy(
      state,
      "policy",
      1,
      (
        vec![deny(PolicyRight::Delete, "refs/heads/copy")],
        Vec::new(),
      ),
    );
    let received = pushed(state, repository, &delete);
    let prohibited = Outcome::Ng(protected::deletion_prohibited(copy));
    assert_eq!(received.outcomes, [prohibited]);
    let other: &[u8] = b"refs/heads/other";
    let create = request(&[("", SECOND, other)], CAPABILITIES, empty_pack());
    set_policy(
      state,
      "policy",
      2,
      (
        vec![deny(PolicyRight::Submit, "refs/heads/other")],
        Vec::new(),
      ),
    );
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
  assert_eq!(
    (&declined.reason, &declined.message),
    (&gits.reason, &gits.message)
  );
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
/// CLI's verb at `main`'s version before its first push, which git sees unborn; with `atomic`, delete the tag and create `cli` at a commit held; without
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
    let stale = [(SECOND, "", COPY_REF), (FIRST, "", TAG_REF)];
    let received = pushed(state, repository, &request(&stale, ATOMIC, &[]));
    assert_eq!(reasons(&received), [ATOMIC_FAILURE, FAILED_TO_UPDATE]);
    atomic_refused_whole_at_its_append(state, repository, held);
    let received = pushed(state, repository, &request(&stale, CAPABILITIES, &[]));
    assert_eq!(reasons(&received), [b"ok".as_slice(), FAILED_TO_UPDATE]);
    assert_eq!(records(state, repository), held + 1);
    assert!(!bound(state, repository, "copy") && bound(state, repository, "refs/tags/v1"));
    atomic_applied_as_one_entry(state, repository, held + 1);
  });
}

/// Format: the ref the atomic test's fixtures push as `copy`.
const COPY_REF: &[u8] = b"refs/heads/copy";
/// Format: the tag the atomic test's fixtures push.
const TAG_REF: &[u8] = b"refs/tags/v1";

/// Branch `cli` made with the CLI's verb at `main`'s version before its first push (no commit, so git sees it unborn), then, with `atomic`, the tag deleted and `cli` created at a commit held:
/// refused whole at its append, git's line naming `cli`, the tag still bound and the `held` records unchanged.
fn atomic_refused_whole_at_its_append(
  state: &mut ShardState,
  repository: RepositoryId,
  held: usize,
) {
  let reply = dispatch(
    state,
    1,
    &owner(),
    RequestBody::CreateBranch {
      repository: "atomic".to_owned(),
      name: "cli".to_owned(),
      from: "main".to_owned(),
      version: Some(0),
    },
  );
  assert!(matches!(reply, ReplyBody::GreenCreated { .. }), "{reply:?}");
  let cli = [
    (TAG, "", TAG_REF),
    ("", SECOND, b"refs/heads/cli".as_slice()),
  ];
  let received = pushed(state, repository, &request(&cli, ATOMIC, empty_pack()));
  assert_eq!(
    reasons(&received),
    [ATOMIC_TRANSACTION_FAILED, ATOMIC_TRANSACTION_FAILED]
  );
  assert_first_line_names(&received, b"refs/heads/cli");
  assert_eq!(records(state, repository), held);
  assert!(bound(state, repository, "copy") && bound(state, repository, "refs/tags/v1"));
}

/// With `atomic`, the tag deleted and `again` created at a commit held: both applied as one entry past the `held`
/// records, `again` holding the second commit's text.
fn atomic_applied_as_one_entry(state: &mut ShardState, repository: RepositoryId, held: usize) {
  let again = [
    (TAG, "", TAG_REF),
    ("", SECOND, b"refs/heads/again".as_slice()),
  ];
  let received = pushed(state, repository, &request(&again, ATOMIC, empty_pack()));
  assert_eq!(reasons(&received), [b"ok", b"ok"]);
  assert_eq!(records(state, repository), held + 1);
  assert!(!bound(state, repository, "refs/tags/v1"));
  assert_hold_the_second_text(state, repository, &["again"]);
}

/// A-743 (main's ruling 2026-11-20, found by use: a branch the CLI made never showed in `git ls-remote`; §4.17's concept
/// table, a slates branch a git ref; A-654's `ref_of`, name `N` ref `refs/heads/N`). Do: push the fixtures' `create`
/// (`main` at the first commit); make branch `topic` from `main` with the CLI's verb; read the advertisement; retire
/// `topic` and read it; bind `topic` again to the green the create answered and read it; then push `topic` from the first
/// commit to the second. Expect: `topic` advertised at the first commit, gone after the retire, back after the bind;
/// the push `ok`, `topic` still bound to the green the CLI made (no second green, the lineage's members unchanged),
/// holding the second commit's text. Before that push, the store made again from the log advertises `topic` too.
#[test]
fn t_a743_a_branch_the_namespace_binds_is_a_ref_git_sees_and_pushes_move_its_green() {
  crate::daemon::audit_on_shard(|state| {
    let repository = git_repository(state, "bindings");
    pushed_from_the_fixtures(state, repository, &["create"]);
    let branch = |state: &mut ShardState, body: RequestBody| dispatch(state, 1, &owner(), body);
    let made = branch(
      state,
      RequestBody::CreateBranch {
        repository: "bindings".to_owned(),
        name: "topic".to_owned(),
        from: "main".to_owned(),
        version: None,
      },
    );
    let ReplyBody::GreenCreated { id: green } = made else {
      panic!("no branch: {made:?}");
    };
    let topic = (named(FIRST), b"refs/heads/topic".to_vec());
    let after_create = super::advertised_refs(state, repository).contains(&topic);
    let retired = branch(
      state,
      RequestBody::RetireBranch {
        repository: "bindings".to_owned(),
        name: "topic".to_owned(),
      },
    );
    assert!(matches!(retired, ReplyBody::Acknowledged), "{retired:?}");
    let after_retire = super::advertised_refs(state, repository)
      .iter()
      .any(|(_, name)| name == b"refs/heads/topic");
    let bound_again = branch(
      state,
      RequestBody::BindBranch {
        repository: "bindings".to_owned(),
        name: "topic".to_owned(),
        green,
      },
    );
    assert!(
      matches!(bound_again, ReplyBody::Acknowledged),
      "{bound_again:?}"
    );
    let after_bind = super::advertised_refs(state, repository).contains(&topic);
    assert_eq!(
      (after_create, after_retire, after_bind),
      (true, false, true)
    );
    crate::vcs_store::rebuild(state, repository);
    crate::vcs_store::settle_rebuild(state, repository);
    assert!(super::advertised_refs(state, repository).contains(&topic));
    let members = state
      .db
      .partition()
      .repositories()
      .members_of(repository)
      .count();
    let update = scenario!("update").request;
    let pack = &update[update
      .windows(4)
      .position(|magic| magic == b"PACK")
      .unwrap()..];
    let moved = [(FIRST, SECOND, b"refs/heads/topic".as_slice())];
    let received = pushed(state, repository, &request(&moved, CAPABILITIES, pack));
    assert_eq!(reasons(&received), [b"ok"]);
    let repositories = state.db.partition().repositories();
    assert_eq!(
      (
        repositories.branch(repository, "topic"),
        repositories.members_of(repository).count()
      ),
      (Some(crate::verbs::to_db_volume(green)), members)
    );
    assert_hold_the_second_text(state, repository, &["topic"]);
  });
}

/// A-722 part 4 (a record's replay a unit a turn, its pack's waiting deltas resolved a delta a unit). Do: push the
/// fixtures' `create` and `update` (a thin pack, its delta's base outside it); make the store again from the log, each
/// turn held to no time at all, counting the turns. Expect: more turns than records; the refs advertised after as
/// before.
#[test]
fn t_a722_p4_a_thin_packs_record_is_replayed_in_turns_with_its_deltas_resolved() {
  crate::daemon::audit_on_shard(|state| {
    let repository = git_repository(state, "thin-replayed");
    pushed_from_the_fixtures(state, repository, &["create", "update"]);
    let before = super::advertised_refs(state, repository);
    let held = records(state, repository);
    crate::vcs_store::rebuild(state, repository);
    let mut turns = 0usize;
    while crate::vcs_store::rebuild_turn_once_within(state, repository, 0) {
      turns += 1;
    }
    crate::vcs_store::paths::whole_build::settle(state);
    assert!(turns > held, "{turns} turns for {held} records");
    assert_eq!(super::advertised_refs(state, repository), before);
  });
}

/// Shape: the partition's green-chain budget in [`t_a753_a_repository_takes_branch_creates_past_its_chain_budget`]:
/// small, so a few creates' works pass it.
const SMALL_CHAIN_BUDGET: usize = 64 << 10;

/// Shape: the branches [`t_a753_a_repository_takes_branch_creates_past_its_chain_budget`] creates: enough that their
/// works together pass [`SMALL_CHAIN_BUDGET`] several times over.
const CREATES: usize = 24;

/// A-753 (found by use, A-722's proof: of 100 concurrent v1.5.0 pushes creating a branch each in one repository, 20 were
/// answered `failed to update ref`, the append refused `green_chains` at capacity: each create held the whole tree as
/// its own work). Do: with a small green-chain budget, push [`CREATES`] creates of branches, each at the fixtures' first
/// commit, each resending its whole pack. Expect: every create `ok`, its branch bound, every branch's green holding the
/// first commit's text. Red before the change: every create after the first refused `missing necessary objects` (the
/// connectivity check read the resent objects from the quarantine, which passes over what the index holds); and, that
/// fixed, refused at the chains' cap, each create making its whole tree again rather than branching from the green that
/// holds its commit.
#[test]
fn t_a753_a_repository_takes_branch_creates_past_its_chain_budget() {
  let refused = crate::daemon::audit_on_shard_configured(
    |state| {
      let repository = git_repository(state, "chains");
      let pack = pack_of(scenario!("create").request);
      let mut refused = Vec::new();
      for k in 0..CREATES {
        let name = format!("refs/heads/p{k}");
        let created = [("", FIRST, name.as_bytes())];
        let received = pushed(state, repository, &request(&created, CAPABILITIES, pack));
        if reasons(&received) != [b"ok".to_vec()] || !bound(state, repository, &format!("p{k}")) {
          if k == 1 {
            eprintln!("DBG {:?}", received.outcomes);
          }
          refused.push(k);
        }
      }
      let first = vec![("text".to_owned(), text(false))];
      let wrong = (0..CREATES)
        .filter(|k| !refused.contains(k))
        .filter(|k| files(state, repository, &format!("p{k}")) != first)
        .count();
      (refused, wrong)
    },
    |config| config.caps.green_chain_bytes = SMALL_CHAIN_BUDGET,
  );
  assert_eq!(refused, (Vec::new(), 0));
}

// ---------------------------------------------------------------------------------------------------------------------
// The hooks (A-665 part 3; Unix, where the hook driver runs).

/// A hook program: a shell script running `body`, in a scratch directory of its own removed with
/// it, and its configuration at `point` under `name`.
#[cfg(unix)]
struct Script {
  /// Its directory.
  _dir: crate::git_hooks::tests::ScratchDir,
  /// The hook as the repository's policy names it.
  config: HookConfig,
}

/// Writes the script `name` running `body` and configures it at `point`.
#[cfg(unix)]
#[allow(clippy::disallowed_methods)] // test code: a script in its own scratch directory
fn script(point: HookPointName, name: &str, body: &str) -> Script {
  use std::os::unix::fs::PermissionsExt;
  let dir = crate::git_hooks::tests::ScratchDir::new();
  let path = dir.path.join(name);
  std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
  std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
  let config = HookConfig {
    point,
    name: name.to_owned(),
    program: path.to_string_lossy().into_owned(),
  };
  Script { _dir: dir, config }
}

/// Shape: how long a test waits for a push whose hooks run: a test's bound for each of its three
/// hooks and one for the push itself.
#[cfg(unix)]
const PUSH_WAIT_NS: u64 = 4 * crate::git_hooks::tests::TEST_BOUND_NS;

/// What a push driven with its hooks answered: what the driver wrote, what it received, and how
/// many views the presenter showed and withdrew; or why the drive stopped.
#[cfg(unix)]
type Driven = Result<(Vec<u8>, Received, (u32, u32)), String>;

/// `request` pushed to `repository` by its owner as the HTTP edge pushes one
/// (`vcs_http::git::receive`): a task on the daemon's shard drives the push a step at a time, the
/// shard's state lent for each step, yielding while a view is sealed and running each hook off the
/// shard's step in a view the scratch presenter shows. What it answered.
#[cfg(unix)]
fn pushed_with_hooks(daemon: &crate::Daemon, repository: RepositoryId, request: &[u8]) -> Driven {
  let (sent, got) = std::sync::mpsc::channel();
  let request = request.to_vec();
  crate::daemon::observe_first(daemon, move |_| {
    let task =
      slates_rt::futures::spawn(drive(repository, request, sent)).expect("the push's task");
    slates_rt::futures::detach(task).expect("the task detached");
  });
  got
    .recv_timeout(std::time::Duration::from_nanos(PUSH_WAIT_NS))
    .unwrap_or_else(|_| Err("the push did not answer within the test's bound".to_owned()))
}

/// Drives the push of [`pushed_with_hooks`] and sends what it answered on `sent`.
#[cfg(unix)]
async fn drive(repository: RepositoryId, request: Vec<u8>, sent: std::sync::mpsc::Sender<Driven>) {
  let _ = sent.send(driven(repository, &request).await);
}

/// The push of [`pushed_with_hooks`], driven.
#[cfg(unix)]
async fn driven(repository: RepositoryId, request: &[u8]) -> Driven {
  use crate::state::with_state;
  let presenter = crate::git_hooks::tests::ScratchPresenter::default();
  let mut push = with_state(|state| {
    ReceivePack::start(
      state,
      (&owner(), repository),
      false,
      &mut Vec::new(),
      presenter,
    )
  })
  .ok_or("no state to start")?
  .map_err(|failed| format!("not started: {failed:?}"))?;
  for piece in request.chunks(PIECE) {
    with_state(|state| push.input(state, piece))
      .ok_or("no state for the request")?
      .map_err(|failed| format!("not taken: {failed:?}"))?;
  }
  with_state(|state| push.end(state))
    .ok_or("no state for the end")?
    .map_err(|failed| format!("not ended: {failed:?}"))?;
  let mut answer = Vec::new();
  let received = loop {
    let step = with_state(|state| push.answer_step(state, &mut answer))
      .ok_or("no state for a step")?
      .map_err(|failed| format!("a step failed: {failed:?}"))?;
    match step {
      Answering::Answered(received) => break *received,
      Answering::Turned => slates_rt::futures::yield_now().await,
      Answering::Hook(call) => {
        let ran = call.run(std::future::pending()).await;
        with_state(|state| push.hook_ran(state, ran, &mut answer))
          .ok_or("no state for the hook's answer")?
          .map_err(|failed| format!("its answer not taken: {failed:?}"))?;
      }
    }
  };
  let shown = (push.presenter.presented, push.presenter.withdrawn);
  Ok((answer, received, shown))
}

/// Each packet of `answer` in order, as its band and payload: band 0 for a packet not multiplexed,
/// none for a flush.
#[cfg(unix)]
fn bands(answer: &[u8]) -> Vec<(u8, Vec<u8>)> {
  let mut packets = Vec::new();
  let mut rest = answer;
  while rest.len() >= 4 {
    let len = usize::from_str_radix(std::str::from_utf8(&rest[..4]).unwrap(), 16).unwrap();
    if len == 0 {
      rest = &rest[4..];
      continue;
    }
    let (packet, after) = rest.split_at(len);
    packets.push((packet[4], packet[5..].to_vec()));
    rest = after;
  }
  packets
}

/// The text of `answer`'s band-2 packets, in order: what a stock client prints after `remote: `.
#[cfg(unix)]
fn band_two(answer: &[u8]) -> String {
  let said: Vec<u8> = bands(answer)
    .into_iter()
    .filter(|(band, _)| *band == 2)
    .flat_map(|(_, payload)| payload)
    .collect();
  String::from_utf8(said).unwrap()
}

/// The pack of a request stock git made: its bytes from the pack's magic on.
#[cfg(unix)]
fn pack_of(request: &[u8]) -> &[u8] {
  let at = request
    .windows(4)
    .position(|magic| magic == b"PACK")
    .unwrap();
  &request[at..]
}

/// The files the green `name` of `repository` holds on `daemon`'s shard, and the records its
/// register holds.
#[cfg(unix)]
fn held(
  daemon: &crate::Daemon,
  repository: RepositoryId,
  name: &'static str,
) -> (Vec<(String, Vec<u8>)>, usize, bool) {
  crate::daemon::observe_first(daemon, move |state| {
    let is_bound = bound(state, repository, name);
    let held_files = if is_bound {
      files(state, repository, name)
    } else {
      Vec::new()
    };
    (held_files, records(state, repository), is_bound)
  })
}

/// Makes git repository `name` on `daemon`'s shard with `hooks` configured: its id.
#[cfg(unix)]
fn hooked_repository(
  daemon: &crate::Daemon,
  name: &'static str,
  hooks: &[&Script],
) -> RepositoryId {
  let configs: Vec<HookConfig> = hooks.iter().map(|hook| hook.config.clone()).collect();
  crate::daemon::observe_first(daemon, move |state| {
    let repository = git_repository(state, name);
    set_policy(state, name, 0, (Vec::new(), configs));
    repository
  })
}

/// A-665 part 3 (§4.17.2 "Hooks, in git's terms"; git's `execute_commands`, `copy_to_sideband`).
/// Do: configure a `pre-receive` hook that writes a line and exits 1; push stock git's `create` as
/// the HTTP edge pushes, each hook's view shown in a scratch directory. Expect: `main` refused
/// `pre-receive hook declined`; the hook's line on band 2, once; nothing appended and `main`
/// holding no file; one view shown and withdrawn.
#[cfg(unix)]
#[test]
fn t_a665_a_push_whose_pre_receive_hook_refuses_gets_gits_words_and_moves_nothing() {
  let daemon = crate::daemon::audit_daemon();
  let hook = script(
    HookPointName::GitPreReceive,
    "pre-receive",
    "cat >/dev/null\necho 'no pushes today'\nexit 1",
  );
  let repository = hooked_repository(&daemon, "refused", &[&hook]);
  let driven = pushed_with_hooks(&daemon, repository, scenario!("create").request);
  let held = held(&daemon, repository, "main");
  daemon.stop();
  let (answer, received, shown) = driven.unwrap();
  assert_eq!(reasons(&received), [protected::PRE_RECEIVE_HOOK_DECLINED]);
  assert_eq!(band_two(&answer), "no pushes today\n");
  assert_eq!(held, (Vec::new(), 0, true));
  assert_eq!(shown, (1, 1));
}

/// A-665 part 3 (§4.17.2 "Hooks, in git's terms"; git's `execute_commands`, `update()` and the end
/// of `cmd_receive_pack`). Do: configure `pre-receive` and `update` hooks that write what they were
/// handed and exit 0, and a `post-receive` hook that writes its standard input and the
/// `packed-refs` of the repository it runs in; push stock git's `create` as the HTTP edge pushes.
/// Expect: `main` applied and holding the first text, one record appended; on band 2,
/// `pre-receive`'s line naming the create, then `update`'s naming the ref and its two ids, before
/// the report on band 1, then `post-receive`'s lines after it, its view's `packed-refs` naming
/// `main` at the pushed commit: it ran after the update; three views shown and withdrawn; each hook's
/// one run taught the shard's table of learned times (A-665 part 4b).
#[cfg(unix)]
#[test]
fn t_a665_a_push_whose_hooks_accept_lands_and_post_receive_runs_after_the_update() {
  let daemon = crate::daemon::audit_daemon();
  let pre = script(
    HookPointName::GitPreReceive,
    "pre-receive",
    "echo \"pre-receive $(cat)\"",
  );
  let update = script(
    HookPointName::GitUpdate,
    "update",
    "echo \"update $1 $2 $3\"",
  );
  let post = script(
    HookPointName::GitPostReceive,
    "post-receive",
    "echo \"post-receive $(cat)\"\ncat packed-refs",
  );
  let repository = hooked_repository(&daemon, "accepted", &[&pre, &update, &post]);
  let driven = pushed_with_hooks(&daemon, repository, scenario!("create").request);
  let held = held(&daemon, repository, "main");
  let learned = finished_runs(&daemon, repository, &[&pre, &update, &post]);
  daemon.stop();
  let (answer, received, shown) = driven.unwrap();
  assert_eq!(reasons(&received), [b"ok"]);
  assert_eq!(held, (vec![("text".to_owned(), text(false))], 1, true));
  let zero = "0".repeat(FIRST.len());
  let said = band_two(&answer);
  let lines = format!(
    "pre-receive {zero} {FIRST} refs/heads/main\nupdate refs/heads/main {zero} {FIRST}\n\
     post-receive {zero} {FIRST} refs/heads/main\n"
  );
  assert!(said.starts_with(&lines), "{said}");
  assert!(
    said.contains(&format!("\n{FIRST} refs/heads/main\n")),
    "{said}"
  );
  assert_report_between_the_hooks(&answer);
  assert_eq!(shown, (3, 3));
  assert_eq!(learned, [Some(1); 3], "each hook learned its one run");
}

/// How many finished runs the shard's table of learned times (A-665 part 4b) holds for each of
/// `repository`'s `hooks` on `daemon`'s shard; none for a hook it holds nothing of.
#[cfg(unix)]
fn finished_runs(
  daemon: &crate::Daemon,
  repository: RepositoryId,
  hooks: &[&Script],
) -> Vec<Option<u64>> {
  let names: Vec<String> = hooks.iter().map(|hook| hook.config.name.clone()).collect();
  crate::daemon::observe_first(daemon, move |state| {
    let record = super::record_of(state, repository).expect("the repository's record");
    names
      .iter()
      .map(|name| {
        let hook = record.policy.hooks.iter().find(|hook| hook.name == *name)?;
        state
          .hook_times
          .times(repository, hook)
          .map(crate::vcs_hooks::HookTimes::finished_runs)
      })
      .collect()
  })
}

/// Asserts that `answer`'s report, on band 1, comes after two band-2 packets (`pre-receive`'s and
/// `update`'s words) and before at least one (`post-receive`'s), every other packet being band 2's.
#[cfg(unix)]
fn assert_report_between_the_hooks(answer: &[u8]) {
  let order: Vec<u8> = bands(answer).iter().map(|(band, _)| *band).collect();
  let report = order.iter().position(|band| *band == 1).unwrap();
  let others: Vec<u8> = order
    .iter()
    .enumerate()
    .filter(|(at, _)| *at != report)
    .map(|(_, band)| *band)
    .collect();
  assert!(others.iter().all(|band| *band == 2), "{order:?}");
  assert!(report >= 2 && report + 1 < order.len(), "{order:?}");
}

/// A-665 part 3 (§4.17.2 "Hooks, in git's terms"; git's `update()`, `run_update_hook()`). Do:
/// configure an `update` hook that refuses `refs/heads/other` with a line and lets every other ref
/// through; push one pack creating `main` and `other` at the fixtures' first commit, as the HTTP
/// edge pushes. Expect: `main` applied and holding the first text; `other` refused `hook declined`,
/// the hook's line then git's `error: hook declined to update refs/heads/other` on band 2, and left
/// unbound; one record appended; two views shown and withdrawn, the second command's hook having
/// run too.
#[cfg(unix)]
#[test]
fn t_a665_an_update_hook_refuses_its_own_command_alone() {
  let daemon = crate::daemon::audit_daemon();
  let hook = script(
    HookPointName::GitUpdate,
    "update",
    "if [ \"$1\" = refs/heads/other ]; then echo \"no $1\"; exit 1; fi",
  );
  let repository = hooked_repository(&daemon, "update", &[&hook]);
  let pack = pack_of(scenario!("create").request);
  let main: &[u8] = b"refs/heads/main";
  let other: &[u8] = b"refs/heads/other";
  let both = request(&[("", FIRST, main), ("", FIRST, other)], CAPABILITIES, pack);
  let driven = pushed_with_hooks(&daemon, repository, &both);
  let main_held = held(&daemon, repository, "main");
  let other_held = held(&daemon, repository, "other");
  daemon.stop();
  let (answer, received, shown) = driven.unwrap();
  assert_eq!(
    reasons(&received),
    [b"ok".as_slice(), protected::HOOK_DECLINED]
  );
  assert_eq!(main_held, (vec![("text".to_owned(), text(false))], 1, true));
  assert!(!other_held.2);
  let words = "no refs/heads/other\nerror: hook declined to update refs/heads/other\n";
  assert_eq!(band_two(&answer), words);
  assert_eq!(shown, (2, 2));
}

/// Shape: the file of [`t_a719_a_pushs_pack_is_read_into_its_quarantine_holding_none_of_its_bytes`]: a mebibyte,
/// many times what the push's tables take.
const LARGE: usize = 1 << 20;
/// Format: the most bytes one stored deflate block holds (RFC 1951 §3.2.4).
const STORED_BLOCK: usize = 0xffff;
/// Format: Adler-32's modulus (RFC 1950 §8).
const ADLER_MODULUS: u32 = 65_521;

/// `data` as a zlib stream of stored blocks.
fn stored_zlib(data: &[u8]) -> Vec<u8> {
  let mut out = vec![0x78, 0x01];
  let mut blocks = data.chunks(STORED_BLOCK).peekable();
  while let Some(block) = blocks.next() {
    out.push(u8::from(blocks.peek().is_none()));
    let len = u16::try_from(block.len()).unwrap();
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

/// A create of `main` at a root commit whose one file is `bytes` long, as stock git posts it, and the file's name.
fn large_create(bytes: usize) -> (Vec<u8>, ObjectId) {
  use slates_git::entry::{self, Header, Representation};
  use slates_git::object_name::{Kind, name_of};
  let blob: Vec<u8> = (0..bytes)
    .map(|at| u8::try_from(at % 251).unwrap_or(u8::MAX))
    .collect();
  let blob_id = name_of(ObjectFormat::Sha1, Kind::Blob, &blob).unwrap();
  let mut tree = b"100644 large\0".to_vec();
  tree.extend_from_slice(blob_id.as_bytes());
  let tree_id = name_of(ObjectFormat::Sha1, Kind::Tree, &tree).unwrap();
  let commit = format!(
    "tree {tree_id}\nauthor A <a@example.com> 1700000000 +0000\n\
     committer A <a@example.com> 1700000000 +0000\n\nlarge\n"
  )
  .into_bytes();
  let commit_id = name_of(ObjectFormat::Sha1, Kind::Commit, &commit).unwrap();
  let mut pack = Vec::new();
  let objects = [
    (Kind::Commit, &commit),
    (Kind::Tree, &tree),
    (Kind::Blob, &blob),
  ];
  let mut writer = slates_git::pack::Writer::new(ObjectFormat::Sha1, 3, &mut pack);
  let mut at = entry::PACK_HEADER_LEN;
  for (kind, content) in objects {
    let header = Header {
      representation: Representation::Whole(kind),
      size: u64::try_from(content.len()).unwrap(),
    };
    let zlib = stored_zlib(content);
    entry::write(header, at, &mut writer).unwrap();
    slates_git::out::Out::put(&mut writer, &zlib);
    at += entry::written_len(header, at).unwrap() + u64::try_from(zlib.len()).unwrap();
  }
  writer.finish().unwrap();
  let request = request(
    &[("", &commit_id.to_string(), b"refs/heads/main")],
    CAPABILITIES,
    &pack,
  );
  (request, blob_id)
}

/// A-719 part 3 (§4.17.1 "A push's intake", rule 1). Do: push a create of `main` at a commit whose one file is a
/// mebibyte, handing the driver its request a piece at a time, then answer it. Expect: once its bytes are all taken,
/// the file in the push's quarantine and the push's admission holding the pack as sent (A-723's record) and less than
/// a sixteenth of the file beside it (each object's bytes given back once the quarantine took it); then `main`
/// created.
#[test]
fn t_a719_a_pushs_pack_is_read_into_its_quarantine_holding_none_of_its_bytes() {
  crate::daemon::audit_on_shard(|state| {
    let repository = git_repository(state, "large");
    let (_, mut push) = started(state, repository);
    let (request, blob) = large_create(LARGE);
    let room = push.admission.left();
    for piece in request.chunks(LARGE / 16) {
      push.input(state, piece).expect("taken");
    }
    let held = room - push.admission.left();
    let quarantined = push
      .quarantine
      .as_ref()
      .and_then(|quarantine| quarantine.quarantine().object(&blob));
    assert!(quarantined.is_some(), "the file is not in the quarantine");
    // The pack as the client sent it is the record's (A-723); nothing inflated is held beside it.
    let sent = request.len() as u64;
    assert!(
      held < sent + (LARGE as u64) / 16,
      "the push holds {held} bytes of a {sent}-byte request"
    );
    push.end(state).expect("ended");
    let mut answer = Vec::new();
    let received = loop {
      match push.answer_step(state, &mut answer).expect("answered") {
        Answering::Answered(received) => break *received,
        Answering::Turned => {}
        #[cfg(unix)]
        Answering::Hook(_) => panic!("no hook is configured"),
      }
    };
    assert_eq!(reasons(&received), [b"ok"]);
  });
}

/// A-724 piece 1 (§4.17.5 "What a repository costs, counted"). Do: push stock git's fixture pushes into one repository
/// while a second repository on the same shard is left alone. Expect: the pushed repository's usage row counts every
/// push begun and answered, every byte the client sent, and steps that took the shard's time, with its owner the one
/// principal named for all of them; the idle repository has no row.
#[test]
fn t_a724_a_repositorys_pushes_are_counted_on_its_own_usage_row() {
  crate::daemon::audit_on_shard(|state| {
    let pushed_to = git_repository(state, "pushed");
    let idle = git_repository(state, "idle");
    let mut sent = 0u64;
    for (scenario, _) in chain() {
      let (_, mut push) = started(state, pushed_to);
      answered(state, &mut push, scenario.request);
      sent += u64::try_from(scenario.request.len()).unwrap();
    }
    let pushes = u64::try_from(chain().len()).unwrap();
    let usage = state
      .vcs_usage
      .usage(&pushed_to)
      .expect("a row for the pushed repository");
    assert_eq!(
      (
        usage.intake.begun,
        usage.intake.answered,
        usage.intake.bytes_in
      ),
      (pushes, pushes, sent)
    );
    assert!(
      usage.intake.steps >= pushes,
      "every push takes steps: {usage:?}"
    );
    assert!(
      usage.intake.step_ns > 0,
      "the steps took the shard's time: {usage:?}"
    );
    assert_eq!(usage.fetch, slates_vcs::WorkUsage::default());
    let named: Vec<_> = usage
      .principals()
      .iter()
      .map(|slot| (slot.principal.clone(), slot.count, slot.overstated))
      .collect();
    assert_eq!(named, vec![(owner(), pushes, 0)]);
    assert!(
      state.vcs_usage.usage(&idle).is_none(),
      "the idle repository was counted"
    );
  });
}

/// Shape: the branches of [`t_a719_a_push_is_decided_a_command_a_step`]: enough that a decision made whole is told
/// from one made a command a step.
#[cfg(unix)]
const BRANCHES: usize = 40;

/// A-719 (found by use: stock git's push of git/git, 1,027 refs, ran its whole decision in one step and spun its
/// shard). Do: push stock git's `create` pack with [`BRANCHES`] creates of branches at its commit, then answer it a step
/// at a time. Expect: every branch created `ok`; and once the push is checked, at least a step a command while it is
/// decided, so no step judges and appends more than one command.
#[cfg(unix)]
#[test]
fn t_a719_a_push_is_decided_a_command_a_step() {
  crate::daemon::audit_on_shard(|state| {
    let repository = git_repository(state, "many");
    let (_, mut push) = started(state, repository);
    let names: Vec<Vec<u8>> = (0..BRANCHES)
      .map(|branch| format!("refs/heads/b{branch}").into_bytes())
      .collect();
    let commands: Vec<(&str, &str, &[u8])> = names
      .iter()
      .map(|name| ("", FIRST, name.as_slice()))
      .collect();
    let create = scenario!("create");
    let request = request(&commands, CAPABILITIES, pack_of(create.request));
    for piece in request.chunks(PIECE) {
      push.input(state, piece).expect("taken");
    }
    push.end(state).expect("ended");
    let mut answer = Vec::new();
    let mut deciding = 0;
    let received = loop {
      let was_deciding = matches!(push.answer, super::AnswerState::Deciding(_));
      match push.answer_step(state, &mut answer).expect("answered") {
        Answering::Answered(received) => break *received,
        Answering::Turned => deciding += usize::from(was_deciding),
        #[cfg(unix)]
        Answering::Hook(_) => panic!("no hook is configured"),
      }
    };
    let reasons = reasons(&received);
    assert_eq!(reasons.len(), BRANCHES);
    assert!(reasons.iter().all(|reason| reason == b"ok"), "{reasons:?}");
    // The first command is decided in the step that seals the push, the last in the step that answers it.
    assert!(
      deciding + 2 >= BRANCHES,
      "{deciding} decision steps for {BRANCHES} commands"
    );
  });
}

/// A-724's starvation fix (found by use: a push's last input step resolved every delta waiting on a base in one poll,
/// 1,435.7 ms for git/git v1.5.0 on the 32-core Studio, and the small repositories sharing its shard waited behind it).
/// Do: push stock git's `create`, then hand the driver `update`'s request (a thin pack whose deltas wait on bases in the
/// repository) whole, end it, and answer it a step at a time. Expect: once its bytes are in, the push is still resolving
/// its deltas (no input step resolved them), and its answer, made over turns, is the one git's own server gave.
#[test]
fn t_a724_a_pushs_waiting_deltas_resolve_in_its_answers_turns_never_in_an_input_step() {
  crate::daemon::audit_on_shard(|state| {
    let repository = git_repository(state, "deferred");
    let mut chained = chain().into_iter();
    let (create, _) = chained.next().unwrap();
    let (_, mut push) = started(state, repository);
    answered(state, &mut push, create.request);
    let (update, _) = chained.next().unwrap();
    let (_, mut push) = started(state, repository);
    push.input(state, update.request).expect("taken");
    push.end(state).expect("ended");
    assert!(
      push.session.is_resolving(),
      "the thin pack's deltas were resolved inside an input step"
    );
    let mut answer = Vec::new();
    let mut turns = 0;
    let received = loop {
      turns += 1;
      match push.answer_step(state, &mut answer).expect("answered") {
        Answering::Answered(received) => break received,
        Answering::Turned => {}
        #[cfg(unix)]
        step @ Answering::Hook(_) => panic!("no hook runs here: {step:?}"),
      }
    };
    assert!(turns >= 2, "resolved and checked in one step");
    assert_eq!(answer, gits_report(&update), "{}", update.name);
    assert!(reasons(&received).iter().all(|reason| reason == b"ok"));
  });
}

/// A-727 (holders made again; w-git's finding through main). Do: push the fixtures' `create` and `update` (a plain push
/// making `main`, then a thin pack moving it) and make the store again from the repository's records. Expect: the store
/// made again equal to the live one whole: its rows, `main`'s versions and every content with the holders it has, so
/// the live store holds no content the records would not, and the rebuilt none fewer.
#[test]
fn t_a727_a_plain_push_then_its_update_are_made_again_with_the_same_holders() {
  crate::daemon::audit_on_shard(|state| {
    let repository = git_repository(state, "again");
    pushed_from_the_fixtures(state, repository, &["create", "update"]);
    let main = state
      .db
      .partition()
      .repositories()
      .branch(repository, "main")
      .expect("main");
    crate::vcs_store::tests::assert_made_again_whole(
      state,
      repository,
      &[crate::vcs_store::green_key(main)],
    );
  });
}

/// A-724 piece 8c (main's ruling of 2026-11-20 (ii): a record names its names only once they are durable at `f + 1` in
/// the region). Do: with version 3 on for the shard, push stock git's `create` and `update`, driving each answer as an
/// edge does (a turn, a yield, and the names it asks to home homed in the region while the home shard publishes), then
/// make the store again from the log. Expect: both pushes answered `ok`; each record that names objects is version 3,
/// naming them by the identity the region homed; and the store made again from those records equal to the live one.
#[test]
fn t_a724_a_pushs_record_names_its_names_once_they_are_homed_in_the_region() {
  let daemon = crate::daemon::audit_daemon_of(1);
  let (sent, got) = std::sync::mpsc::channel();
  crate::daemon::observe_first(&daemon, move |state| {
    crate::vcs_store::write_version_3_for_test(true);
    let repository = git_repository(state, "homed");
    let requests: Vec<Vec<u8>> = chain()
      .into_iter()
      .take(2)
      .map(|(scenario, _)| scenario.request.to_vec())
      .collect();
    let task = slates_rt::futures::spawn(async move {
      // The home's publications, as the daemon's own commit them: every window kept before one is durable after it.
      let publisher = slates_rt::futures::spawn(async {
        for _ in 0..20_000 {
          crate::vcs_store::homes::publication_committed();
          slates_rt::futures::yield_now().await;
        }
      })
      .expect("the publisher");
      slates_rt::futures::detach(publisher).expect("detached");
      let mut outcome = Vec::new();
      for request in requests {
        outcome.push(homed_drive(repository, &request).await);
      }
      let _ = sent.send((repository, outcome));
    })
    .expect("the pushes' task");
    slates_rt::futures::detach(task).expect("detached");
  });
  let (repository, outcome) = got
    .recv_timeout(std::time::Duration::from_nanos(PUSH_WAIT_NS))
    .expect("the pushes answered within the test's bound");
  for received in &outcome {
    let received = received.as_ref().expect("answered");
    assert!(
      reasons(received).iter().all(|reason| reason == b"ok"),
      "{:?}",
      received.outcomes
    );
  }
  let (checked, sent) = std::sync::mpsc::channel();
  crate::daemon::observe_first(&daemon, move |state| {
    let records = crate::vcs_store::tests::records(state, repository);
    let versions: Vec<u32> = records
      .iter()
      .map(|record| u32::from_le_bytes(record[4..8].try_into().unwrap()))
      .collect();
    let main = state
      .db
      .partition()
      .repositories()
      .branch(repository, "main")
      .expect("main");
    crate::vcs_store::tests::assert_made_again_whole(
      state,
      repository,
      &[crate::vcs_store::green_key(main)],
    );
    let _ = checked.send(versions);
  });
  let versions = sent.recv().expect("checked");
  assert!(!versions.is_empty(), "no record");
  assert!(
    versions.iter().all(|version| *version == 3),
    "versions {versions:?}"
  );
}

/// One push of `request` driven as an edge drives it (`crate::vcs_http::git::receive`): its answer a step at a time,
/// yielding after a turn, and the names it asks to home homed in the region off the step (A-724 piece 8c).
pub(crate) async fn homed_drive(
  repository: RepositoryId,
  request: &[u8],
) -> Result<super::Received, String> {
  use crate::state::with_state;
  let mut push = with_state(|state| {
    ReceivePack::start(
      state,
      (&owner(), repository),
      false,
      &mut Vec::new(),
      Unpresented,
    )
  })
  .ok_or("no state to start")?
  .map_err(|failed| format!("not started: {failed:?}"))?;
  with_state(|state| push.input(state, request))
    .ok_or("no state for the request")?
    .map_err(|failed| format!("not taken: {failed:?}"))?;
  with_state(|state| push.end(state))
    .ok_or("no state for the end")?
    .map_err(|failed| format!("not ended: {failed:?}"))?;
  let mut answer = Vec::new();
  loop {
    let step = with_state(|state| push.answer_step(state, &mut answer))
      .ok_or("no state for a step")?
      .map_err(|failed| format!("a step failed: {failed:?}"))?;
    match step {
      Answering::Answered(received) => return Ok(*received),
      Answering::Turned => {
        slates_rt::futures::yield_now().await;
        if let Some(bytes) = push.take_homing() {
          let (origin, shards) =
            with_state(|state| (state.shard, state.shards.clone())).ok_or("no state")?;
          let homed = crate::vcs_store::homes::home_in_region(
            origin,
            &shards,
            bytes,
            crate::fleet::live_budget(),
          )
          .await;
          if homed.is_err() {
            return Err(format!("not homed: {homed:?}"));
          }
          with_state(|state| push.homed(state, homed)).ok_or("no state for the homing")?;
        }
      }
      #[cfg(unix)]
      step @ Answering::Hook(_) => return Err(format!("no hook runs here: {step:?}")),
    }
  }
}

/// A-724 piece 8c (main's ruling (ii): a replay reads a version-3 record's names locally, else from their region homes).
/// Do: on a node of four shards, with version 3 on, push stock git's `create`; drop the names from the owner's content
/// store, so only their home shard holds them for the region; make the store again from the log. Expect: the rebuild
/// reads the names from their home before the record's replay (counted fetched), and the store made again is the live
/// one.
#[test]
fn t_a724_a_rebuild_reads_a_version_3_records_names_from_their_region_home() {
  let daemon = crate::daemon::audit_daemon_of(4);
  let (sent, got) = std::sync::mpsc::channel();
  crate::daemon::observe_first(&daemon, move |state| {
    crate::vcs_store::write_version_3_for_test(true);
    let repository = git_repository(state, "fetched");
    let request = chain()
      .into_iter()
      .next()
      .map(|(scenario, _)| scenario.request.to_vec())
      .expect("create");
    let task = slates_rt::futures::spawn(async move {
      // Every shard's publications, as the daemon's own commit them: the names' home may be another shard.
      let (origin, shards) =
        crate::state::with_state(|state| (state.shard, state.shards.clone())).expect("state");
      let publisher = slates_rt::futures::spawn(async move {
        for _ in 0..2_000 {
          for shard in &shards {
            let _ = crate::xshard::call_in_shard_time(
              origin,
              *shard,
              |_| crate::vcs_store::homes::publication_committed(),
              crate::daemon::LIVENESS_BUDGET_NS,
            )
            .await;
          }
          slates_rt::futures::yield_now().await;
        }
      })
      .expect("the publisher");
      slates_rt::futures::detach(publisher).expect("detached");
      let answered = homed_drive(repository, &request).await;
      let _ = sent.send((repository, answered.map(|_| ())));
    })
    .expect("the push's task");
    slates_rt::futures::detach(task).expect("detached");
  });
  let (repository, answered) = got
    .recv_timeout(std::time::Duration::from_nanos(PUSH_WAIT_NS))
    .expect("answered within the test's bound");
  answered.expect("the push answered");
  let (checked, done) = std::sync::mpsc::channel();
  crate::daemon::observe_first(&daemon, move |state| {
    let record = crate::vcs_store::tests::records(state, repository)
      .pop()
      .expect("a record");
    let format = slates_git::object_name::ObjectFormat::Sha1;
    let (identity, _) = crate::vcs_store::record::Record::decode(&record, format)
      .expect("decoded")
      .named
      .expect("version 3");
    let home = crate::vcs_store::homes::home_of(&state.shards, &identity).expect("a home");
    let owner_shard = state.shard;
    crate::vcs_store::release_names(state, &identity);
    let main = state
      .db
      .partition()
      .repositories()
      .branch(repository, "main")
      .expect("main");
    let before = state
      .refusals
      .get(crate::vcs_store::NAMES_FETCHED)
      .copied()
      .unwrap_or(0);
    let _ = checked.send((home, owner_shard, before, main));
  });
  let (home, owner_shard, before, main) = done.recv().expect("checked");
  assert_ne!(
    home, owner_shard,
    "the names' home is the owner shard: the case reads nothing from the region"
  );
  let (made, rebuilt) = std::sync::mpsc::channel();
  crate::daemon::observe_first(&daemon, move |state| {
    crate::vcs_store::rebuild(state, repository);
    let _ = made.send(());
  });
  rebuilt.recv().expect("rebuild begun");
  let deadline = std::time::Instant::now() + std::time::Duration::from_nanos(PUSH_WAIT_NS);
  loop {
    let (polled, poll) = std::sync::mpsc::channel();
    crate::daemon::observe_first(&daemon, move |state| {
      let _ = polled.send((
        state.vcs_stores.unrecovered(repository).is_none()
          && !state.vcs_stores.rebuilding(repository),
        state
          .refusals
          .get(crate::vcs_store::NAMES_FETCHED)
          .copied()
          .unwrap_or(0),
      ));
    });
    let (ready, fetched) = poll.recv().expect("polled");
    if ready {
      assert!(
        fetched > before,
        "the names were not read from their region home"
      );
      break;
    }
    assert!(
      std::time::Instant::now() < deadline,
      "the rebuild did not end"
    );
    std::thread::yield_now();
  }
  let (whole, compared) = std::sync::mpsc::channel();
  crate::daemon::observe_first(&daemon, move |state| {
    crate::vcs_store::tests::assert_made_again_whole(
      state,
      repository,
      &[crate::vcs_store::green_key(main)],
    );
    let _ = whole.send(());
  });
  compared.recv().expect("compared");
}

/// A pack of `objects`, each whole, as stock git writes one.
fn pack_of_objects(objects: &[(slates_git::object_name::Kind, Vec<u8>)]) -> Vec<u8> {
  use slates_git::entry::{self, Header, Representation};
  let mut pack = Vec::new();
  let count = u32::try_from(objects.len()).unwrap();
  let mut writer = slates_git::pack::Writer::new(ObjectFormat::Sha1, count, &mut pack);
  let mut at = entry::PACK_HEADER_LEN;
  for (kind, content) in objects {
    let header = Header {
      representation: Representation::Whole(*kind),
      size: u64::try_from(content.len()).unwrap(),
    };
    let zlib = stored_zlib(content);
    entry::write(header, at, &mut writer).unwrap();
    slates_git::out::Out::put(&mut writer, &zlib);
    at += entry::written_len(header, at).unwrap() + u64::try_from(zlib.len()).unwrap();
  }
  writer.finish().unwrap();
  pack
}

/// A commit of a flat tree of `files` files, each saying `word` and its number, over `parent`: its id and its objects
/// (the commit, the tree, every blob).
fn flat_commit(
  files: usize,
  word: &str,
  parent: Option<ObjectId>,
) -> (ObjectId, Vec<(slates_git::object_name::Kind, Vec<u8>)>) {
  use slates_git::object_name::{Kind, name_of};
  let mut objects = Vec::new();
  let mut tree = Vec::new();
  for at in 0..files {
    let blob = format!("{word} {at}\n").into_bytes();
    let id = name_of(ObjectFormat::Sha1, Kind::Blob, &blob).unwrap();
    tree.extend_from_slice(format!("100644 f{at:05}\0").as_bytes());
    tree.extend_from_slice(id.as_bytes());
    objects.push((Kind::Blob, blob));
  }
  let tree_id = name_of(ObjectFormat::Sha1, Kind::Tree, &tree).unwrap();
  let parent = parent
    .map(|parent| format!("parent {parent}\n"))
    .unwrap_or_default();
  let commit = format!(
    "tree {tree_id}\n{parent}author A <a@example.com> 1700000000 +0000\n\
     committer A <a@example.com> 1700000000 +0000\n\n{word}\n"
  )
  .into_bytes();
  let commit_id = name_of(ObjectFormat::Sha1, Kind::Commit, &commit).unwrap();
  objects.insert(0, (Kind::Tree, tree));
  objects.insert(0, (Kind::Commit, commit));
  (commit_id, objects)
}

/// Shape: the files of a flat head in [`t_a752_a_push_over_a_flat_head_builds_its_work_a_unit_at_a_time`], as w-git
/// measured (3,500); fewer in debug, where every unit is slower, to keep the test's time.
const FLAT_FILES: usize = if cfg!(debug_assertions) { 1_500 } else { 3_500 };

/// What a push driven step by step on a daemon's shard answered, and its longest step in nanoseconds; or why the drive
/// stopped.
type Stepped = Result<(Received, u64), String>;

/// `request` pushed to `repository` by its owner by a task on the daemon's shard, a step at a time, yielding the shard
/// between every two steps as an edge's task does (each step a poll of its own, so the watchdog sees each).
fn pushed_in_steps(daemon: &crate::Daemon, repository: RepositoryId, request: &[u8]) -> Stepped {
  let (sent, got) = std::sync::mpsc::channel();
  let request = request.to_vec();
  crate::daemon::observe_first(daemon, move |_| {
    let task = slates_rt::futures::spawn(async move {
      let _ = sent.send(stepped(repository, &request).await);
    })
    .expect("the push's task");
    slates_rt::futures::detach(task).expect("the task detached");
  });
  got
    .recv_timeout(std::time::Duration::from_secs(600))
    .unwrap_or_else(|_| Err("the push did not answer within the test's bound".to_owned()))
}

/// The push of [`pushed_in_steps`], driven.
async fn stepped(repository: RepositoryId, request: &[u8]) -> Stepped {
  use crate::state::with_state;
  let mut longest = 0u64;
  let mut timed = |began: std::time::Instant| {
    longest = longest.max(u64::try_from(began.elapsed().as_nanos()).unwrap_or(u64::MAX));
  };
  let mut push = with_state(|state| {
    ReceivePack::start(
      state,
      (&owner(), repository),
      false,
      &mut Vec::new(),
      Unpresented,
    )
  })
  .ok_or("no state to start")?
  .map_err(|failed| format!("not started: {failed:?}"))?;
  for piece in request.chunks(4096) {
    let began = std::time::Instant::now();
    with_state(|state| push.input(state, piece))
      .ok_or("no state for the request")?
      .map_err(|failed| format!("not taken: {failed:?}"))?;
    timed(began);
    slates_rt::futures::yield_now().await;
  }
  with_state(|state| push.end(state))
    .ok_or("no state for the end")?
    .map_err(|failed| format!("not ended: {failed:?}"))?;
  let mut answer = Vec::new();
  let received = loop {
    let began = std::time::Instant::now();
    let step = with_state(|state| push.answer_step(state, &mut answer))
      .ok_or("no state for a step")?
      .map_err(|failed| format!("a step failed: {failed:?}"))?;
    timed(began);
    match step {
      Answering::Answered(received) => break *received,
      Answering::Turned => slates_rt::futures::yield_now().await,
      #[cfg(unix)]
      Answering::Hook(_) => return Err("no hook is configured".to_owned()),
    }
  };
  Ok((received, longest))
}

/// A-752 (main's order of 2026-11-20, w-git's measure: one command bringing 3,502 objects over a flat 3,500-file tree
/// built its work in one step, 374 to 642 ms debug, so a monorepo's push would outlast the watchdog). Do: create `main`
/// at a flat tree of [`FLAT_FILES`] files, then push a commit changing every file, answering a step at a time. Expect:
/// both `ok`; the work builds took more turns than the files they wrote (a unit at a time, before the verdicts), none
/// past its budget by more than its longest unit and the scheduler's own overrun (`push.build.past_budget` 0).
#[test]
fn t_a752_a_push_over_a_flat_head_builds_its_work_a_unit_at_a_time() {
  let daemon = crate::daemon::audit_daemon();
  let repository = crate::daemon::observe_first(&daemon, |state| git_repository(state, "flat"));
  let (first, objects) = flat_commit(FLAT_FILES, "first", None);
  let create = request(
    &[("", &first.to_string(), b"refs/heads/main")],
    CAPABILITIES,
    &pack_of_objects(&objects),
  );
  let created = pushed_in_steps(&daemon, repository, &create);
  let before = crate::daemon::observe_first(&daemon, |state| state.push_turns);
  let (second, objects) = flat_commit(FLAT_FILES, "second", Some(first));
  let update = request(
    &[(&first.to_string(), &second.to_string(), b"refs/heads/main")],
    CAPABILITIES,
    &pack_of_objects(&objects),
  );
  let updated = pushed_in_steps(&daemon, repository, &update);
  let after = crate::daemon::observe_first(&daemon, |state| state.push_turns);
  daemon.stop();
  let (created, created_longest) = created.unwrap();
  let (updated, updated_longest) = updated.unwrap();
  eprintln!(
    "A-752 flat {FLAT_FILES}: create's longest step {created_longest} ns, update's {updated_longest} ns; \
     builds before {before:?} after {after:?}"
  );
  assert_eq!(reasons(&created), [b"ok"]);
  assert_eq!(reasons(&updated), [b"ok"]);
  let turns = after.turns - before.turns;
  assert!(
    turns > FLAT_FILES as u64,
    "the update's work was built in {turns} turns for {FLAT_FILES} files"
  );
  assert_eq!(after.past_budget, 0, "{after:?}");
}
