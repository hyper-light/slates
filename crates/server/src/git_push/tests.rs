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

use super::{AGENT, Answering, HOOK_NOT_RUN, ROOT_UNPUBLISHED, ReceivePack, Unpresented};
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
  let reply = crate::test_store::create(
    state,
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
      hg_publishing: true,
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
      Answering::Turned => carry_blocking(state, push),
      #[cfg(unix)]
      step @ Answering::Hook(_) => panic!("no hook runs where no view is shown: {step:?}"),
    }
  }
}

/// `request` pushed to `repository` by its owner: what was received.
pub(crate) fn pushed(state: &mut ShardState, repository: RepositoryId, request: &[u8]) -> Received {
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

/// The files the green `repository`'s branch `name` binds holds, with their bytes: reached through the door, made first
/// if it owes its push (P2).
pub(crate) fn files(
  state: &mut ShardState,
  repository: RepositoryId,
  name: &str,
) -> Vec<(String, Vec<u8>)> {
  let green = state
    .db
    .partition()
    .repositories()
    .branch(repository, name)
    .unwrap();
  crate::green_ready::made_now(state, green);
  let engine = state.greens.get(&green).unwrap();
  engine
    .files_bytes()
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
fn assert_hold_the_second_text(state: &mut ShardState, repository: RepositoryId, names: &[&str]) {
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
  crate::test_store::audit_on_stored_shard("git-push", |state| {
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

/// A-645 P3 (main's ruling 1): with a `pre-receive` hook configured, `delete` is refused in words naming the root mount
/// before this daemon has published it, and `hook not run` once it has (nothing yet shows the hook's view); each names
/// the hook and records nothing past `held`.
fn assert_a_configured_hook_refuses(
  state: &mut ShardState,
  repository: RepositoryId,
  delete: &[u8],
  held: usize,
) {
  let received = pushed(state, repository, delete);
  assert_eq!(reasons(&received), [ROOT_UNPUBLISHED]);
  assert_first_line_names(&received, b"pre-receive");
  assert_eq!(records(state, repository), held);
  state.hook_views =
    crate::vcs_store::svn_presenter::HookViews::Given(std::path::PathBuf::from("/slates-root"));
  let received = pushed(state, repository, delete);
  assert_eq!(reasons(&received), [HOOK_NOT_RUN]);
  assert_first_line_names(&received, b"pre-receive");
  assert_eq!(records(state, repository), held);
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
  crate::test_store::audit_on_stored_shard("git-push", |state| {
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
    assert_a_configured_hook_refuses(state, repository, &delete, held);
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
  crate::test_store::audit_on_stored_shard("git-push", |state| {
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
  crate::test_store::audit_on_stored_shard("git-push", |state| {
    let repository = git_repository(state, "bindings");
    pushed_from_the_fixtures(state, repository, &["create"]);
    // Through the door, as `verbs::run_recorded` reaches a green: one that owes its push is made first (P2).
    let branch = |state: &mut ShardState, body: RequestBody| {
      if let Some(green) = crate::green_ready::owing_green_of(state, &body) {
        crate::green_ready::made_now(state, green);
      }
      dispatch(state, 1, &owner(), body)
    };
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
  crate::test_store::audit_on_stored_shard("git-push", |state| {
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

/// Format: where the fixture `update`'s pack's three entries begin (`git verify-pack -v`): its commit, its tree, then
/// its blob, a REF_DELTA whose base is outside the pack (the thin delta).
const UPDATE_ENTRIES: [usize; 3] = [12, 153, 196];

/// The fixture `update`'s request with its pack's thin delta moved first: the same objects, the delta now waiting on its
/// base before the commit and tree that follow it (a REF_DELTA names its base by id, so its place in the pack is free;
/// git's own thin packs put such deltas where the object order puts them).
fn update_with_its_delta_first() -> Vec<u8> {
  let request = scenario!("update").request;
  let at = request
    .windows(4)
    .position(|magic| magic == b"PACK")
    .unwrap();
  let pack = &request[at..];
  let end = pack.len() - ObjectFormat::Sha1.raw_len();
  let [commit, _, delta] = UPDATE_ENTRIES;
  let mut moved = pack[..commit].to_vec();
  moved.extend_from_slice(&pack[delta..end]);
  moved.extend_from_slice(&pack[commit..delta]);
  let mut hasher = slates_git::object_name::Hasher::raw(ObjectFormat::Sha1);
  hasher.update(&moved);
  moved.extend_from_slice(hasher.finish().unwrap().as_bytes());
  let mut out = request[..at].to_vec();
  out.extend_from_slice(&moved);
  out
}

/// A-722 (found by reading B2's replay: a push admits a thin pack's waiting deltas once its pack is whole, after every
/// object resolved as it was read, and its record names its objects in that order; a replay admitted them in pack
/// order and compared the two in order). Do: push the fixtures' `create`, then `update` with its thin delta first in its
/// pack; make the store again from the log. Expect: both pushes `ok`; the refs advertised after as before. Red before
/// the change: the replay refused as diverged, the second push's ref lost.
#[test]
fn t_a722_a_thin_packs_record_whose_delta_waits_first_is_replayed() {
  crate::test_store::audit_on_stored_shard("git-push", |state| {
    let repository = git_repository(state, "thin-first");
    pushed_from_the_fixtures(state, repository, &["create"]);
    let received = pushed(state, repository, &update_with_its_delta_first());
    assert!(
      reasons(&received).iter().all(|reason| reason == b"ok"),
      "{:?}",
      received.outcomes
    );
    let before = super::advertised_refs(state, repository);
    crate::vcs_store::rebuild(state, repository);
    while crate::vcs_store::rebuild_turn_once_within(state, repository, 0) {}
    crate::vcs_store::paths::whole_build::settle(state);
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
  let refused = crate::test_store::audit_on_stored_shard_configured(
    "git-push",
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

/// Shape: the files of each tree [`t_a238_a_pushs_append_meeting_the_chains_cap_checkpoints_nothing_in_its_step`] pushes:
/// enough that each create's work is a large part of [`SMALL_CHAIN_BUDGET`].
const ROOMY_FILES: usize = 64;

/// Shape: the branches it creates, each at a tree of its own: enough that their works pass the chains' cap many times.
const ROOMY_CREATES: usize = 16;

/// A-238 moved off the append's step (main's order of 2026-11-21, 04:58 PST; found by use: git/git's whole-history push
/// had its daemon killed by the watchdog, shard 10 spinning 1,142 ms in one append that met `green_chains` at its cap
/// and checkpointed the repository's every member green, about a thousand, each folding its identity whole). Do: with a
/// small green-chain budget, push [`ROOMY_CREATES`] creates of branches, each its own commit of a one-file tree, then
/// an update of each to its own tree of [`ROOMY_FILES`] files, each a push of its own (a create owes its push since P2,
/// so the updates' works are what fill the chains), so the chains fill and later appends meet their cap with many members.
/// Expect: every create `ok`; no green checkpointed inside an append's step (the counter none); the room made in the
/// pushes' turns before their appends, a green a turn (the turns' counter moved). Red before the change: the appends
/// checkpointed the members themselves, in their steps.
#[test]
fn t_a238_a_pushs_append_meeting_the_chains_cap_checkpoints_nothing_in_its_step() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored_configured(|config| {
    config.caps.green_chain_bytes = SMALL_CHAIN_BUDGET;
  });
  let repository = crate::daemon::observe_first(&daemon, |state| git_repository(state, "roomy"));
  let mut refused = Vec::new();
  for k in 0..ROOMY_CREATES {
    // A create owes its push (P2), so its update is what builds a work into its green's chain.
    let (first, objects) = flat_commit(1, &format!("create {k} file"), None);
    let (second, more) = flat_commit(ROOMY_FILES, &format!("update {k} file"), Some(first));
    let (first, second) = (first.to_string(), second.to_string());
    let name = format!("refs/heads/r{k}");
    let created = [("", first.as_str(), name.as_bytes())];
    let updated = [(first.as_str(), second.as_str(), name.as_bytes())];
    for (commands, objects) in [(created, &objects), (updated, &more)] {
      let pushing = request(&commands, CAPABILITIES, &pack_of_objects(objects));
      // A push a poll of the shard, as a client's push is its own task.
      let answered = crate::daemon::observe_first(&daemon, move |state| {
        reasons(&pushed(state, repository, &pushing))
      });
      if answered != [b"ok".to_vec()] {
        refused.push(k);
      }
    }
  }
  let (in_appends, in_turns) = crate::daemon::observe_first(&daemon, |state| {
    let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
    (
      count(crate::repository::push::APPEND_CHECKPOINTED),
      count(super::ROOM_CHECKPOINTED),
    )
  });
  daemon.stop();
  assert_eq!((refused, in_appends), (Vec::new(), 0));
  assert!(in_turns > 0, "the room made in the turns: {in_turns}");
}

/// Shape: the commits of [`t_a614a_a_long_historys_derivation_takes_turns_by_the_shards_pace_not_its_objects`]'s line,
/// three objects each: enough that a few units a turn is hundreds of turns.
const PACED_HISTORY: usize = 600;

/// Shape: the turns that line's one derivation takes besides those its work fills while nobody else waits on the shard:
/// a turn to begin it, one to end the turn its work ends in, one to find nothing due, with room for a scheduler's
/// spurious wake or two.
const PACED_TURNS: u64 = 8;

/// A-614 amended, paced (main's report of 2026-11-21, r52: a clone of git/git's branches took 465 s, its derivation of 8
/// versions 3,024,021 turns, each paying the shard's lend, its settle and a yield). Do: push a line of [`PACED_HISTORY`]
/// commits as one create, then turn the repository's derivation until it is idle, nobody else waiting on the shard.
/// Expect: the one version derived, in at most [`PACED_TURNS`] turns besides one per step's ceiling of the derivation's
/// wall time and one per wait of another task's the shard found meanwhile (`waiting_found`, the shard's own count of
/// what a paced step asks): a derivation runs while nobody waits to the step's ceiling (A-783), never a few units a
/// turn, so its turns follow its time and the shard's other work, whatever the machine's load (a loaded host's
/// ceiling-long turns do less each; main's ruling of 2026-11-21 08:20, the rule stated rather than the machine's speed).
/// Red before the change: hundreds of turns. Until 2026-11-23 the shard's own waits were left to a slack of eight: a
/// derivation slowed past a second (ThreadSanitizer's lane) met its shard's timers and beat more often than that, and
/// failed two runs of three there.
#[test]
fn t_a614a_a_long_historys_derivation_takes_turns_by_the_shards_pace_not_its_objects() {
  use crate::vcs_store::git_derive::{DERIVED, TURNS, derive_for_read};
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let count = |state: &ShardState, name: &str| state.refusals.get(name).copied().unwrap_or(0);
  let (repository, before) = crate::daemon::observe_first(&daemon, move |state| {
    let repository = git_repository(state, "paced");
    let mut objects = Vec::new();
    let mut parent = None;
    for at in 0..PACED_HISTORY {
      let (commit, made) = flat_commit(1, &format!("line {at}"), parent);
      objects.extend(made);
      parent = Some(commit);
    }
    let head = parent.unwrap().to_string();
    let created = [("", head.as_str(), b"refs/heads/main".as_slice())];
    let pack = pack_of_objects(&objects);
    let received = pushed(state, repository, &request(&created, CAPABILITIES, &pack));
    assert_eq!(reasons(&received), [b"ok".to_vec()]);
    (repository, (count(state, TURNS), count(state, DERIVED)))
  });
  // The derivation runs as a fetch's does, a task of the shard; this thread waits on a channel, not on the shard, so
  // nobody else waits there.
  let (done, finished) = std::sync::mpsc::channel();
  let ceiling_ns = crate::daemon::observe_first(&daemon, |state| {
    slates_rt::futures::step_ceiling_ns().unwrap_or_else(|| state.config.step_quantum_ns())
  });
  let waits_found =
    || slates_rt::registry::with_current(|context| context.counters().waiting_found).unwrap_or(0);
  let found_before = crate::daemon::observe_first(&daemon, move |_| waits_found());
  let began = std::time::Instant::now();
  crate::daemon::observe_first(&daemon, move |_| {
    let task = slates_rt::futures::spawn(async move {
      derive_for_read(repository).await;
      let _ = done.send(());
    })
    .unwrap();
    slates_rt::futures::detach(task).unwrap();
  });
  finished.recv().unwrap();
  let took_ns = u64::try_from(began.elapsed().as_nanos()).unwrap_or(u64::MAX);
  let found = crate::daemon::observe_first(&daemon, move |_| waits_found()) - found_before;
  let after = crate::daemon::observe_first(&daemon, move |state| {
    (count(state, TURNS), count(state, DERIVED))
  });
  daemon.stop();
  let (turns, derived) = (after.0 - before.0, after.1 - before.1);
  assert_eq!(derived, 1);
  let filled = took_ns.div_ceil(ceiling_ns.max(1));
  assert!(
    turns <= PACED_TURNS + filled + found,
    "{turns} turns over {took_ns} ns at a {ceiling_ns} ns ceiling, the shard finding others waiting {found} times"
  );
}

/// Shape: the files of [`t_p2_an_unborn_default_branchs_first_push_owes_its_tree_and_reads_as_its_commit`]'s first
/// version: enough that building it is many quanta.
const UNBORN_FILES: usize = 1_500;

/// P2 (main's ruling of 2026-11-21 08:10 PST, `BranchChange::UpdateDeferred`; found by use: git/git's unborn master,
/// 4,876 files, built whole in one step at its push). Do: push the unborn default branch `main` at a commit of a flat
/// tree of [`UNBORN_FILES`] files, a step at a time; then reach its green through the door (`green_ready`) and wait for
/// it to be made. Expect: the push `ok`, its build taking at most the one turn its command takes to name the owed
/// push, its green owing the push (an update deferred, counted); once reached, the green made once, holding every file
/// of the commit with its bytes, as the eager update made it.
/// Red before the change: the push built the whole tree in its steps (14,273 quanta) and the green held it at once.
#[test]
fn t_p2_an_unborn_default_branchs_first_push_owes_its_tree_and_reads_as_its_commit() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (first, objects) = flat_commit(UNBORN_FILES, "unborn", None);
  let (repository, green, before) = crate::daemon::observe_first(&daemon, |state| {
    let repository = git_repository(state, "unborn");
    let green = state
      .db
      .partition()
      .repositories()
      .branch(repository, "main")
      .unwrap();
    (repository, green, state.push_turns)
  });
  let create = request(
    &[("", &first.to_string(), b"refs/heads/main")],
    CAPABILITIES,
    &pack_of_objects(&objects),
  );
  let (created, _) = pushed_in_steps(&daemon, repository, &create).unwrap();
  let (after, owing, deferred) = crate::daemon::observe_first(&daemon, move |state| {
    (
      state.push_turns,
      state.greens.is_owing(&green),
      state
        .refusals
        .get("repository.push.updates_deferred")
        .copied()
        .unwrap_or(0),
    )
  });
  crate::daemon::observe_first(&daemon, move |state| {
    crate::green_ready::green_ready(state, green);
  });
  let began = std::time::Instant::now();
  while crate::daemon::observe_first(&daemon, move |state| state.greens.is_owing(&green))
    && began.elapsed() < std::time::Duration::from_secs(120)
  {}
  let materialized = crate::daemon::observe_first(&daemon, move |state| {
    state
      .refusals
      .get(crate::green_ready::MATERIALIZED)
      .copied()
      .unwrap_or(0)
  });
  let read = crate::daemon::observe_first(&daemon, move |state| files(state, repository, "main"));
  daemon.stop();
  assert_eq!(reasons(&created), [b"ok"]);
  let quanta = (after.more_quanta - before.more_quanta) + (after.turns - before.turns);
  assert_eq!(
    (quanta <= 1, owing, deferred),
    (true, true, 1),
    "the push built nothing, its one turn the create's naming its owed push; its green owes the push ({quanta} quanta)"
  );
  let expected: Vec<(String, Vec<u8>)> = (0..UNBORN_FILES)
    .map(|at| (format!("f{at:05}"), format!("unborn {at}\n").into_bytes()))
    .collect();
  assert_eq!(materialized, 1, "made once, through the door");
  assert_eq!(read, expected, "made as the eager update made it");
}

/// `green`'s owed push as its record encodes it, and its head: what a replay of it must hold again (P2).
fn owed_of(green: &slates_merge::engine::Green) -> (u64, Option<Vec<u8>>) {
  (
    green.head(),
    green
      .deferred()
      .map(slates_merge::push::DeferredPush::encode),
  )
}

/// P2's oracle (main's order of 2026-11-21 08:10 PST: the deferred green, materialized, equals the eager one byte for
/// byte, before and after a replay, per branch and tag). Do: push the fixtures' `create` (the unborn default `main`, an
/// update deferred), `update`, `tag` (`refs/tags/v1`, a create deferred, its owed push the change from `main`'s first
/// version) and `copy` (`refs/heads/copy`, a create deferred at the commit `main` holds); for each created ref, replay
/// its green from its durable chain, make the live green through the door, then replay it again. Expect: each created
/// green owes its push after the pushes, at its base, version 0; each replay holds the live green's debt byte for byte
/// (the same head and the same owed push's record, version 1), before the live green was made and after; made, the live
/// green holds the second commit's text, as the eager create's did, at version 1. The made content follows from the base
/// and the push alone (one builder over one input, A-812), so a replay owing the same push at the same base is made the
/// same; making a recovered owing green over a restarted node's shares is the fleet test GAPS owes (A-812 (b)).
#[test]
fn t_p2_a_created_branch_and_tag_owe_one_push_across_a_replay_and_are_made_as_the_eager_create() {
  crate::test_store::audit_on_stored_shard("git-push", |state| {
    let repository = git_repository(state, "p2-oracle");
    pushed_from_the_fixtures(state, repository, &["create", "update", "tag", "copy"]);
    let second = vec![("text".to_owned(), text(true))];
    for name in ["refs/tags/v1", "copy"] {
      let green = state
        .db
        .partition()
        .repositories()
        .branch(repository, name)
        .unwrap();
      let live = owed_of(&state.greens.lend(&green).expect("the green"));
      assert_eq!(
        (live.0, live.1.is_some()),
        (0, true),
        "{name}: owes its push at its base"
      );
      let before = crate::verbs::replayed_green_for_test(state, green).unwrap();
      assert_eq!(owed_of(&before), live, "{name}: replayed before it is made");
      assert_eq!(
        before
          .deferred()
          .map(slates_merge::push::DeferredPush::version),
        Some(1),
        "{name}: version 1 owed"
      );
      crate::green_ready::made_now(state, green);
      let engine = state.greens.get(&green).expect("made");
      let files: Vec<(String, Vec<u8>)> = engine
        .files_bytes()
        .map(|(path, bytes)| (path, bytes.to_vec()))
        .collect();
      assert_eq!(
        (engine.head(), files),
        (1, second.clone()),
        "{name}: the eager create's"
      );
      drop(engine);
      let after = crate::verbs::replayed_green_for_test(state, green).unwrap();
      assert_eq!(owed_of(&after), live, "{name}: replayed after it is made");
    }
  });
}

/// Shape: the pushes [`t_a722_p4e_a_stage_a_landing_overtook_is_made_again_in_turns_before_its_append`] answers at
/// once, a step of each in turn.
#[cfg(unix)]
const INTERLEAVED: usize = 12;

/// A-722 part 4e (the answer's longest step at 100 concurrent pushes to one repository: a commit staging again whole a
/// push whose stage another's landing overtook). Do: start [`INTERLEAVED`] pushes to one repository, each creating its
/// own branch at the fixtures' first commit with its whole pack; hand each its request; answer them a step of each in
/// turn. Expect: every push `ok`, and no commit staged a push again whole (the store's `restaged` count none): each
/// stage the first landing overtook was made again in its push's turns before its append.
#[cfg(unix)]
#[test]
fn t_a722_p4e_a_stage_a_landing_overtook_is_made_again_in_turns_before_its_append() {
  let (outcomes, restaged) = crate::test_store::audit_on_stored_shard("git-push", |state| {
    let repository = git_repository(state, "overtaken");
    let pack = pack_of(scenario!("create").request);
    let mut pushes: Vec<Option<ReceivePack>> = (0..INTERLEAVED)
      .map(|k| {
        let name = format!("refs/heads/p{k}");
        let created = [("", FIRST, name.as_bytes())];
        let body = request(&created, CAPABILITIES, pack);
        let (_, mut push) = started(state, repository);
        for piece in body.chunks(PIECE) {
          push.input(state, piece).expect("taken");
        }
        push.end(state).expect("ended");
        Some(push)
      })
      .collect();
    let mut outcomes = vec![Vec::new(); INTERLEAVED];
    while pushes.iter().any(Option::is_some) {
      for (at, slot) in pushes.iter_mut().enumerate() {
        let Some(push) = slot.as_mut() else {
          continue;
        };
        if let Answering::Answered(received) =
          push.answer_step(state, &mut Vec::new()).expect("answered")
        {
          outcomes[at] = reasons(&received);
          *slot = None;
        } else {
          carry_blocking(state, push);
        }
      }
    }
    let restaged = state
      .vcs_stores
      .git_ref(repository)
      .map(|(_, git)| git.repository.counters().restaged);
    (outcomes, restaged)
  });
  assert!(
    outcomes.iter().all(|reasons| reasons == &[b"ok".to_vec()]),
    "{outcomes:?}"
  );
  assert_eq!(restaged, Some(0));
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

/// What a push driven with its hooks answered: what the driver wrote, what it received, and how
/// many views the presenter showed and withdrew; or why the drive stopped.
#[cfg(unix)]
type Driven = Result<(Vec<u8>, Received, (u32, u32)), String>;

/// What the driver of a push with its hooks tells its test as it goes: each step it took or hook it ran, then the end.
/// The test's deadline is derived from these (the push's own answer): a test's bound for each, so a push of more steps
/// is waited for longer and one that stops is given up on one bound after its last.
#[cfg(unix)]
enum Progress {
  /// A step of the push taken, or one of its hooks run.
  Went,
  /// The push answered, or its drive stopped.
  Done(Box<Driven>),
}

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
  // Each step or hook within a test's bound (`git_hooks::tests::TEST_BOUND_NS`, ten liveness budgets) of the last: the
  // deadline the push's own steps make, never a fixed whole.
  let bound = std::time::Duration::from_nanos(crate::git_hooks::tests::TEST_BOUND_NS);
  let mut went = 0u64;
  loop {
    match got.recv_timeout(bound) {
      Ok(Progress::Went) => went += 1,
      Ok(Progress::Done(driven)) => return *driven,
      Err(_) => {
        return Err(format!(
          "the push went no further within the test's bound after {went} steps"
        ));
      }
    }
  }
}

/// Drives the push of [`pushed_with_hooks`], telling `sent` of each step, then what it answered.
#[cfg(unix)]
async fn drive(
  repository: RepositoryId,
  request: Vec<u8>,
  sent: std::sync::mpsc::Sender<Progress>,
) {
  let driven = driven(repository, &request, &sent).await;
  let _ = sent.send(Progress::Done(Box::new(driven)));
}

/// The push of [`pushed_with_hooks`], driven, `progress` told of each step and hook.
#[cfg(unix)]
async fn driven(
  repository: RepositoryId,
  request: &[u8],
  progress: &std::sync::mpsc::Sender<Progress>,
) -> Driven {
  let went = || {
    let _ = progress.send(Progress::Went);
  };
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
    went();
  }
  with_state(|state| push.end(state))
    .ok_or("no state for the end")?
    .map_err(|failed| format!("not ended: {failed:?}"))?;
  let mut answer = Vec::new();
  let received = loop {
    let step = with_state(|state| push.answer_step(state, &mut answer))
      .ok_or("no state for a step")?
      .map_err(|failed| format!("a step failed: {failed:?}"))?;
    went();
    match step {
      Answering::Answered(received) => break *received,
      Answering::Turned => {
        slates_rt::futures::yield_now().await;
        carry_awaited(&mut push).await?;
      }
      Answering::Hook(call) => {
        let ran = call.run(std::future::pending()).await;
        with_state(|state| push.hook_ran(state, ran, &mut answer))
          .ok_or("no state for the hook's answer")?
          .map_err(|failed| format!("its answer not taken: {failed:?}"))?;
        went();
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
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
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
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  crate::test_store::keep_landed(&daemon);
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
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
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
pub(crate) fn large_create(bytes: usize) -> (Vec<u8>, ObjectId) {
  let blob: Vec<u8> = (0..bytes)
    .map(|at| u8::try_from(at % 251).unwrap_or(u8::MAX))
    .collect();
  create_of_file(blob)
}

/// A create of `main` at a root commit whose one file holds `blob`, as stock git posts it, and the file's name.
pub(crate) fn create_of_file(blob: Vec<u8>) -> (Vec<u8>, ObjectId) {
  use slates_git::entry::{self, Header, Representation};
  use slates_git::object_name::{Kind, name_of};
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
  crate::test_store::audit_on_stored_shard("git-push", |state| {
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
        Answering::Turned => carry_blocking(state, &mut push),
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
  crate::test_store::audit_on_stored_shard("git-push", |state| {
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
  crate::test_store::audit_on_stored_shard("git-push", |state| {
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
        Answering::Turned => {
          deciding += usize::from(was_deciding);
          carry_blocking(state, &mut push);
        }
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
  crate::test_store::audit_on_stored_shard("git-push", |state| {
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
        Answering::Turned => carry_blocking(state, &mut push),
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
  crate::test_store::audit_on_stored_shard("git-push", |state| {
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

/// The edge's carry as a test's synchronous driver stands in for it (C5 part 2b piece 2: a push whose homing no edge
/// takes is refused): its names and pack kept for the region on this shard as their home keeps them
/// (`crate::vcs_store::homes::keep_for_region`, the home's own step; a test's daemon is one node, and the crossing to
/// other homes is held by the async drivers' tests, [`homed_drive`]), and its changeset landed on the node's store for
/// real, from a thread of its own (`crate::test_store::land_answered_blocking`). Nothing to do where the push asked
/// nothing.
pub(crate) fn carry_blocking<P>(state: &mut ShardState, push: &mut ReceivePack<P>) {
  // The shard's materializations run between the push's turns, as their tasks would (P2's door): a synchronous driver
  // holds the shard, so a push waiting at the door for a green would otherwise wait forever.
  crate::green_ready::make_building_now(state);
  let Some(asked) = push.take_homing() else {
    return;
  };
  let carried = carried_blocking(state, asked);
  push.homed(state, carried);
}

/// `carry` carried as [`carry_blocking`] stands in for the edge: content kept for the region on this shard as its home
/// keeps it, a changeset landed on the node's store from a thread of its own. What a git commit the server makes is
/// carried by in a test's synchronous driver too (`crate::vcs_store::commits::made_after_now`).
pub(crate) fn carried_blocking(state: &mut ShardState, carry: super::Carry) -> super::Carried {
  match carry {
    super::Carry::Home(bytes, owner) => {
      let outcome = crate::vcs_store::homes::keep_for_region(state, &bytes, &[owner])
        .map(|(identity, _)| identity);
      super::Carried::Homed(crate::vcs_store::homes::Homed { outcome, bytes })
    }
    super::Carry::Land(landing) => {
      let super::Landing {
        disk,
        header,
        parts,
      } = *landing;
      super::Carried::Landed(crate::test_store::land_answered_blocking(
        disk, header, parts,
      ))
    }
  }
}

/// The edge's carry as a test's async driver runs it ([`super::carry`]): the homing or the landing the push asked,
/// handed back. Refused only where the shard's state is not lent.
pub(crate) async fn carry_awaited<P>(push: &mut ReceivePack<P>) -> Result<(), String> {
  use crate::state::with_state;
  if let Some(asked) = push.take_homing() {
    let (origin, shards) =
      with_state(|state| (state.shard, state.shards.clone())).ok_or("no state to carry")?;
    let carried = super::carry(origin, &shards, asked).await;
    with_state(|state| push.homed(state, carried)).ok_or("no state for the carried")?;
  }
  Ok(())
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
        // As the edges do (goal 93 item 2b(b), push reader (b)): the window a step met let go, brought back.
        crate::vcs_store::upload::brought_back_waited(repository).await;
        if let Some(asked) = push.take_homing() {
          let (origin, shards) =
            with_state(|state| (state.shard, state.shards.clone())).ok_or("no state")?;
          let carried = crate::git_push::carry(origin, &shards, asked).await;
          match &carried {
            crate::git_push::Carried::Homed(homed) if homed.outcome.is_err() => {
              return Err(format!("not homed: {:?}", homed.outcome));
            }
            crate::git_push::Carried::Landed(Err(refused)) => {
              return Err(format!("not landed: {refused:?}"));
            }
            _ => {}
          }
          with_state(|state| push.homed(state, carried)).ok_or("no state for the homing")?;
        }
      }
      #[cfg(unix)]
      step @ Answering::Hook(_) => return Err(format!("no hook runs here: {step:?}")),
    }
  }
}

/// C5 part 2b (main's ruling of 2026-11-21 00:50: a homing that falls short refuses the push, never a record naming its
/// objects inline). Do: on a store-backed shard, push stock git's `create`, driving its answer as an edge does but
/// handing its names' homing back fallen short (no region home answered). Expect: the push refused `Unlanded`, and the
/// repository's register holding no record. Red before part 2b: answered `ok`, its record version 2.
#[test]
fn t_a767_c5b_a_push_whose_homing_falls_short_is_refused_and_records_nothing() {
  let (_store, daemon) = crate::test_store::audit_daemon_of_stored(1);
  let (sent, got) = std::sync::mpsc::channel();
  crate::daemon::observe_first(&daemon, move |state| {
    let repository = git_repository(state, "short");
    let request = chain()
      .into_iter()
      .next()
      .map(|(scenario, _)| scenario.request.to_vec())
      .expect("create");
    let task = slates_rt::futures::spawn(async move {
      let outcome = short_homing_drive(repository, &request).await;
      let records =
        crate::state::with_state(|state| crate::vcs_store::tests::records(state, repository).len());
      let _ = sent.send((outcome, records));
    })
    .expect("the push's task");
    slates_rt::futures::detach(task).expect("detached");
  });
  let (outcome, records) = got.recv().expect("the push ended");
  assert_eq!(outcome, Err("Unlanded".to_owned()));
  assert_eq!(records, Some(0), "nothing of the push in the register");
}

/// [`homed_drive`] whose first homing is handed back fallen short, as when no region home answers: the answer's
/// refusal (its kind), or `Ok` with the report's reasons.
async fn short_homing_drive(repository: RepositoryId, request: &[u8]) -> Result<Vec<u8>, String> {
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
  .ok_or("no state")?
  .map_err(|failed| format!("{failed:?}"))?;
  with_state(|state| push.input(state, request))
    .ok_or("no state")?
    .map_err(|failed| format!("{failed:?}"))?;
  with_state(|state| push.end(state))
    .ok_or("no state")?
    .map_err(|failed| format!("{failed:?}"))?;
  let mut answer = Vec::new();
  loop {
    let step = with_state(|state| push.answer_step(state, &mut answer))
      .ok_or("no state")?
      .map_err(|failed| format!("{failed:?}"))?;
    match step {
      Answering::Answered(_) => return Ok(answer),
      Answering::Turned => {
        slates_rt::futures::yield_now().await;
        if let Some(crate::git_push::Carry::Home(bytes, _)) = push.take_homing() {
          let fallen = crate::git_push::Carried::Homed(crate::vcs_store::homes::Homed {
            outcome: Err(crate::vcs_store::homes::HomeRefused::Unanswered),
            bytes,
          });
          with_state(|state| push.homed(state, fallen)).ok_or("no state")?;
        }
      }
      #[cfg(unix)]
      step @ Answering::Hook(_) => return Err(format!("no hook runs here: {step:?}")),
    }
  }
}

/// Shape: the files of [`t_goal93_a_greens_owed_push_is_built_from_its_packs_windows_let_go`]'s commit.
const LET_GO_FILES: usize = 200;

/// Goal 93 item 2b, reader (a) (main 01a20182 by w-git, 2026-11-22: once a push's windows were let go, its green's owed
/// push was refused building `Object { path: "f0", .., why: Malformed }`, its parked requests `ContentUnavailable`: the
/// build read its trees and blobs through `views`, whose stored read answers none on a let-go window). Do: push the
/// unborn default branch `main` at a commit of a flat tree of [`LET_GO_FILES`] files, a step at a time; let every window
/// of its staged pack go as landed; then reach its green through the door (`green_ready`) and wait for it to be made.
/// Expect: the windows let go; the green made once, holding every file with its bytes, each window it read brought back
/// from the node's store. Failing first: the build refused `Malformed`, the green never made.
#[test]
fn t_goal93_a_greens_owed_push_is_built_from_its_packs_windows_let_go() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (first, objects) = flat_commit(LET_GO_FILES, "away", None);
  let (repository, green) = crate::daemon::observe_first(&daemon, |state| {
    let repository = git_repository(state, "owed-away");
    let green = state
      .db
      .partition()
      .repositories()
      .branch(repository, "main")
      .unwrap();
    (repository, green)
  });
  let pack = pack_of_objects(&objects);
  let create = request(
    &[("", &first.to_string(), b"refs/heads/main")],
    CAPABILITIES,
    &pack,
  );
  let (created, _) = pushed_in_steps(&daemon, repository, &create).unwrap();
  let let_go = crate::daemon::observe_first(&daemon, move |state| {
    crate::vcs_store::within_budget(state, 0, |stores| {
      let Some(content) = stores.content_mut() else {
        return 0;
      };
      let window = content.window();
      pack
        .chunks(window)
        .map(slates_vcs::ContentId::of)
        .filter(|id| {
          if content.mark_landed(id) {
            let _ = content.let_go_landed(id, None);
          }
          content.is_away(id)
        })
        .count()
    })
  });
  crate::daemon::observe_first(&daemon, move |state| {
    crate::green_ready::green_ready(state, green);
  });
  let began = std::time::Instant::now();
  while crate::daemon::observe_first(&daemon, move |state| {
    state.greens.is_owing(&green) && crate::green_ready::is_building(state, green)
  }) && began.elapsed() < std::time::Duration::from_secs(120)
  {}
  let (materialized, owing) = crate::daemon::observe_first(&daemon, move |state| {
    (
      state
        .refusals
        .get(crate::green_ready::MATERIALIZED)
        .copied()
        .unwrap_or(0),
      state.greens.is_owing(&green),
    )
  });
  let read = crate::daemon::observe_first(&daemon, move |state| files(state, repository, "main"));
  daemon.stop();
  assert_eq!(reasons(&created), [b"ok"]);
  assert!(let_go > 0, "the pack's windows let go");
  assert_eq!(
    (materialized, owing),
    (1, false),
    "made once from windows let go"
  );
  let expected: Vec<(String, Vec<u8>)> = (0..LET_GO_FILES)
    .map(|at| (format!("f{at:05}"), format!("away {at}\n").into_bytes()))
    .collect();
  assert_eq!(read, expected, "every file with its bytes");
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

/// An annotated tag object named `name` of the object `target` of `kind`: its id and its bytes as git writes them.
fn tag_object(name: &str, (target, kind): (ObjectId, &str)) -> (ObjectId, Vec<u8>) {
  use slates_git::object_name::{Kind, name_of};
  let bytes = format!(
    "object {target}\ntype {kind}\ntag {name}\ntagger A <a@example.com> 1700000000 +0000\n\n{name}\n"
  )
  .into_bytes();
  (
    name_of(ObjectFormat::Sha1, Kind::Tag, &bytes).unwrap(),
    bytes,
  )
}

/// Main's order of 2026-11-21 (w-interop's A/B of record, r16: git/git's push refused every ref whose target is
/// neither a commit nor a tree, `junio-gpg-pub`, a blob, and `v1.0rc1` to `v1.0rc6`, so its clones were short of
/// them). Git lets a ref name any object, and stock `git daemon` takes them. Do: push `main` at a commit, then in one
/// push `refs/tags/junio-gpg-pub` naming a blob, `refs/tags/signed-blob` naming an annotated tag of that blob, and
/// `refs/tags/rc` naming an annotated tag of an annotated tag of the commit; then read the refs advertised, list the tags
/// with `peel` and fetch the three. Expect: every command `ok`; each ref advertised at the object it names; `ls-refs`
/// naming each tag peeled through every tag it names (the signed blob's to the blob, `rc` to the commit) and none for the
/// bare blob; the fetch's pack holding the blob, both tags, the inner tag and the commit. Red before the change: each
/// refused `ref names no tree` ("names neither a commit nor a tree").
#[test]
fn t_a836_refs_naming_a_blob_and_tags_of_tags_are_taken_and_advertised() {
  use slates_git::object_name::{Kind, name_of};
  crate::test_store::audit_on_stored_shard("git-push", |state| {
    let repository = git_repository(state, "targets");
    let (first, objects) = flat_commit(1, "first", None);
    let created = [("", first.to_string(), b"refs/heads/main".to_vec())];
    let created: Vec<(&str, &str, &[u8])> = created
      .iter()
      .map(|(old, new, name)| (*old, new.as_str(), name.as_slice()))
      .collect();
    let received = pushed(
      state,
      repository,
      &request(&created, CAPABILITIES, &pack_of_objects(&objects)),
    );
    assert_eq!(reasons(&received), [b"ok"]);
    let key = b"-----BEGIN PGP PUBLIC KEY BLOCK-----\n".to_vec();
    let blob = name_of(ObjectFormat::Sha1, Kind::Blob, &key).unwrap();
    let (signed, signed_bytes) = tag_object("signed-blob", (blob, "blob"));
    let (inner, inner_bytes) = tag_object("rc-inner", (first, "commit"));
    let (rc, rc_bytes) = tag_object("rc", (inner, "tag"));
    let objects = vec![
      (Kind::Blob, key),
      (Kind::Tag, signed_bytes),
      (Kind::Tag, inner_bytes),
      (Kind::Tag, rc_bytes),
    ];
    let named = [
      (blob, b"refs/tags/junio-gpg-pub".to_vec()),
      (signed, b"refs/tags/signed-blob".to_vec()),
      (rc, b"refs/tags/rc".to_vec()),
    ];
    let ids: Vec<String> = named.iter().map(|(id, _)| id.to_string()).collect();
    let commands: Vec<(&str, &str, &[u8])> = named
      .iter()
      .zip(&ids)
      .map(|((_, name), id)| ("", id.as_str(), name.as_slice()))
      .collect();
    let received = pushed(
      state,
      repository,
      &request(&commands, CAPABILITIES, &pack_of_objects(&objects)),
    );
    let advertised = super::advertised_refs(state, repository);
    assert_eq!(
      reasons(&received),
      [b"ok", b"ok", b"ok"],
      "{:?}",
      received.outcomes
    );
    for (id, name) in &named {
      assert!(
        advertised.contains(&(*id, name.clone())),
        "{} not advertised at {id}: {advertised:?}",
        String::from_utf8_lossy(name)
      );
    }
    // Served as git serves them: `ls-refs` with `peel` names each tag's object peeled through every tag it names, and a
    // fetch wanting the three answers a pack holding them and everything they reach.
    let lines = tags_listed_peeled(state, repository);
    for expected in [
      format!("{blob} refs/tags/junio-gpg-pub"),
      format!("{signed} refs/tags/signed-blob peeled:{blob}"),
      format!("{rc} refs/tags/rc peeled:{first}"),
    ] {
      assert!(
        lines.contains(&expected),
        "{expected} not listed: {lines:?}"
      );
    }
    let ids = fetched_ids(state, repository, &[blob, signed, rc]);
    for id in [blob, signed, rc, inner, first] {
      assert!(ids.contains(&id), "{id} not fetched: {ids:?}");
    }
  });
}

/// Shape: the commits, each tagged, of [`t_goal93_a_whole_clone_of_a_line_with_an_annotated_tag_of_every_commit_is_whole`]:
/// enough tags that their versions' runs overlap many times, as git/git's 1,012 tags on master's history do.
const TAGGED_LINE: usize = 40;

/// A commit of one file, `notes`, holding the first `at + 1` lines of a long text, over `parent`: a line of these is a
/// history git and the encoder store as deltas of one another, as git/git's is. Its id and its objects.
fn growing_commit(
  at: usize,
  parent: Option<ObjectId>,
) -> (ObjectId, Vec<(slates_git::object_name::Kind, Vec<u8>)>) {
  use slates_git::object_name::{Kind, name_of};
  let blob: Vec<u8> = (0..=at)
    .flat_map(|line| {
      format!("line {line:04} of the notes, long enough that a delta of the file is far smaller than the file\n")
        .into_bytes()
    })
    .collect();
  let blob_id = name_of(ObjectFormat::Sha1, Kind::Blob, &blob).unwrap();
  let mut tree = b"100644 notes\0".to_vec();
  tree.extend_from_slice(blob_id.as_bytes());
  let tree_id = name_of(ObjectFormat::Sha1, Kind::Tree, &tree).unwrap();
  let parent = parent
    .map(|parent| format!("parent {parent}\n"))
    .unwrap_or_default();
  let commit = format!(
    "tree {tree_id}\n{parent}author A <a@example.com> 1700000000 +0000\n\
     committer A <a@example.com> 1700000000 +0000\n\nnotes {at}\n"
  )
  .into_bytes();
  let commit_id = name_of(ObjectFormat::Sha1, Kind::Commit, &commit).unwrap();
  (
    commit_id,
    vec![
      (Kind::Commit, commit),
      (Kind::Tree, tree),
      (Kind::Blob, blob),
    ],
  )
}

/// Shape: the most turns [`t_goal93_a_whole_clone_of_a_line_with_an_annotated_tag_of_every_commit_is_whole`]'s encoding
/// may take before the test calls it unending: far past its versions' work done once.
const ENCODE_TURNS_BOUND: usize = 1 << 16;

/// Goal 93's whole-clone abort, its shape at a unit's scale (w-sessionlog's red by use: a whole clone of git/git pushed
/// to slates is aborted "possible repository corruption on the remote side" once master's annotated tags are pushed with
/// it; master alone, or master and one tag, clones whole). Do: push a line of [`TAGGED_LINE`] commits of a growing file
/// (deltas of one another) as `main` with an annotated tag of each commit, all in one push; fetch every ref's object as
/// `git clone` asks, `done`, first with nothing encoded, then once the repository's encoding is idle (from the versions'
/// kept fragments, A-718). Expect: the push `ok` for every ref; each fetch answered with no refusal, its pack whole and
/// holding every object pushed, each once. Not red at this scale (the abort needs git/git's, w-sessionlog bisecting):
/// kept as the shape's guard. It prints the encoding's turns, which grow with tags times history (each tag's first
/// version is encoded over everything it reaches): 41 versions, 11,733 turns here.
#[test]
fn t_goal93_a_whole_clone_of_a_line_with_an_annotated_tag_of_every_commit_is_whole() {
  use slates_git::object_name::Kind;
  crate::test_store::audit_on_stored_shard("git-push", |state| {
    let repository = git_repository(state, "tagged-line");
    let mut objects = Vec::new();
    let mut commits = Vec::new();
    let mut parent = None;
    for at in 0..TAGGED_LINE {
      let (commit, made) = growing_commit(at, parent);
      objects.extend(made);
      commits.push(commit);
      parent = Some(commit);
    }
    let mut named: Vec<(ObjectId, Vec<u8>)> =
      vec![(*commits.last().unwrap(), b"refs/heads/main".to_vec())];
    for (at, commit) in commits.iter().enumerate() {
      let name = format!("v{at}");
      let (tag, bytes) = tag_object(&name, (*commit, "commit"));
      objects.push((Kind::Tag, bytes));
      named.push((tag, format!("refs/tags/{name}").into_bytes()));
    }
    let ids: Vec<String> = named.iter().map(|(id, _)| id.to_string()).collect();
    let commands: Vec<(&str, &str, &[u8])> = named
      .iter()
      .zip(&ids)
      .map(|((_, name), id)| ("", id.as_str(), name.as_slice()))
      .collect();
    let received = pushed(
      state,
      repository,
      &request(&commands, CAPABILITIES, &pack_of_objects(&objects)),
    );
    assert!(
      reasons(&received).iter().all(|reason| reason == b"ok"),
      "{:?}",
      received.outcomes
    );
    use crate::vcs_store::upload::tests::{answered, fetch, pack_of};
    let wants: Vec<ObjectId> = named.iter().map(|(id, _)| *id).collect();
    let pushed_ids: std::collections::BTreeSet<ObjectId> = objects
      .iter()
      .map(|(kind, bytes)| {
        slates_git::object_name::name_of(ObjectFormat::Sha1, *kind, bytes).unwrap()
      })
      .collect();
    // Fetched as a fresh repository answers (nothing encoded yet), then from the versions' kept fragments (A-718), as a
    // daemon serves a clone once its encoding has run.
    for round in ["before the encoding", "from the kept fragments"] {
      if round == "from the kept fragments" {
        let mut turns = 0usize;
        while crate::vcs_store::git_encode::turn(state, repository)
          != crate::vcs_store::git_encode::Turned::Idle
        {
          turns += 1;
          assert!(turns < ENCODE_TURNS_BOUND, "an encoding that never ended");
        }
        let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
        eprintln!(
          "goal 93 tagged line: {} versions encoded in {turns} turns",
          count(crate::vcs_store::git_encode::ENCODED)
        );
      }
      let fetched = answered(
        state,
        (repository, &owner()),
        slates_git::upload::Asked::Request(&fetch(&wants, &[])),
      );
      assert_eq!(fetched.1, None, "the clone's fetch refused, {round}");
      let unpacked = slates_git::unpack::read_whole(
        &pack_of(&fetched.0),
        ObjectFormat::Sha1,
        &slates_git::unpack::NoIndex,
        &mut slates_git::Admission::new(u64::MAX),
      )
      .unwrap_or_else(|refused| panic!("not a whole pack {round}: {refused:?}"));
      let fetched_ids: std::collections::BTreeSet<ObjectId> =
        unpacked.objects.iter().map(|object| object.id).collect();
      assert_eq!(
        (unpacked.objects.len(), &fetched_ids),
        (pushed_ids.len(), &pushed_ids),
        "the clone holds every object pushed, each once, {round}"
      );
    }
  });
}

/// Goal 93's whole-clone abort (w-sessionlog's reduction, 2026-11-21 16:15: a clone of git/git with 506 tags or more
/// stops waiting for the repository's encoding, `encode_for_fetch`'s bounded wait, and is answered while the encoding is
/// under way; its pack ends `aborting due to possible repository corruption on the remote side`, every clone after it
/// too). Do: push the tagged line of [`t_goal93_a_whole_clone_of_a_line_with_an_annotated_tag_of_every_commit_is_whole`];
/// turn its encoding a few turns at a time, and between each few fetch every ref's object, as a clone that stopped
/// waiting does, until the encoding is idle. Expect: every fetch answered with no refusal, its pack whole and holding
/// every object pushed, each once, whatever the encoding had kept so far. Not red at this scale, even fetched after every
/// turn (the abort needs git/git's: objects past a window and a first version that never finishes encoding); kept as the
/// guard of a fetch answered mid-encoding, and the HTTP edge now counts and logs a refused answer's cause.
#[test]
fn t_goal93_a_clone_answered_while_the_encoding_is_under_way_is_whole() {
  use slates_git::object_name::Kind;
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let mut objects = Vec::new();
  let mut commits = Vec::new();
  let mut parent = None;
  for at in 0..TAGGED_LINE {
    let (commit, made) = growing_commit(at, parent);
    objects.extend(made);
    commits.push(commit);
    parent = Some(commit);
  }
  let mut named: Vec<(ObjectId, Vec<u8>)> =
    vec![(*commits.last().unwrap(), b"refs/heads/main".to_vec())];
  for (at, commit) in commits.iter().enumerate() {
    let name = format!("v{at}");
    let (tag, bytes) = tag_object(&name, (*commit, "commit"));
    objects.push((Kind::Tag, bytes));
    named.push((tag, format!("refs/tags/{name}").into_bytes()));
  }
  let ids: Vec<String> = named.iter().map(|(id, _)| id.to_string()).collect();
  let commands: Vec<(&str, &str, &[u8])> = named
    .iter()
    .zip(&ids)
    .map(|((_, name), id)| ("", id.as_str(), name.as_slice()))
    .collect();
  let pushing = request(&commands, CAPABILITIES, &pack_of_objects(&objects));
  // The push a poll of its own, as each round below is: the shard steps between them, as a daemon's does.
  let (repository, reasons_pushed) = crate::daemon::observe_first(&daemon, move |state| {
    let repository = git_repository(state, "encoding-under-way");
    (repository, reasons(&pushed(state, repository, &pushing)))
  });
  assert!(
    reasons_pushed.iter().all(|reason| reason == b"ok"),
    "{reasons_pushed:?}"
  );
  let pushed_ids: std::collections::BTreeSet<ObjectId> = objects
    .iter()
    .map(|(kind, bytes)| {
      slates_git::object_name::name_of(ObjectFormat::Sha1, *kind, bytes).unwrap()
    })
    .collect();
  let wants: Vec<ObjectId> = named.iter().map(|(id, _)| *id).collect();
  let mut fetches = 0usize;
  let mut rounds = 0usize;
  loop {
    let wanting = wants.clone();
    // A round: a few turns of the encoding, then a fetch of every ref's object as a clone that stopped waiting asks.
    let (fetched, idle) = crate::daemon::observe_first(&daemon, move |state| {
      let mut idle = false;
      for _ in 0..ENCODING_TURNS_BETWEEN_FETCHES {
        if crate::vcs_store::git_encode::turn(state, repository)
          == crate::vcs_store::git_encode::Turned::Idle
        {
          idle = true;
          break;
        }
      }
      (fetched_ids(state, repository, &wanting), idle)
    });
    fetches += 1;
    rounds += 1;
    let set: std::collections::BTreeSet<ObjectId> = fetched.iter().copied().collect();
    assert_eq!(
      (fetched.len(), &set),
      (pushed_ids.len(), &pushed_ids),
      "fetch {fetches}: every object pushed, each once"
    );
    if idle {
      break;
    }
    assert!(rounds < ENCODE_TURNS_BOUND, "an encoding that never ended");
  }
  daemon.stop();
  assert!(
    fetches > 1,
    "fetched only once: the encoding was never under way"
  );
}

/// Shape: the encoding's turns between two fetches of [`t_goal93_a_clone_answered_while_the_encoding_is_under_way_is_whole`]:
/// few enough that many fetches meet a version's encoding under way.
const ENCODING_TURNS_BETWEEN_FETCHES: usize = 1;

/// Goal 93's whole-clone abort, its likely cause (w-sessionlog's reduction: past ~500 tags of git/git a clone stops
/// waiting for the repository's encoding, `encode_for_fetch`'s bounded wait, which derives each version's reach sets
/// first, A-614 amended, and is answered with its pack unfinished). Do: push a line of [`PACED_HISTORY`] commits as one
/// create (its versions' reach sets deferred, as git/git's whole push leaves them), then, with no derivation run, fetch
/// its head as `git clone` asks. Expect: the fetch answered with no refusal, its pack whole, holding every object pushed,
/// each once: a fetch costs time, never its answer. Red before the fix: refused, the pack ended with git's abort words.
#[test]
fn t_goal93_a_clone_answered_before_its_reach_sets_are_derived_is_whole() {
  crate::test_store::audit_on_stored_shard("git-push", |state| {
    let repository = git_repository(state, "underived");
    let mut objects = Vec::new();
    let mut parent = None;
    for at in 0..PACED_HISTORY {
      let (commit, made) = flat_commit(1, &format!("line {at}"), parent);
      objects.extend(made);
      parent = Some(commit);
    }
    let head = parent.unwrap();
    let head_hex = head.to_string();
    let created = [("", head_hex.as_str(), b"refs/heads/main".as_slice())];
    let received = pushed(
      state,
      repository,
      &request(&created, CAPABILITIES, &pack_of_objects(&objects)),
    );
    assert_eq!(reasons(&received), [b"ok".to_vec()]);
    let fetched = fetched_ids(state, repository, &[head]);
    let set: std::collections::BTreeSet<ObjectId> = fetched.iter().copied().collect();
    let pushed_ids: std::collections::BTreeSet<ObjectId> = objects
      .iter()
      .map(|(kind, bytes)| {
        slates_git::object_name::name_of(ObjectFormat::Sha1, *kind, bytes).unwrap()
      })
      .collect();
    assert_eq!((fetched.len(), &set), (pushed_ids.len(), &pushed_ids));
  });
}

/// Shape: r16's history at a unit's scale (main's ruling of 2026-11-21 22:40: git/git's master and its tags pushed
/// whole into an empty repository in one push): the commits of the line, a tag on every [`TAG_EVERY`]th.
const R16_HISTORY: usize = 1012;

/// Shape: how far apart r16's tags lie on its line: [`R16_HISTORY`] over this is 506 tags, w-sessionlog's count past
/// which the clone aborted.
const TAG_EVERY: usize = 2;

/// A commit of one small file of its own, `at`, over `parent`, as a long history's commits are at the scale a unit
/// test holds: its id and its objects (the commit, its tree, its blob).
fn line_commit(
  at: usize,
  parent: Option<ObjectId>,
) -> (ObjectId, Vec<(slates_git::object_name::Kind, Vec<u8>)>) {
  use slates_git::object_name::{Kind, name_of};
  let blob = format!("commit {at} of the line\n").into_bytes();
  let blob_id = name_of(ObjectFormat::Sha1, Kind::Blob, &blob).unwrap();
  let mut tree = b"100644 at\0".to_vec();
  tree.extend_from_slice(blob_id.as_bytes());
  let tree_id = name_of(ObjectFormat::Sha1, Kind::Tree, &tree).unwrap();
  let parent = parent
    .map(|parent| format!("parent {parent}\n"))
    .unwrap_or_default();
  let commit = format!(
    "tree {tree_id}\n{parent}author A <a@example.com> 1700000000 +0000\n\
     committer A <a@example.com> 1700000000 +0000\n\nline {at}\n"
  )
  .into_bytes();
  let commit_id = name_of(ObjectFormat::Sha1, Kind::Commit, &commit).unwrap();
  (
    commit_id,
    vec![
      (Kind::Commit, commit),
      (Kind::Tree, tree),
      (Kind::Blob, blob),
    ],
  )
}

/// A-850, r16's shape (main's ruling of 2026-11-21 22:40: git/git's master and its ~1,000 tags pushed whole into an
/// empty repository in one push never finished encoding, each tag's first version bringing everything its commit
/// reaches). Do: push a line of [`R16_HISTORY`] commits as `main` and an annotated tag of every [`TAG_EVERY`]th commit,
/// their commands sent by ref name as git sends them, into an empty repository in one push; derive every version's
/// sets; sum what each version of each ref brought (D(k), what its fragments encode); encode to the end; fetch every ref
/// whole. Expect: every ref `ok`; the sum exactly the objects pushed, each brought once (the derived multiple of history
/// and tags is one on a line: the push's commands are decided oldest commit first, so each create's search back at its
/// admission meets the move below it, its base, and brings only what that base does not reach); the search's counter
/// moved and within the history's commits; the encoding ends within its bound; the fetch's pack whole, every object
/// pushed once. Red before: each tag's first version brought its commit's whole history, about tags times history; with
/// the search but its commands decided as sent, 9,557 brought against 3,542 (2026-11-21, this Mac).
/// A pushed history: its objects (each a type and its bytes) and each ref's object and name.
type PushedHistory = (
  Vec<(slates_git::object_name::Kind, Vec<u8>)>,
  Vec<(ObjectId, Vec<u8>)>,
);

/// r16's history (A-850): a line of [`R16_HISTORY`] commits as `main` and an annotated tag of every [`TAG_EVERY`]th
/// commit, the refs sorted by name as git sends them.
fn r16_history() -> PushedHistory {
  use slates_git::object_name::Kind;
  let mut objects = Vec::new();
  let mut commits = Vec::new();
  let mut parent = None;
  for at in 0..R16_HISTORY {
    let (commit, made) = line_commit(at, parent);
    objects.extend(made);
    commits.push(commit);
    parent = Some(commit);
  }
  let mut named: Vec<(ObjectId, Vec<u8>)> =
    vec![(*commits.last().unwrap(), b"refs/heads/main".to_vec())];
  for (at, commit) in commits
    .iter()
    .enumerate()
    .filter(|(at, _)| at % TAG_EVERY == 0)
  {
    let name = format!("v{at}");
    let (tag, bytes) = tag_object(&name, (*commit, "commit"));
    objects.push((Kind::Tag, bytes));
    named.push((tag, format!("refs/tags/{name}").into_bytes()));
  }
  // Sent as git sends a push's commands, by ref name: v0, v10, v100, v1000, v1002, ..., far from the history's order.
  named.sort_by(|one, other| one.1.cmp(&other.1));
  (objects, named)
}

/// Turns `turn` a poll at a time on `daemon` until it answers `idle`, at most [`ENCODE_TURNS_BOUND`] times: the turns.
fn turned_to_idle<T: PartialEq + Send + 'static>(
  daemon: &crate::daemon::Daemon,
  turn: impl Fn(&mut ShardState) -> T + Clone + Send + 'static,
  idle: T,
) -> usize {
  let mut turns = 0usize;
  while crate::daemon::observe_first(daemon, turn.clone()) != idle {
    turns += 1;
    assert!(turns < ENCODE_TURNS_BOUND, "a turn that never ended");
  }
  turns
}

/// What every version of each of `names`' refs in `repository` brought, summed (D(k), what its fragments encode), and the
/// commits its admissions' searches back for a base met.
fn brought_and_searched(
  state: &mut ShardState,
  repository: RepositoryId,
  names: &[Vec<u8>],
) -> (u64, u64) {
  let keys: Vec<u64> = names
    .iter()
    .map(|name| {
      let name = std::str::from_utf8(name).unwrap();
      let green = state
        .db
        .partition()
        .repositories()
        .branch(repository, name.strip_prefix("refs/heads/").unwrap_or(name))
        .unwrap_or_else(|| panic!("{name} bound"));
      crate::vcs_store::green_key(green)
    })
    .collect();
  let lent = crate::vcs_store::Lent::new(&mut state.store, &mut state.vcs_stores);
  let (content, git) = lent.stores.git_ref(repository).expect("the store");
  let mut brought = 0u64;
  for key in keys {
    for version in 0..git.versions(key).len() {
      brought += git
        .repository
        .brought(content, key, version)
        .unwrap_or_else(|refused| panic!("{key} v{version}: {refused:?}"))
        .len();
    }
  }
  (brought, git.repository.counters().base_searched)
}

/// The reasons of `request` pushed to `repository` by its owner as an edge pushes one (`vcs_ssh::receive`'s steps): a
/// task on the daemon's first shard takes each step of the push on a poll of its own, the shard's state lent for each
/// and the task yielding between: each piece of the request as an edge hands it in, its end, and each step of its
/// answer, the names it asks to home carried between them. The test thread waits on the task's channel, never on the
/// shard. Until 2026-11-23 [`t_a850_r16_a_history_and_its_tags_pushed_whole_into_an_empty_repository_bring_each_object_once`]
/// drove its whole push inside one observation: 4,151 input pieces and 10,217 answer steps held the shard in one poll for
/// 1,540 to 1,763 ms on adacachy2 (a plain debug build), and under ThreadSanitizer past the watchdog's window, which ended
/// the library's test process (exit 4) with about 1,000 of its tests unrun.
fn pushed_a_step_a_poll(
  daemon: &crate::Daemon,
  repository: RepositoryId,
  request: &[u8],
) -> Vec<Vec<u8>> {
  let (sent, got) = std::sync::mpsc::channel();
  let request = request.to_vec();
  crate::daemon::observe_first(daemon, move |_| {
    let task = slates_rt::futures::spawn(async move {
      let _ = sent.send(stepped_drive(repository, &request).await);
    })
    .expect("the push's task");
    slates_rt::futures::detach(task).expect("the task detached");
  });
  got
    .recv()
    .expect("the push's task reported")
    .expect("the push answered")
}

/// The push of [`pushed_a_step_a_poll`], a step a poll: its reasons, or why it stopped.
async fn stepped_drive(repository: RepositoryId, request: &[u8]) -> Result<Vec<Vec<u8>>, String> {
  use crate::state::with_state;
  let mut push = with_state(|state| {
    ReceivePack::start(
      state,
      (&owner(), repository),
      true,
      &mut Vec::new(),
      Unpresented,
    )
  })
  .ok_or("no state to start")?
  .map_err(|failed| format!("not started: {failed:?}"))?;
  for piece in request.chunks(PIECE) {
    with_state(|state| push.input(state, piece))
      .ok_or("no state for the request")?
      .map_err(|failed| format!("not taken: {failed:?}"))?;
    slates_rt::futures::yield_now().await;
  }
  with_state(|state| push.end(state))
    .ok_or("no state for the end")?
    .map_err(|failed| format!("not ended: {failed:?}"))?;
  let mut answer = Vec::new();
  loop {
    slates_rt::futures::yield_now().await;
    let step = with_state(|state| push.answer_step(state, &mut answer))
      .ok_or("no state for a step")?
      .map_err(|failed| format!("a step failed: {failed:?}"))?;
    match step {
      Answering::Answered(received) => return Ok(reasons(&received)),
      Answering::Turned => {
        crate::vcs_store::upload::brought_back_waited(repository).await;
        carry_awaited(&mut push).await?;
      }
      #[cfg(unix)]
      step @ Answering::Hook(_) => {
        return Err(format!("no hook runs where no view is shown: {step:?}"));
      }
    }
  }
}

/// A-850's r16 shape (main's ruling of 2026-11-21 22:40): do push a history of [`R16_HISTORY`] commits and a tag on every
/// [`TAG_EVERY`]th into an empty repository in one push, driven a step a poll as an edge drives it
/// ([`pushed_a_step_a_poll`]), then turn its derivation and its encoding to idle, a turn a poll. Expect every command
/// `ok`, each object brought once, the search back meeting at most the history's commits, and a clone holding every
/// object pushed, each once.
#[test]
fn t_a850_r16_a_history_and_its_tags_pushed_whole_into_an_empty_repository_bring_each_object_once()
{
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (objects, named) = r16_history();
  assert_eq!(
    named.len(),
    R16_HISTORY / TAG_EVERY + 1,
    "main and 506 tags"
  );
  let ids: Vec<String> = named.iter().map(|(id, _)| id.to_string()).collect();
  let commands: Vec<(&str, &str, &[u8])> = named
    .iter()
    .zip(&ids)
    .map(|((_, name), id)| ("", id.as_str(), name.as_slice()))
    .collect();
  let pushing = request(&commands, CAPABILITIES, &pack_of_objects(&objects));
  // Each step of the push a poll of its own, and each turn of the derivation and of the encoding below one too: the
  // shard steps between them, as a daemon's does.
  let repository =
    crate::daemon::observe_first(&daemon, |state| git_repository(state, "r16-shape"));
  let reasons_pushed = pushed_a_step_a_poll(&daemon, repository, &pushing);
  assert!(
    reasons_pushed.iter().all(|reason| reason == b"ok"),
    "{reasons_pushed:?}"
  );
  turned_to_idle(
    &daemon,
    move |state| crate::vcs_store::git_derive::turn(state, repository),
    crate::vcs_store::git_derive::Turned::Idle,
  );
  let names: Vec<Vec<u8>> = named.iter().map(|(_, name)| name.clone()).collect();
  let (brought, searched) = crate::daemon::observe_first(&daemon, move |state| {
    brought_and_searched(state, repository, &names)
  });
  assert_eq!(
    brought,
    u64::try_from(objects.len()).unwrap(),
    "each object brought once: each tag's first version brings what the tag below it does not reach"
  );
  assert!(
    searched > 0 && searched <= u64::try_from(R16_HISTORY).unwrap(),
    "the search back met {searched} commits"
  );
  let turns = turned_to_idle(
    &daemon,
    move |state| crate::vcs_store::git_encode::turn(state, repository),
    crate::vcs_store::git_encode::Turned::Idle,
  );
  eprintln!("A-850 r16 shape: the encoding ended in {turns} turns");
  let pushed_ids: std::collections::BTreeSet<ObjectId> = objects
    .iter()
    .map(|(kind, bytes)| {
      slates_git::object_name::name_of(ObjectFormat::Sha1, *kind, bytes).unwrap()
    })
    .collect();
  let wants: Vec<ObjectId> = named.iter().map(|(id, _)| *id).collect();
  let fetched =
    crate::daemon::observe_first(&daemon, move |state| fetched_ids(state, repository, &wants));
  daemon.stop();
  let set: std::collections::BTreeSet<ObjectId> = fetched.iter().copied().collect();
  assert_eq!(
    (fetched.len(), &set),
    (pushed_ids.len(), &pushed_ids),
    "the clone holds every object pushed, each once"
  );
}

/// `repository`'s tags as `ls-refs` with `peel` lists them to its owner, a line each.
fn tags_listed_peeled(state: &mut ShardState, repository: RepositoryId) -> Vec<String> {
  use crate::vcs_store::upload::tests::{answered, lines_of, request as upload_request};
  let asking = ["peel".to_owned(), "ref-prefix refs/tags/".to_owned()];
  let listed = answered(
    state,
    (repository, &owner()),
    slates_git::upload::Asked::Request(&upload_request("ls-refs", &asking)),
  );
  assert_eq!(listed.1, None, "ls-refs refused");
  lines_of(&listed.0)
    .iter()
    .map(|line| String::from_utf8_lossy(line).trim_end().to_owned())
    .collect()
}

/// The objects of the pack a fetch of `wants` from `repository` answers its owner.
fn fetched_ids(
  state: &mut ShardState,
  repository: RepositoryId,
  wants: &[ObjectId],
) -> Vec<ObjectId> {
  use crate::vcs_store::upload::tests::{answered, fetch, pack_of};
  let fetched = answered(
    state,
    (repository, &owner()),
    slates_git::upload::Asked::Request(&fetch(wants, &[])),
  );
  assert_eq!(fetched.1, None, "fetch refused");
  let unpacked = slates_git::unpack::read_whole(
    &pack_of(&fetched.0),
    ObjectFormat::Sha1,
    &slates_git::unpack::NoIndex,
    &mut slates_git::Admission::new(u64::MAX),
  )
  .expect("a whole pack");
  unpacked.objects.iter().map(|object| object.id).collect()
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
      Answering::Turned => {
        slates_rt::futures::yield_now().await;
        carry_awaited(&mut push).await?;
      }
      #[cfg(unix)]
      Answering::Hook(_) => return Err("no hook is configured".to_owned()),
    }
  };
  Ok((received, longest))
}

/// A-752 (main's order of 2026-11-20, w-git's measure: one command bringing 3,502 objects over a flat 3,500-file tree
/// built its work in one step, 374 to 642 ms debug, so a monorepo's push would outlast the watchdog), with A-783's rule
/// (a turn yields at its quantum when someone waits). Do: create `main` at a flat tree of [`FLAT_FILES`] files, then push
/// a commit changing every file, answering a step at a time, no one else on the shard; then fetch `main`'s head. Expect:
/// both `ok`; the update's build a unit at a time (a unit a file at least) in more than one turn, its turns going on past
/// their first quantum while no one waited (`more_quanta` moved), none past its ceiling (the watchdog's look) by more
/// than a unit and its opening, and none past its budget (`push.build.past_budget` 0); the fetch holding every object
/// both pushes brought, each once. How many quanta a turn spans is the host's speed over its quantum, not the build's
/// slicing: "a tenth as many turns as quanta" failed 1 in 3 alone at load ~40, "960 turns for 8081 quanta", every turn
/// within its bounds (main 01a20182's order of 2026-11-22 06:13).
#[test]
fn t_a752_a_push_over_a_flat_head_builds_its_work_a_unit_at_a_time() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  crate::test_store::keep_landed(&daemon);
  let repository = crate::daemon::observe_first(&daemon, |state| git_repository(state, "flat"));
  let (first, first_objects) = flat_commit(FLAT_FILES, "first", None);
  let create = request(
    &[("", &first.to_string(), b"refs/heads/main")],
    CAPABILITIES,
    &pack_of_objects(&first_objects),
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
  let fetched = crate::daemon::observe_first(&daemon, move |state| {
    fetched_ids(state, repository, &[second])
  });
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
  let quanta = (after.more_quanta - before.more_quanta) + turns;
  // A unit at a time still: a unit a file at least, whatever quantum the shard keeps. (The rule is units, not quanta:
  // how many quanta the units span is the machine's speed over its live quantum, which load and few CPUs raise; "more
  // quanta than files" failed under load, 1 in 2 at load 12.8, with every unit still its own.)
  let units = after.units - before.units;
  assert!(
    units >= FLAT_FILES as u64,
    "the update's work ran {units} units for {FLAT_FILES} files"
  );
  // Sliced: more than one turn. A-783: no one waited, so turns went on past their first quantum.
  assert!(
    turns > 1 && after.more_quanta > before.more_quanta,
    "{turns} turns for {quanta} quanta: the build was not sliced, or its turns did not go on while no one waited"
  );
  let ceiling = crate::daemon::HEARTBEAT_NS;
  assert!(
    after.longest_ns <= ceiling + after.longest_unit_ns + after.longest_opening_ns,
    "a turn of {} ns past its {ceiling} ns ceiling",
    after.longest_ns
  );
  assert_eq!(after.past_budget, 0, "{after:?}");
  let brought: std::collections::BTreeSet<ObjectId> = first_objects
    .into_iter()
    .chain(objects)
    .map(|(kind, bytes)| {
      slates_git::object_name::name_of(ObjectFormat::Sha1, kind, &bytes).unwrap()
    })
    .collect();
  let set: std::collections::BTreeSet<ObjectId> = fetched.iter().copied().collect();
  assert_eq!(
    (fetched.len(), &set),
    (brought.len(), &brought),
    "the fetch of main's head holds every object both pushes brought, each once"
  );
}

/// A-724 piece 8c and A-722's record v4 (main's rulings of 2026-11-20: a record names its names and its pack only once
/// they are durable at `f + 1` in the region). Do: with version 4 on for the shard, push stock git's `create` and
/// `update`, driving each answer as an edge does (a turn, a yield, the names and then the pack it asks to home homed in
/// the region, the home publishing itself), then make the store again from the log. Expect: both pushes answered `ok`;
/// each record that names objects version 4, naming its names and its pack by the identities the region homed; and
/// the store made again from those records equal to the live one. Every wait
/// is on its event (an answer, a channel), none on a fixed time: a loaded machine is the norm.
#[test]
fn t_a724_a_pushs_record_names_its_names_once_they_are_homed_in_the_region() {
  let (_store, daemon) = crate::test_store::audit_daemon_of_stored(1);
  crate::test_store::keep_landed(&daemon);
  let (sent, got) = std::sync::mpsc::channel();
  crate::daemon::observe_first(&daemon, move |state| {
    let repository = git_repository(state, "homed");
    let requests: Vec<Vec<u8>> = chain()
      .into_iter()
      .take(2)
      .map(|(scenario, _)| scenario.request.to_vec())
      .collect();
    let task = slates_rt::futures::spawn(async move {
      let mut outcome = Vec::new();
      for request in requests {
        outcome.push(homed_drive(repository, &request).await);
      }
      let _ = sent.send((repository, outcome));
    })
    .expect("the pushes' task");
    slates_rt::futures::detach(task).expect("detached");
  });
  let (repository, outcome) = got.recv().expect("the pushes answered");
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
    versions
      .iter()
      .all(|version| *version == crate::vcs_store::record::VERSION_LANDED),
    "versions {versions:?}"
  );
}

/// A-724 piece 8c and A-767 C5 (a replay reads a record's names locally, else from the node's changeset store; a node
/// whose store lacks the changeset reads them from their region homes). Do: on a store-backed node of four shards, with
/// the homing on, push stock git's `create` on a shard that is not its names' home (pushed again on another where the
/// first was); drop the names from that owner's content store; make the store again from the log, a task on the owner
/// waiting for the rebuild's end. Expect: the rebuild reads the record's changeset back from the node's store before its
/// replay (counted read), and the store made again is the live one. Every wait is on its event, none on a fixed time: a
/// loaded machine is the norm.
#[test]
fn t_a767_c5_a_rebuild_reads_a_records_names_back_from_the_nodes_store() {
  let (_store, daemon) = crate::test_store::audit_daemon_of_stored(4);
  let (first, identity, home) = pushed_v3_at(&daemon, 0, "fetched");
  let (partition, repository) =
    match crate::daemon::observe_at(&daemon, 0, |state| state.shard) == home {
      false => (0, first),
      true => (1, pushed_v3_at(&daemon, 1, "fetched-again").0),
    };
  let (sent, got) = std::sync::mpsc::channel();
  crate::daemon::observe_at(&daemon, partition, move |state| {
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
      .get(crate::vcs_store::CHANGESET_READ)
      .copied()
      .unwrap_or(0);
    crate::vcs_store::rebuild(state, repository);
    // The rebuild's end, waited for on the owner shard: its event, not a time.
    let task = slates_rt::futures::spawn(async move {
      loop {
        let ended = crate::state::with_state(|state| {
          state.vcs_stores.unrecovered(repository).is_none()
            && !state.vcs_stores.rebuilding(repository)
        });
        if ended != Some(false) {
          break;
        }
        slates_rt::futures::yield_now().await;
      }
      let fetched = crate::state::with_state(|state| {
        state
          .refusals
          .get(crate::vcs_store::CHANGESET_READ)
          .copied()
          .unwrap_or(0)
      })
      .unwrap_or(0);
      let _ = sent.send((owner_shard, fetched > before, main));
    })
    .expect("the rebuild's watch");
    slates_rt::futures::detach(task).expect("detached");
  });
  let (owner_shard, fetched, main) = got.recv().expect("the rebuild ended");
  assert_ne!(
    home, owner_shard,
    "the names' home is the owner shard: the case reads nothing from the region"
  );
  assert!(
    fetched,
    "the names were not read back from the node's store"
  );
  let (whole, compared) = std::sync::mpsc::channel();
  crate::daemon::observe_at(&daemon, partition, move |state| {
    crate::vcs_store::tests::assert_made_again_whole(
      state,
      repository,
      &[crate::vcs_store::green_key(main)],
    );
    let _ = whole.send(());
  });
  compared.recv().expect("compared");
}

/// Stock git's `create` pushed with version 3 on into a new repository `name` on `daemon`'s shard at `partition`, its
/// answer awaited (its event): the repository, its record's names' identity, and the shard that is the names' home.
fn pushed_v3_at(
  daemon: &crate::Daemon,
  partition: usize,
  name: &'static str,
) -> (RepositoryId, slates_vcs::ContentId, u16) {
  let (sent, got) = std::sync::mpsc::channel();
  crate::daemon::observe_at(daemon, partition, move |state| {
    let repository = git_repository(state, name);
    let request = chain()
      .into_iter()
      .next()
      .map(|(scenario, _)| scenario.request.to_vec())
      .expect("create");
    let task = slates_rt::futures::spawn(async move {
      let answered = homed_drive(repository, &request).await;
      let _ = sent.send((repository, answered.map(|_| ())));
    })
    .expect("the push's task");
    slates_rt::futures::detach(task).expect("detached");
  });
  let (repository, answered) = got.recv().expect("the push answered");
  answered.expect("the push answered ok");
  crate::daemon::observe_at(daemon, partition, move |state| {
    let record = crate::vcs_store::tests::records(state, repository)
      .pop()
      .expect("a record");
    let (identity, _) = crate::vcs_store::record::Record::decode(
      &record,
      slates_git::object_name::ObjectFormat::Sha1,
    )
    .expect("decoded")
    .named
    .expect("version 3");
    let home = crate::vcs_store::homes::home_of(&state.shards, &identity).expect("a home");
    (repository, identity, home)
  })
}

/// A-724 piece 8c (the takeover test's fix): a push's records ride its repository's names log. Do: push stock git's
/// `create`, then take the state logs' records the partition handed on. Expect: a names-log record carrying the push's
/// record (`LoggedChange::Objects`), the same bytes the repository's register keeps.
#[test]
fn t_a724_a_pushs_record_rides_its_repositorys_names_log() {
  crate::test_store::audit_on_stored_shard("git-push", |state| {
    let repository = git_repository(state, "shipped");
    let _ = state.db.take_state_log_records();
    pushed_from_the_fixtures(state, repository, &["create"]);
    let records = state.db.take_state_log_records();
    let carried: Vec<Vec<u8>> = records
      .iter()
      .filter(|record| {
        record.repository == repository && record.log == slates_db::catalog::StateLog::Names
      })
      .flat_map(|record| record.changes.iter())
      .filter_map(|change| match change {
        slates_db::catalog::LoggedChange::Objects { record } => Some(record.clone()),
        _ => None,
      })
      .collect();
    let kept: Vec<Vec<u8>> = crate::vcs_store::tests::records(state, repository);
    assert!(
      !carried.is_empty(),
      "no names record carried the push's record: {records:?}"
    );
    assert_eq!(carried, kept);
  });
}

/// A-724 piece 8c and C5 part 2b piece 2 (a decision never waits for a homing nobody runs, found by the takeover test;
/// and no record names a push's objects inline). Do: push stock git's `create` through a driver that never takes the
/// homing its decision asks for. Expect: the push refused `Unlanded`, the untaken homing counted, and the repository's
/// register holding no record. Red before piece 2: answered `ok`, its record version 2.
#[test]
fn t_a767_c5b_a_push_whose_homing_no_edge_takes_is_refused() {
  crate::test_store::audit_on_stored_shard("git-push", |state| {
    let repository = git_repository(state, "untaken");
    let request = chain()
      .into_iter()
      .next()
      .map(|(scenario, _)| scenario.request.to_vec())
      .expect("create");
    let (_, mut push) = started(state, repository);
    for piece in request.chunks(PIECE) {
      push.input(state, piece).expect("taken");
    }
    push.end(state).expect("ended");
    let mut answer = Vec::new();
    let refused = loop {
      match push.answer_step(state, &mut answer) {
        Ok(Answering::Answered(_)) => break None,
        Ok(Answering::Turned) => {}
        #[cfg(unix)]
        Ok(step @ Answering::Hook(_)) => panic!("no hook runs here: {step:?}"),
        Err(failed) => break Some(format!("{failed:?}")),
      }
    };
    assert_eq!(refused.as_deref(), Some("Unlanded"));
    assert_eq!(
      state
        .refusals
        .get(crate::vcs_store::homes::HOMING_UNTAKEN)
        .copied(),
      Some(1)
    );
    assert_eq!(
      records(state, repository),
      0,
      "nothing of the push in the register"
    );
  });
}

/// A pack as stock git's client sends a history: `objects` in order, each whole but the blobs after the first, each a
/// REF_DELTA against the blob before it (`slates_git::store::delta_of`), every entry's data in stored zlib blocks.
fn deltified_pack(objects: &[(slates_git::object_name::Kind, Vec<u8>)]) -> Vec<u8> {
  use slates_git::entry::{self, Header, Representation};
  use slates_git::object_name::Kind;
  let mut pack = Vec::new();
  let count = u32::try_from(objects.len()).unwrap();
  let mut writer = slates_git::pack::Writer::new(ObjectFormat::Sha1, count, &mut pack);
  let mut at = entry::PACK_HEADER_LEN;
  let mut previous: Option<&Vec<u8>> = None;
  for (kind, content) in objects {
    let delta = match (kind, previous) {
      (Kind::Blob, Some(base)) => slates_git::store::delta_of(base, content).map(|delta| {
        let base = slates_git::object_name::name_of(ObjectFormat::Sha1, Kind::Blob, base).unwrap();
        (base, delta)
      }),
      _ => None,
    };
    let (header, data) = match delta {
      Some((base, delta)) => (
        Header {
          representation: Representation::RefDelta { base },
          size: u64::try_from(delta.len()).unwrap(),
        },
        stored_zlib(&delta),
      ),
      None => (
        Header {
          representation: Representation::Whole(*kind),
          size: u64::try_from(content.len()).unwrap(),
        },
        stored_zlib(content),
      ),
    };
    if *kind == Kind::Blob {
      previous = Some(content);
    }
    entry::write(header, at, &mut writer).unwrap();
    slates_git::out::Out::put(&mut writer, &data);
    at += entry::written_len(header, at).unwrap() + u64::try_from(data.len()).unwrap();
  }
  writer.finish().unwrap();
  pack
}

/// Format: what an entry may carry past the pushed one's bytes when it is sent on (A-859): its header written again,
/// at most a type-and-size's varint and an offset's (ten bytes each for a 64-bit value), and a reference delta's base
/// named by its id in place of an offset (twenty bytes, SHA-1).
const REUSED_ENTRY_SLACK: usize = 10 + 10 + 20;

/// A line of [`TAGGED_LINE`] growing commits, each tagged: its objects (every commit's, then every tag's) and its refs
/// in name order (`refs/heads/main` at the last commit, `refs/tags/v<n>` at each commit's tag).
#[allow(clippy::type_complexity)]
fn tagged_line() -> (
  Vec<(slates_git::object_name::Kind, Vec<u8>)>,
  Vec<(ObjectId, Vec<u8>)>,
) {
  use slates_git::object_name::Kind;
  let mut objects = Vec::new();
  let mut commits = Vec::new();
  let mut parent = None;
  for at in 0..TAGGED_LINE {
    let (commit, made) = growing_commit(at, parent);
    objects.extend(made);
    commits.push(commit);
    parent = Some(commit);
  }
  let mut named: Vec<(ObjectId, Vec<u8>)> =
    vec![(*commits.last().unwrap(), b"refs/heads/main".to_vec())];
  for (at, commit) in commits.iter().enumerate() {
    let name = format!("v{at}");
    let (tag, bytes) = tag_object(&name, (*commit, "commit"));
    objects.push((Kind::Tag, bytes));
    named.push((tag, format!("refs/tags/{name}").into_bytes()));
  }
  named.sort_by(|one, other| one.1.cmp(&other.1));
  (objects, named)
}

/// A-859 (main's ruling of 2026-11-22 03:38; found by use: a git/git clone from slates moved 4-5 GiB where stock git's pack
/// is about 300 MB, every object sent whole and uncompressed: 160 of 160 whole at unit scale). Do: push a line of
/// [`TAGGED_LINE`] commits of a growing file and a tag of each, its pack as stock git sends one (each blob after the first
/// a delta against the one before); clone every ref whole before any version is encoded (the stream's loose stage), then
/// encode its versions and clone again (from the kept fragments). Expect: each clone's pack at most the pushed pack's
/// bytes plus [`REUSED_ENTRY_SLACK`] an object (each pushed entry sent on as it lies), printing both; the stream and the
/// encoding each counting entries reused whole and every pushed delta reused, none refused; each clone whole, every
/// object once; and no pushed object encoded afresh by either (main 01a20182's ruling of 2026-11-22 05:38: a stored entry
/// reused whenever one exists). Red before: every object whole and uncompressed, the clone near the objects' raw size;
/// then (main's order of 04:53) the stream reusing 20 of the 39 deltas, a delta whose base's position (by type, then id)
/// came after its own sent fresh, where git's `write_one` writes a delta's base first; then 117 of the pushed commits,
/// trees and tags searched and encoded afresh, their whole entries passed over where the encoder would try a delta.
#[test]
fn t_a859_a_clone_of_a_deltified_push_sends_its_entries_as_they_lie() {
  crate::test_store::audit_on_stored_shard("git-push", |state| {
    let repository = git_repository(state, "reused");
    let (objects, named) = tagged_line();
    let pack = deltified_pack(&objects);
    let ids: Vec<String> = named.iter().map(|(id, _)| id.to_string()).collect();
    let commands: Vec<(&str, &str, &[u8])> = named
      .iter()
      .zip(&ids)
      .map(|((_, name), id)| ("", id.as_str(), name.as_slice()))
      .collect();
    let received = pushed(state, repository, &request(&commands, CAPABILITIES, &pack));
    assert!(
      reasons(&received).iter().all(|reason| reason == b"ok"),
      "{:?}",
      received.outcomes
    );
    while crate::vcs_store::git_derive::turn(state, repository)
      != crate::vcs_store::git_derive::Turned::Idle
    {}
    let wants: Vec<ObjectId> = named.iter().map(|(id, _)| *id).collect();
    let fetch = crate::vcs_store::upload::tests::fetch(&wants, &[]);
    let bound = pack.len() + objects.len() * REUSED_ENTRY_SLACK;
    // Cloned before any version is encoded: every object from the stream's loose stage.
    let (streamed, counted_streamed) =
      crate::vcs_store::upload::tests::fetched_with_counters(state, (repository, &owner()), &fetch);
    let streamed = crate::vcs_store::upload::tests::pack_of(&streamed);
    eprintln!(
      "A-859 before encoding: pushed pack {} bytes, clone pack {} bytes; {counted_streamed:?}",
      pack.len(),
      streamed.len()
    );
    assert!(
      streamed.len() <= bound,
      "the streamed clone's pack {} past {bound}",
      streamed.len()
    );
    let deltas = u64::try_from(TAGGED_LINE - 1).unwrap();
    assert_the_stream_sent_every_pushed_entry_as_it_lies(&counted_streamed, deltas, objects.len());
    assert_whole_clone(&streamed, objects.len());
    while crate::vcs_store::git_encode::turn(state, repository)
      != crate::vcs_store::git_encode::Turned::Idle
    {}
    let (answer, counted) =
      crate::vcs_store::upload::tests::fetched_with_counters(state, (repository, &owner()), &fetch);
    let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
    let reused = (
      count(crate::vcs_store::git_encode::REUSED_WHOLE),
      count(crate::vcs_store::git_encode::REUSED_DELTAS),
      count(crate::vcs_store::git_encode::REUSE_REFUSED),
    );
    let raw: usize = objects.iter().map(|(_, bytes)| bytes.len()).sum();
    let clone = crate::vcs_store::upload::tests::pack_of(&answer);
    eprintln!(
      "A-859: pushed pack {} bytes, clone pack {} bytes, objects' raw {raw}; reused (whole, deltas, refused) {reused:?}; {counted:?}",
      pack.len(),
      clone.len()
    );
    assert!(
      clone.len() <= bound,
      "the clone's pack {} past {bound}",
      clone.len()
    );
    assert!(
      reused.0 > 0 && reused.1 == deltas && reused.2 == 0,
      "{reused:?}"
    );
    assert_eq!(
      reused.0 + reused.1,
      u64::try_from(objects.len()).unwrap(),
      "every pushed object kept as its entry lies, none encoded: {reused:?}"
    );
    assert_whole_clone(&clone, objects.len());
  });
}

/// The streamed clone's counters in [`t_a859_a_clone_of_a_deltified_push_sends_its_entries_as_they_lie`], `counted`:
/// every one of the push's `deltas` reused and none refused, and every one of its `objects` sent as its entry lies.
fn assert_the_stream_sent_every_pushed_entry_as_it_lies(
  counted: &slates_git::store::UploadCounters,
  deltas: u64,
  objects: usize,
) {
  assert!(
    counted.reused_whole > 0 && counted.reused_deltas == deltas && counted.reuse_refused == 0,
    "every one of the {deltas} pushed deltas reused: {counted:?}"
  );
  // main 01a20182's ruling of 2026-11-22 05:38: a stored entry is reused whenever one exists, so no pushed object is
  // searched or encoded afresh.
  assert_eq!(
    (
      counted.loose_whole + counted.loose_deltas,
      counted.reused_whole + counted.reused_deltas,
    ),
    (0, u64::try_from(objects).unwrap()),
    "every pushed object sent as its entry lies, none encoded: {counted:?}"
  );
  // A-864: each sent on by its kept CRC32, none rebuilt to be checked; the pack read front to back (piece 1), no
  // pushed entry read before the one read before it.
  assert_eq!(
    (
      counted.reuse_rebuilt,
      counted.reuse_crc_missed,
      counted.backward_reads
    ),
    (0, 0, 0),
    "{counted:?}"
  );
}

/// Shape: the files of [`t_a859_an_encoding_places_a_pushed_deltas_base_before_it`]'s one commit, each a line longer
/// than the one before, so each after the first is pushed as a delta against a smaller one.
const GROWING_FILES: usize = 8;

/// A-859 (main 01a20182's order of 2026-11-22 04:53: write deltas in the stored pack's own order, as git's pack reuse
/// does). Do: push one commit of [`GROWING_FILES`] files, each a line longer than the one before, its pack as stock git
/// sends one (each blob after the first a delta against the one before, a smaller one); encode its version, then clone
/// it from the kept fragments. Expect: every pushed delta reused by the encoding, none refused, though the encoding
/// orders the larger first, so each delta's base is placed before it (git's `write_one`); the clone whole. Red before:
/// no delta reused, each one's base still in the encoding's order behind it.
#[test]
fn t_a859_an_encoding_places_a_pushed_deltas_base_before_it() {
  use slates_git::object_name::{Kind, name_of};
  crate::test_store::audit_on_stored_shard("git-push", |state| {
    let repository = git_repository(state, "based");
    let blobs: Vec<Vec<u8>> = (0..GROWING_FILES)
      .map(|file| {
        (0..=file + 20)
          .flat_map(|line| {
            format!("line {line:04} of a file that grows a line a file, long enough to delta\n")
              .into_bytes()
          })
          .collect()
      })
      .collect();
    let mut tree = Vec::new();
    for (file, blob) in blobs.iter().enumerate() {
      tree.extend_from_slice(format!("100644 file{file:02}\0").as_bytes());
      tree.extend_from_slice(
        name_of(ObjectFormat::Sha1, Kind::Blob, blob)
          .unwrap()
          .as_bytes(),
      );
    }
    let tree_id = name_of(ObjectFormat::Sha1, Kind::Tree, &tree).unwrap();
    let commit = format!(
      "tree {tree_id}\nauthor A <a@example.com> 1700000000 +0000\n\
       committer A <a@example.com> 1700000000 +0000\n\ngrowing files\n"
    )
    .into_bytes();
    let commit_id = name_of(ObjectFormat::Sha1, Kind::Commit, &commit).unwrap();
    let mut objects = vec![(Kind::Commit, commit), (Kind::Tree, tree)];
    objects.extend(blobs.into_iter().map(|blob| (Kind::Blob, blob)));
    let pack = deltified_pack(&objects);
    let id = commit_id.to_string();
    let commands: Vec<(&str, &str, &[u8])> = vec![("", id.as_str(), b"refs/heads/main")];
    let received = pushed(state, repository, &request(&commands, CAPABILITIES, &pack));
    assert!(
      reasons(&received).iter().all(|reason| reason == b"ok"),
      "{:?}",
      received.outcomes
    );
    while crate::vcs_store::git_derive::turn(state, repository)
      != crate::vcs_store::git_derive::Turned::Idle
    {}
    while crate::vcs_store::git_encode::turn(state, repository)
      != crate::vcs_store::git_encode::Turned::Idle
    {}
    let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
    let reused = (
      count(crate::vcs_store::git_encode::REUSED_DELTAS),
      count(crate::vcs_store::git_encode::REUSE_REFUSED),
    );
    assert_eq!(
      reused,
      (u64::try_from(GROWING_FILES - 1).unwrap(), 0),
      "every pushed delta reused, none refused"
    );
    let fetch = crate::vcs_store::upload::tests::fetch(&[commit_id], &[]);
    let (answer, _) =
      crate::vcs_store::upload::tests::fetched_with_counters(state, (repository, &owner()), &fetch);
    assert_whole_clone(
      &crate::vcs_store::upload::tests::pack_of(&answer),
      objects.len(),
    );
  });
}

/// `pack` is a whole pack of `objects` objects, each once, every delta resolved to its object (read as `index-pack`
/// reads one).
fn assert_whole_clone(pack: &[u8], objects: usize) {
  let unpacked = slates_git::unpack::read_whole(
    pack,
    ObjectFormat::Sha1,
    &slates_git::unpack::NoIndex,
    &mut slates_git::Admission::new(u64::MAX),
  )
  .expect("a whole pack");
  let fetched: std::collections::BTreeSet<ObjectId> =
    unpacked.objects.iter().map(|object| object.id).collect();
  assert_eq!((unpacked.objects.len(), fetched.len()), (objects, objects));
}

/// [`short_homing_drive`] whose names' homing is carried as an edge carries it and only the pack's is handed back fallen
/// short (A-727 (d1a)): the push refused after a homing kept its names in the region.
async fn half_homed_drive(repository: RepositoryId, request: &[u8]) -> Result<Vec<u8>, String> {
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
  .ok_or("no state")?
  .map_err(|failed| format!("{failed:?}"))?;
  with_state(|state| push.input(state, request))
    .ok_or("no state")?
    .map_err(|failed| format!("{failed:?}"))?;
  with_state(|state| push.end(state))
    .ok_or("no state")?
    .map_err(|failed| format!("{failed:?}"))?;
  let mut answer = Vec::new();
  let mut homings = 0usize;
  loop {
    let step = with_state(|state| push.answer_step(state, &mut answer))
      .ok_or("no state")?
      .map_err(|failed| format!("{failed:?}"))?;
    match step {
      Answering::Answered(_) => return Ok(answer),
      Answering::Turned => {
        slates_rt::futures::yield_now().await;
        let Some(asked) = push.take_homing() else {
          continue;
        };
        homings += 1;
        let carried = match asked {
          crate::git_push::Carry::Home(bytes, _) if homings > 1 => {
            crate::git_push::Carried::Homed(crate::vcs_store::homes::Homed {
              outcome: Err(crate::vcs_store::homes::HomeRefused::Unanswered),
              bytes,
            })
          }
          asked => {
            let (origin, shards) =
              with_state(|state| (state.shard, state.shards.clone())).ok_or("no state")?;
            crate::git_push::carry(origin, &shards, asked).await
          }
        };
        with_state(|state| push.homed(state, carried)).ok_or("no state")?;
      }
      #[cfg(unix)]
      step @ Answering::Hook(_) => return Err(format!("no hook runs here: {step:?}")),
    }
  }
}

/// Shape: the turns a test waits for the shard's settle task (A-727 (d1a)) to settle what a push's end owed: a turn a
/// settle, a handful owed, room to spare.
const SETTLE_TURNS: usize = 1_000;

/// Waits a turn at a time, at most [`SETTLE_TURNS`], until the shard's settle task has settled `settles` (counted either
/// way): whether it had.
async fn settled(settles: u64) -> bool {
  for _ in 0..SETTLE_TURNS {
    let done = crate::state::with_state(|state| {
      let counted = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
      counted(crate::vcs_store::homes::SETTLED)
        + counted(crate::vcs_store::homes::SETTLE_UNANSWERED)
    })
    .unwrap_or(0);
    if done >= settles {
      return true;
    }
    slates_rt::futures::yield_now().await;
  }
  false
}

/// A-727 (d1a) (main 01a20182's rulings of 2026-11-23: a region hold names its owner, and a refused attempt releases its
/// own). Do: on a store-backed shard, push stock git's `create`, its names homed in the region and its pack's homing
/// handed back fallen short. Expect: the push refused `Unlanded`, its attempt's release settled, and nothing held for the
/// region. Failing first: the names' region hold stayed, no record naming them, for the life of the home.
#[test]
fn t_a727_d1a_a_push_refused_after_its_names_were_homed_leaves_no_hold_in_the_region() {
  let (_store, daemon) = crate::test_store::audit_daemon_of_stored(1);
  let (sent, got) = std::sync::mpsc::channel();
  crate::daemon::observe_first(&daemon, move |state| {
    let repository = git_repository(state, "half-homed");
    let request = chain()
      .into_iter()
      .next()
      .map(|(scenario, _)| scenario.request.to_vec())
      .expect("create");
    let task = slates_rt::futures::spawn(async move {
      let outcome = half_homed_drive(repository, &request).await;
      // Two releases: the names' (homed) and the pack's (its homing fallen).
      let settled = settled(2).await;
      let held = crate::vcs_store::homes::region_held_here();
      let unanswered = crate::state::with_state(|state| {
        state
          .refusals
          .get(crate::vcs_store::homes::SETTLE_UNANSWERED)
          .copied()
          .unwrap_or(0)
      });
      let _ = sent.send((outcome, settled, held, unanswered));
    })
    .expect("the push's task");
    slates_rt::futures::detach(task).expect("detached");
  });
  let (outcome, settled, held, unanswered) = got.recv().expect("the push ended");
  assert_eq!(outcome, Err("Unlanded".to_owned()));
  assert!(settled, "the attempt's release settled");
  assert_eq!(unanswered, Some(0), "every home answered it");
  assert!(held.is_empty(), "nothing held for the region: {held:?}");
}

/// A-727 (d1a), two repositories on one identity (main's ruling: a refused attempt's release leaves another repository's
/// hold of the same object untouched). Do: on a store-backed shard, push stock git's `create` to repository `a` whole,
/// its holds settled to its records; then the same push to repository `b`, its names homed (the same object as `a`'s)
/// and its pack's homing fallen short. Expect: `a`'s push answered, `b`'s refused `Unlanded`; afterwards every object
/// held for the region held for `a`'s records alone, as many as after `a`'s push: `b`'s release took nothing of `a`'s.
#[test]
fn t_a727_d1a_a_refused_push_leaves_another_repositorys_hold_of_the_same_object() {
  let (_store, daemon) = crate::test_store::audit_daemon_of_stored(1);
  let (sent, got) = std::sync::mpsc::channel();
  crate::daemon::observe_first(&daemon, move |state| {
    let first = git_repository(state, "a");
    let second = git_repository(state, "b");
    let request = chain()
      .into_iter()
      .next()
      .map(|(scenario, _)| scenario.request.to_vec())
      .expect("create");
    let task = slates_rt::futures::spawn(async move {
      let answered = homed_drive(first, &request).await.is_ok();
      // Two moves: the names' and the pack's; then b's two releases.
      let moved = settled(2).await;
      let after_first = crate::vcs_store::homes::region_held_here();
      let refused = half_homed_drive(second, &request).await;
      let released = settled(4).await;
      let held = crate::vcs_store::homes::region_held_here();
      let owners: Vec<Vec<slates_cluster::homes::HomeOwner>> = held
        .iter()
        .map(crate::vcs_store::homes::region_owners)
        .collect();
      let _ = sent.send((
        (answered, moved, after_first.len()),
        (refused, released, held.len(), owners),
        first,
      ));
    })
    .expect("the pushes' task");
    slates_rt::futures::detach(task).expect("detached");
  });
  let ((answered, moved, before), (refused, released, after, owners), first) =
    got.recv().expect("the pushes ended");
  assert!(
    answered && moved,
    "a's push answered, its holds moved to its records"
  );
  assert!(before > 0, "a's push held something for the region");
  assert_eq!(refused, Err("Unlanded".to_owned()));
  assert!(released, "b's release settled");
  assert_eq!(after, before, "nothing of a's let go");
  let records = slates_cluster::homes::HomeOwner::Record {
    repository: first.bytes,
  };
  assert!(
    owners.iter().all(|owners| owners == &vec![records]),
    "every hold a's records' alone: {owners:?}"
  );
}
