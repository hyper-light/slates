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

/// Makes git repository `name`, SHA-1, its default branch `main`, on `daemon`'s first shard as a client's creation is
/// made, this thread waiting for its record ([`crate::test_store::create_on`]): its id. The shard is never held through
/// the store's commit (docs/bugs/2026-11-24-a-test-creation-held-its-shard-through-the-stores-commit.md).
pub(crate) fn git_repository_on(daemon: &crate::Daemon, name: &str) -> RepositoryId {
  crate::test_store::create_on(
    daemon,
    0,
    &owner(),
    (name, VcsTool::Git, WireFormat::Sha1, "main"),
  )
  .0
}

/// Runs `history` on a stored daemon's first shard ([`crate::test_store::audit_daemon_stored`], its landed packs kept
/// resident by [`crate::test_store::keep_landed`]) with git repositories `names` made first from the
/// test's thread ([`git_repository_on`]), never on the shard: their ids, in `names`' order. The shard is never held
/// through a creation's commit (`test_store::refuse_on_a_shard`: w-hg's guard of 2026-11-25).
pub(crate) fn on_stored_shard_with<T: Send + 'static, const N: usize>(
  names: [&'static str; N],
  history: impl FnOnce(&mut ShardState, [RepositoryId; N]) -> T + Clone + Send + 'static,
) -> T {
  on_stored_shard_configured_with(names, |_| {}, history)
}

/// [`on_stored_shard_with`] with the daemon's configuration changed by `configure` first.
pub(crate) fn on_stored_shard_configured_with<T: Send + 'static, const N: usize>(
  names: [&'static str; N],
  configure: impl FnOnce(&mut crate::DaemonConfig),
  history: impl FnOnce(&mut ShardState, [RepositoryId; N]) -> T + Clone + Send + 'static,
) -> T {
  let (store, daemon) = crate::test_store::audit_daemon_stored_configured(configure);
  crate::test_store::keep_landed(&daemon);
  let made = names.map(|name| git_repository_on(&daemon, name));
  let answer = crate::daemon::observe_first(&daemon, move |state| history(state, made));
  daemon.stop();
  drop(store);
  answer
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

/// How long an awaiting history ([`on_stored_shard_awaited`]) may run on its shard before the test fails
/// ([`crate::test_store::HISTORY_PATIENCE`]).
const HISTORY_PATIENCE: std::time::Duration = crate::test_store::HISTORY_PATIENCE;

/// [`on_stored_shard_with`] for a history that awaits (main 01a20182's order of 2026-11-25, the shard-blocking
/// allow-list): `history` runs as a task on the stored daemon's first shard ([`crate::test_store::run_on_first`]), with
/// git repositories `names` made first from the test's thread, and reads and changes the shard's state between its awaits
/// through [`crate::state::with_state`]; its pushes land on the store by awaiting ([`answered_awaited`]), never holding
/// the shard through the store's commit, as the synchronous driver's `carry_blocking` did (removed with it, 2026-11-25).
pub(crate) fn on_stored_shard_awaited<T, F, const N: usize>(
  names: [&'static str; N],
  history: impl FnOnce([RepositoryId; N]) -> F + Clone + Send + 'static,
) -> T
where
  T: Send + 'static,
  F: std::future::Future<Output = T> + 'static,
{
  on_stored_shard_configured_awaited(names, |_| {}, history)
}

/// [`on_stored_shard_awaited`] with the daemon's configuration changed by `configure` first.
pub(crate) fn on_stored_shard_configured_awaited<T, F, const N: usize>(
  names: [&'static str; N],
  configure: impl FnOnce(&mut crate::DaemonConfig),
  history: impl FnOnce([RepositoryId; N]) -> F + Clone + Send + 'static,
) -> T
where
  T: Send + 'static,
  F: std::future::Future<Output = T> + 'static,
{
  let (store, daemon) = crate::test_store::audit_daemon_stored_configured(configure);
  crate::test_store::keep_landed(&daemon);
  let made = names.map(|name| git_repository_on(&daemon, name));
  let answer = crate::test_store::run_on_first(&daemon, move || history(made), HISTORY_PATIENCE);
  daemon.stop();
  drop(store);
  answer
}

/// [`on_stored_shard_awaited`] for a history that wants the repository's root green too: `asked` made first from the
/// test's thread (`test_store::create_on`), then `history` run as a task of the shard with its id and green.
pub(crate) fn on_stored_shard_made_awaited<T, F>(
  asked: crate::test_store::Asked<'static>,
  history: impl FnOnce((RepositoryId, slates_db::catalog::VolumeId)) -> F + Clone + Send + 'static,
) -> T
where
  T: Send + 'static,
  F: std::future::Future<Output = T> + 'static,
{
  let (store, daemon) = crate::test_store::audit_daemon_stored();
  crate::test_store::keep_landed(&daemon);
  let made = crate::test_store::create_on(&daemon, 0, &owner(), asked);
  let answer = crate::test_store::run_on_first(&daemon, move || history(made), HISTORY_PATIENCE);
  daemon.stop();
  drop(store);
  answer
}

/// The shard's state for `f`, in an awaiting history ([`on_stored_shard_awaited`]).
pub(crate) fn on_state<T>(f: impl FnOnce(&mut ShardState) -> T) -> T {
  crate::state::with_state(f).expect("the shard's state")
}

/// A push driven as an edge's task drives it: `request` a piece at a time, then its end, then its answer a step at a
/// time, yielding after each turn and carrying what the push asks off the step ([`carry_awaited`]): what the driver
/// wrote, and what it received.
pub(crate) async fn answered_awaited(
  push: &mut ReceivePack,
  request: &[u8],
) -> (Vec<u8>, Received) {
  for piece in request.chunks(PIECE) {
    on_state(|state| push.input(state, piece)).expect("taken");
  }
  on_state(|state| push.end(state)).expect("ended");
  let mut answer = Vec::new();
  loop {
    match on_state(|state| push.answer_step(state, &mut answer)).expect("answered") {
      Answering::Answered(received) => return (answer, *received),
      Answering::Turned => {
        slates_rt::futures::yield_now().await;
        carry_awaited(push).await.expect("carried");
      }
      #[cfg(unix)]
      step @ Answering::Hook(_) => panic!("no hook runs where no view is shown: {step:?}"),
    }
  }
}

/// Drops `push` where the shard's state is lent, as a synchronous step drops it ([`pushed_awaited`]).
pub(crate) fn dropped_in_a_step(push: ReceivePack) {
  on_state(|_| drop(push));
}

/// `request` pushed to `repository` by its owner from a task of its shard ([`answered_awaited`]). The push is dropped
/// where the shard's state is lent, as a synchronous step dropped it, so what a push's end starts
/// through the state (a derivation behind it, its owed let-gos) is not started: a test that measures a derivation
/// drives it itself.
pub(crate) async fn pushed_awaited(repository: RepositoryId, request: &[u8]) -> Received {
  let (_, mut push) = on_state(|state| started(state, repository));
  let (_, received) = answered_awaited(&mut push, request).await;
  dropped_in_a_step(push);
  received
}

/// `request` pushed to `repository` by its owner from a task of `daemon`'s first shard ([`pushed_awaited`]), the test's
/// thread waiting for its answer: what was received.
pub(crate) fn pushed_on(
  daemon: &crate::Daemon,
  repository: RepositoryId,
  request: &[u8],
) -> Received {
  let request = request.to_vec();
  crate::test_store::run_on_first(
    daemon,
    move || async move { pushed_awaited(repository, &request).await },
    HISTORY_PATIENCE,
  )
}

/// The fixtures' scenarios `names` pushed from a task of `daemon`'s first shard
/// ([`pushed_from_the_fixtures_awaited`]), the test's thread waiting for it.
pub(crate) fn pushed_from_the_fixtures_on(
  daemon: &crate::Daemon,
  repository: RepositoryId,
  names: &'static [&'static str],
) {
  crate::test_store::run_on_first(
    daemon,
    move || async move { pushed_from_the_fixtures_awaited(repository, names).await },
    HISTORY_PATIENCE,
  );
}

/// Pushes the fixtures' scenarios `names` in order to `repository` ([`pushed_awaited`]), each answered `ok`.
pub(crate) async fn pushed_from_the_fixtures_awaited(repository: RepositoryId, names: &[&str]) {
  let mut taken = Vec::new();
  for (scenario, _) in chain() {
    if names.contains(&scenario.name) {
      let received = pushed_awaited(repository, scenario.request).await;
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
  on_stored_shard_awaited(["fixtures"], |[repository]| async move {
    let mut after = "";
    for (scenario, next) in chain() {
      let (advertised, mut push) = on_state(|state| started(state, repository));
      let expected = advertisement(scenario.before, &OURS);
      assert_eq!(advertised, expected, "{}: the advertisement", scenario.name);
      let (answer, _) = answered_awaited(&mut push, scenario.request).await;
      assert_eq!(
        answer,
        gits_report(&scenario),
        "{}: the answer",
        scenario.name
      );
      if scenario.name == "create" {
        let first = vec![("text".to_owned(), text(false))];
        assert_eq!(on_state(|state| files(state, repository, "main")), first);
      }
      after = next;
    }
    on_state(|state| {
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
  });
}

/// A-645 P3 (main's ruling 1): with a `pre-receive` hook configured, `delete` is refused in words naming the root mount
/// before this daemon has published it, and `hook not run` once it has (nothing yet shows the hook's view); each names
/// the hook and records nothing past `held`.
async fn assert_a_configured_hook_refuses(repository: RepositoryId, delete: &[u8], held: usize) {
  let received = pushed_awaited(repository, delete).await;
  assert_eq!(reasons(&received), [ROOT_UNPUBLISHED]);
  assert_first_line_names(&received, b"pre-receive");
  on_state(|state| {
    assert_eq!(records(state, repository), held);
    state.hook_views =
      crate::vcs_store::svn_presenter::HookViews::Given(std::path::PathBuf::from("/slates-root"));
  });
  let received = pushed_awaited(repository, delete).await;
  assert_eq!(reasons(&received), [HOOK_NOT_RUN]);
  assert_first_line_names(&received, b"pre-receive");
  assert_eq!(on_state(|state| records(state, repository)), held);
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
  on_stored_shard_awaited(["policy"], |[repository]| async move {
    pushed_from_the_fixtures_awaited(repository, &["create", "update", "copy"]).await;
    let held = on_state(|state| records(state, repository));
    let copy: &[u8] = b"refs/heads/copy";
    let delete = request(&[(SECOND, "", copy)], CAPABILITIES, &[]);
    let (advertised, mut push) = on_state(|state| {
      set_policy(
        state,
        "policy",
        0,
        (vec![deny(PolicyRight::Read, "refs/heads/copy")], Vec::new()),
      );
      started(state, repository)
    });
    let main_alone = format!("{SECOND} refs/heads/main\n");
    assert_eq!(advertised, advertisement(&main_alone, &OURS));
    let (_, received) = answered_awaited(&mut push, &delete).await;
    assert_eq!(reasons(&received), [HIDDEN_DELETE]);
    on_state(|state| {
      set_policy(
        state,
        "policy",
        1,
        (
          vec![deny(PolicyRight::Delete, "refs/heads/copy")],
          Vec::new(),
        ),
      );
    });
    let received = pushed_awaited(repository, &delete).await;
    let prohibited = Outcome::Ng(protected::deletion_prohibited(copy));
    assert_eq!(received.outcomes, [prohibited]);
    let other: &[u8] = b"refs/heads/other";
    let create = request(&[("", SECOND, other)], CAPABILITIES, empty_pack());
    on_state(|state| {
      set_policy(
        state,
        "policy",
        2,
        (
          vec![deny(PolicyRight::Submit, "refs/heads/other")],
          Vec::new(),
        ),
      );
    });
    assert_declined_as_a_hook(&pushed_awaited(repository, &create).await, other, b"rule 0");
    on_state(|state| set_policy(state, "policy", 3, (Vec::new(), vec![pre_receive()])));
    assert_a_configured_hook_refuses(repository, &delete, held).await;
    on_state(|state| {
      assert!(bound(state, repository, "copy") && !bound(state, repository, "other"));
      set_policy(state, "policy", 4, (Vec::new(), Vec::new()));
    });
    assert_eq!(reasons(&pushed_awaited(repository, &delete).await), [b"ok"]);
    on_state(|state| {
      assert!(!bound(state, repository, "copy"));
      assert_eq!(records(state, repository), held + 1);
    });
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
  on_stored_shard_awaited(["atomic"], |[repository]| async move {
    pushed_from_the_fixtures_awaited(repository, &["create", "update", "tag", "copy"]).await;
    let held = on_state(|state| records(state, repository));
    let stale = [(SECOND, "", COPY_REF), (FIRST, "", TAG_REF)];
    let received = pushed_awaited(repository, &request(&stale, ATOMIC, &[])).await;
    assert_eq!(reasons(&received), [ATOMIC_FAILURE, FAILED_TO_UPDATE]);
    atomic_refused_whole_at_its_append(repository, held).await;
    let received = pushed_awaited(repository, &request(&stale, CAPABILITIES, &[])).await;
    assert_eq!(reasons(&received), [b"ok".as_slice(), FAILED_TO_UPDATE]);
    on_state(|state| {
      assert_eq!(records(state, repository), held + 1);
      assert!(!bound(state, repository, "copy") && bound(state, repository, "refs/tags/v1"));
    });
    atomic_applied_as_one_entry(repository, held + 1).await;
  });
}

/// Shape: the turns the test yields at most while the shard's task maps a landing: far past the few its quantum needs.
const MAPPING_TURNS: u32 = 10_000;

/// P2 piece 2 (CLAUDE.md §3: a step over user-scaled data is bounded; the landing map, `Repository::bringer_of`). Do:
/// push the fixtures' `create` pack with `atomic`, making `main` and `other` at its commit in one landing. Expect: both
/// `ok`; the landing left to be mapped after its step, then mapped by the shard's task in its own turns (none left
/// unsettled, the store's `landing_mapped` moved), its task's slot given back.
#[test]
fn a_landing_of_several_refs_is_mapped_by_the_shards_task_after_its_step() {
  on_stored_shard_awaited(["settle"], |[repository]| async move {
    let created = scenario!("create").request;
    let pack = &created[created.windows(4).position(|four| four == b"PACK").unwrap()..];
    let both = [
      ("", FIRST, b"refs/heads/main".as_slice()),
      ("", FIRST, b"refs/heads/other".as_slice()),
    ];
    let received = pushed_awaited(repository, &request(&both, ATOMIC, pack)).await;
    assert_eq!(reasons(&received), [b"ok", b"ok"]);
    let settled = |state: &mut ShardState| {
      let store = state.vcs_stores.git_ref(repository).map(|(_, store)| store);
      store.map(|store| {
        (
          store.repository.landings_unsettled(),
          store.repository.counters().landing_mapped,
        )
      })
    };
    let mut turns = 0;
    while on_state(settled).is_some_and(|(unsettled, _)| unsettled) {
      turns += 1;
      assert!(turns < MAPPING_TURNS, "the landing was never mapped");
      slates_rt::futures::yield_now().await;
    }
    let (_, mapped) = on_state(settled).unwrap();
    assert!(mapped > 0, "the task mapped no position");
    assert!(on_state(|state| !state.vcs_stores.settles(repository)));
  });
}

/// P2 piece 3 (CLAUDE.md §3: a step over user-scaled data is bounded; `slates_git::store::Shedding`). Do: push the
/// fixtures' `create`, its objects in its pack. Expect: `ok`; its maps by object given up by its landing and freed by
/// the shard's task after its step (`vcs_store.landed.shed_entries` moved by three entries an object at least: its
/// place, its pack entry and its position; none left queued, the task ended).
#[test]
fn a_landed_pushs_maps_by_object_are_freed_by_the_shards_task_after_its_step() {
  on_stored_shard_awaited(["shed"], |[repository]| async move {
    let created = scenario!("create");
    let received = pushed_awaited(repository, created.request).await;
    assert_eq!(reasons(&received), [b"ok"]);
    let shed = |state: &mut ShardState| {
      let freed = state
        .refusals
        .get(crate::vcs_store::SHED_ENTRIES)
        .copied()
        .unwrap_or(0);
      (state.vcs_stores.sheds(), freed)
    };
    let mut turns = 0;
    while on_state(shed).0 != (0, false) {
      turns += 1;
      assert!(
        turns < MAPPING_TURNS,
        "the landed push's maps were never freed"
      );
      slates_rt::futures::yield_now().await;
    }
    let positions = on_state(|state| {
      state
        .vcs_stores
        .git_ref(repository)
        .map_or(0, |(_, store)| store.repository.counters().positions)
    });
    let (_, freed) = on_state(shed);
    // Each object its place, its pack entry and its position: three entries an object at least.
    assert!(
      positions > 0 && freed >= positions.saturating_mul(3),
      "freed {freed} entries for {positions} objects"
    );
  });
}

/// Format: the ref the atomic test's fixtures push as `copy`.
const COPY_REF: &[u8] = b"refs/heads/copy";
/// Format: the tag the atomic test's fixtures push.
const TAG_REF: &[u8] = b"refs/tags/v1";

/// Branch `cli` made with the CLI's verb at `main`'s version before its first push (no commit, so git sees it unborn), then, with `atomic`, the tag deleted and `cli` created at a commit held:
/// refused whole at its append, git's line naming `cli`, the tag still bound and the `held` records unchanged.
async fn atomic_refused_whole_at_its_append(repository: RepositoryId, held: usize) {
  let reply = on_state(|state| {
    dispatch(
      state,
      1,
      &owner(),
      RequestBody::CreateBranch {
        repository: "atomic".to_owned(),
        name: "cli".to_owned(),
        from: "main".to_owned(),
        version: Some(0),
      },
    )
  });
  assert!(matches!(reply, ReplyBody::GreenCreated { .. }), "{reply:?}");
  let cli = [
    (TAG, "", TAG_REF),
    ("", SECOND, b"refs/heads/cli".as_slice()),
  ];
  let received = pushed_awaited(repository, &request(&cli, ATOMIC, empty_pack())).await;
  assert_eq!(
    reasons(&received),
    [ATOMIC_TRANSACTION_FAILED, ATOMIC_TRANSACTION_FAILED]
  );
  assert_first_line_names(&received, b"refs/heads/cli");
  on_state(|state| {
    assert_eq!(records(state, repository), held);
    assert!(bound(state, repository, "copy") && bound(state, repository, "refs/tags/v1"));
  });
}

/// With `atomic`, the tag deleted and `again` created at a commit held: both applied as one entry past the `held`
/// records, `again` holding the second commit's text.
async fn atomic_applied_as_one_entry(repository: RepositoryId, held: usize) {
  let again = [
    (TAG, "", TAG_REF),
    ("", SECOND, b"refs/heads/again".as_slice()),
  ];
  let received = pushed_awaited(repository, &request(&again, ATOMIC, empty_pack())).await;
  assert_eq!(reasons(&received), [b"ok", b"ok"]);
  on_state(|state| {
    assert_eq!(records(state, repository), held + 1);
    assert!(!bound(state, repository, "refs/tags/v1"));
    assert_hold_the_second_text(state, repository, &["again"]);
  });
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
  on_stored_shard_awaited(["bindings"], |[repository]| async move {
    pushed_from_the_fixtures_awaited(repository, &["create"]).await;
    let (green, members) = on_state(|state| {
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
      (green, members)
    });
    let update = scenario!("update").request;
    let pack = &update[update
      .windows(4)
      .position(|magic| magic == b"PACK")
      .unwrap()..];
    let moved = [(FIRST, SECOND, b"refs/heads/topic".as_slice())];
    let received = pushed_awaited(repository, &request(&moved, CAPABILITIES, pack)).await;
    assert_eq!(reasons(&received), [b"ok"]);
    on_state(|state| {
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
  });
}

/// A-722 part 4 (a record's replay a unit a turn, its pack's waiting deltas resolved a delta a unit). Do: push the
/// fixtures' `create` and `update` (a thin pack, its delta's base outside it); make the store again from the log, each
/// turn held to no time at all, counting the turns. Expect: more turns than records; the refs advertised after as
/// before.
#[test]
fn t_a722_p4_a_thin_packs_record_is_replayed_in_turns_with_its_deltas_resolved() {
  on_stored_shard_awaited(["thin-replayed"], |[repository]| async move {
    pushed_from_the_fixtures_awaited(repository, &["create", "update"]).await;
    on_state(|state| {
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
  on_stored_shard_awaited(["thin-first"], |[repository]| async move {
    pushed_from_the_fixtures_awaited(repository, &["create"]).await;
    let received = pushed_awaited(repository, &update_with_its_delta_first()).await;
    assert!(
      reasons(&received).iter().all(|reason| reason == b"ok"),
      "{:?}",
      received.outcomes
    );
    on_state(|state| {
      let before = super::advertised_refs(state, repository);
      crate::vcs_store::rebuild(state, repository);
      while crate::vcs_store::rebuild_turn_once_within(state, repository, 0) {}
      crate::vcs_store::paths::whole_build::settle(state);
      assert_eq!(super::advertised_refs(state, repository), before);
    });
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
  let refused = on_stored_shard_configured_awaited(
    ["chains"],
    |config| config.caps.green_chain_bytes = SMALL_CHAIN_BUDGET,
    |[repository]| async move {
      let pack = pack_of(scenario!("create").request);
      let mut refused = Vec::new();
      for k in 0..CREATES {
        let name = format!("refs/heads/p{k}");
        let created = [("", FIRST, name.as_bytes())];
        let received = pushed_awaited(repository, &request(&created, CAPABILITIES, pack)).await;
        if reasons(&received) != [b"ok".to_vec()]
          || !on_state(|state| bound(state, repository, &format!("p{k}")))
        {
          if k == 1 {
            eprintln!("DBG {:?}", received.outcomes);
          }
          refused.push(k);
        }
      }
      let first = vec![("text".to_owned(), text(false))];
      let wrong = on_state(|state| {
        (0..CREATES)
          .filter(|k| !refused.contains(k))
          .filter(|k| files(state, repository, &format!("p{k}")) != first)
          .count()
      });
      (refused, wrong)
    },
  );
  assert_eq!(refused, (Vec::new(), 0));
}

/// Shape: the greens the smaller repository of
/// [`t_goal93_a_pushs_search_for_the_green_holding_a_commit_is_flat_in_the_repositorys_greens`] holds before its push.
const HELD_FEW: usize = 4;

/// Shape: the greens the larger repository holds: four times [`HELD_FEW`], so work linear in the greens shows as four
/// times as much.
const HELD_MANY: usize = 4 * HELD_FEW;

/// Shape: the creates the one push makes, each at a commit no green holds yet.
const CREATES_UNHELD: usize = 4;

/// The greens a push's search for the green holding a commit examined (`HOLDING_EXAMINED`), in a repository holding
/// `held` greens at the fixtures' first commit, over one push creating [`CREATES_UNHELD`] branches at its second commit,
/// which no green holds; and each create's outcome.
async fn examined_over(repository: RepositoryId, held: usize) -> (u64, Vec<Vec<u8>>) {
  let create = pack_of(scenario!("create").request);
  for k in 0..held {
    let branch = if k == 0 {
      "main".to_owned()
    } else {
      format!("held{k}")
    };
    let named = format!("refs/heads/{branch}");
    let created = [("", FIRST, named.as_bytes())];
    let received = pushed_awaited(repository, &request(&created, CAPABILITIES, create)).await;
    assert_eq!(reasons(&received), [b"ok".to_vec()], "{branch}");
  }
  let names: Vec<Vec<u8>> = (0..CREATES_UNHELD)
    .map(|k| format!("refs/heads/unheld{k}").into_bytes())
    .collect();
  let commands: Vec<(&str, &str, &[u8])> = names
    .iter()
    .map(|name| ("", SECOND, name.as_slice()))
    .collect();
  let update = pack_of(scenario!("update").request);
  let examined = || {
    on_state(|state| {
      state
        .refusals
        .get(super::HOLDING_EXAMINED)
        .copied()
        .unwrap_or(0)
    })
  };
  let before = examined();
  let received = pushed_awaited(repository, &request(&commands, CAPABILITIES, update)).await;
  (examined() - before, reasons(&received))
}

/// Goal 93 (main 01a20182's order: git/git's push spent its time in `ShardGreens::holding`, which lent every member
/// green and read every one's versions for each decision). Do: in one repository holding [`HELD_FEW`] greens at the
/// first commit and another holding [`HELD_MANY`], push [`CREATES_UNHELD`] creates at the second commit, which no green
/// holds, so each create asks which green holds its commit and finds none. Expect: every create `ok`, and the greens
/// the search examined the same in both repositories: what a create's search costs follows the greens holding its
/// commit, never the repository's greens. Red before the change: the search examined every member green for each create,
/// four times as many in the larger repository.
#[test]
fn t_goal93_a_pushs_search_for_the_green_holding_a_commit_is_flat_in_the_repositorys_greens() {
  let (few, many) = on_stored_shard_awaited(["few", "many"], |[few, many]| async move {
    let few = examined_over(few, HELD_FEW).await;
    let many = examined_over(many, HELD_MANY).await;
    (few, many)
  });
  let created = vec![b"ok".to_vec(); CREATES_UNHELD];
  assert_eq!((&few.1, &many.1), (&created, &created));
  assert_eq!(few.0, many.0, "the search followed the repository's greens");
}

/// A-658 3c's measure first (main 01a20182's ruling of 2026-11-27 22:48): a push's judging on its owner's shard is timed,
/// so a bench can read what share of the owner's push work it is. Do: push one create of `main`. Expect it `ok`, and
/// `git_push.judge_ns` and `git_push.judge_ns_most` both moved, the most no more than the sum: the counter is not dead.
#[test]
fn t_a658_3c_a_pushs_judging_is_timed_on_its_owner_shard() {
  let ((sum, most), received) = on_stored_shard_awaited(["judged"], |[repository]| async move {
    let read =
      |name: &'static str| on_state(move |state| state.refusals.get(name).copied().unwrap_or(0));
    let before = (read(super::JUDGE_NS), read(super::JUDGE_NS_MOST));
    let create = pack_of(scenario!("create").request);
    let created = [("", FIRST, b"refs/heads/main".as_slice())];
    let received = pushed_awaited(repository, &request(&created, CAPABILITIES, create)).await;
    let after = (read(super::JUDGE_NS), read(super::JUDGE_NS_MOST));
    (
      (after.0 - before.0, after.1.max(before.1)),
      reasons(&received),
    )
  });
  assert_eq!(received, [b"ok".to_vec()], "the create landed");
  assert!(sum > 0, "the push's judging was timed: {sum} ns");
  assert!(
    most > 0 && most <= sum,
    "the most within the sum: {most} of {sum} ns"
  );
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
  let repository = git_repository_on(&daemon, "roomy");
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
      // A push a task of the shard, as a client's push is its own task.
      let answered = reasons(&pushed_on(&daemon, repository, &pushing));
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
/// Expect: the one version derived, in at most [`PACED_TURNS`] turns besides one per span of the derivation's wall time
/// (the step's ceiling less a quantum, what a turn runs to) and one per turn that yielded to a waiter (counted): a
/// derivation runs while nobody waits (A-783), never a few units a turn, so its turns follow its time, whatever the
/// machine's load (a loaded host's long turns do less each; main's ruling of 2026-11-21 08:20, the rule stated rather
/// than the machine's speed). Red before the change: hundreds of turns. Main's TSan lane (run 45527257702) failed the
/// older bound, ceilings alone: 19 turns over 962,882,161 ns at a 100,000,000 ns ceiling.
#[test]
fn t_a614a_a_long_historys_derivation_takes_turns_by_the_shards_pace_not_its_objects() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let repository = git_repository_on(&daemon, "paced");
  paced_line_pushed_on(&daemon, repository);
  let before = crate::daemon::observe_first(&daemon, derive_counts);
  let paced = derived_for_read(&daemon, repository);
  let after = crate::daemon::observe_first(&daemon, derive_counts);
  daemon.stop();
  assert_paced(&paced, (before, after));
}

/// The derivation's counters: its turns, the versions it derived and the turns that yielded to a waiter.
type DeriveCounts = (u64, u64, u64);

/// The shard's [`DeriveCounts`] now.
fn derive_counts(state: &mut ShardState) -> DeriveCounts {
  use crate::vcs_store::git_derive::{DERIVED, TURNS, TURNS_YIELDED};
  let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
  (count(TURNS), count(DERIVED), count(TURNS_YIELDED))
}

/// A line of [`PACED_HISTORY`] commits pushed as one create to `repository`, made beforehand from the test's thread
/// ([`git_repository_on`]), from a task of `daemon`'s first shard ([`pushed_on`]): the pack it landed.
fn paced_line_pushed_on(daemon: &crate::Daemon, repository: RepositoryId) -> Vec<u8> {
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
  let received = pushed_on(daemon, repository, &request(&created, CAPABILITIES, &pack));
  assert_eq!(reasons(&received), [b"ok".to_vec()]);
  pack
}

/// The derivation's time and turns as this shard counted them.
fn derive_time(state: &mut ShardState) -> (u64, u64) {
  use crate::vcs_store::git_derive::{NS, TURNS};
  let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
  (count(NS), count(TURNS))
}

/// Main 01a20182's order of 2026-11-25 (a first clone's first pack byte spends about a third of its time deriving its
/// tags, told so far only by difference). Do: push a line of [`PACED_HISTORY`] commits and derive it as a fetch's
/// request does; then derive it again, nothing left due. Expect: the derivation's time counted
/// (`vcs_store.git_derive.ns`) with its turns; and the second, which turns no derivation, counts neither time nor a turn
/// (every encoding turn asks first: counted, 63,153 such asks in one clone read as a derivation spinning). Red before:
/// no time counted, and the second counted a turn.
#[test]
fn a_derivations_own_time_is_counted_and_a_call_with_nothing_due_counts_nothing() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let repository = git_repository_on(&daemon, "derive-time");
  paced_line_pushed_on(&daemon, repository);
  let before = crate::daemon::observe_first(&daemon, derive_time);
  derived_for_read(&daemon, repository);
  let derived = crate::daemon::observe_first(&daemon, derive_time);
  derived_for_read(&daemon, repository);
  let again = crate::daemon::observe_first(&daemon, derive_time);
  daemon.stop();
  assert!(
    derived.0 > before.0 && derived.1 > before.1,
    "time and turns counted: {before:?} → {derived:?}"
  );
  assert_eq!(again, derived, "nothing due, nothing counted");
}

/// A request creating `main` at a line of [`PACED_HISTORY`] commits, its pack whole.
fn paced_line_request() -> Vec<u8> {
  let mut objects = Vec::new();
  let mut parent = None;
  for at in 0..PACED_HISTORY {
    let (commit, made) = flat_commit(1, &format!("line {at}"), parent);
    objects.extend(made);
    parent = Some(commit);
  }
  let head = parent.unwrap().to_string();
  let created = [("", head.as_str(), b"refs/heads/main".as_slice())];
  request(&created, CAPABILITIES, &pack_of_objects(&objects))
}

/// The shard's derivation counts: versions derived, and derivations begun behind a push.
fn derived_and_behind(state: &mut ShardState) -> (u64, u64) {
  use crate::vcs_store::git_derive::{BEHIND, DERIVED};
  let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
  (count(DERIVED), count(BEHIND))
}

/// Waits, a look a while, until `daemon`'s shard has derived at least `at_least` versions: whether it did in time.
fn derived_by(daemon: &crate::Daemon, at_least: u64) -> bool {
  let began = std::time::Instant::now();
  while crate::daemon::observe_first(daemon, derived_and_behind).0 < at_least {
    if began.elapsed() > std::time::Duration::from_secs(60) {
      return false;
    }
    std::thread::yield_now();
  }
  true
}

/// Main 01a20182's order of 2026-11-25 (a first clone after git/git's push spent about a third of its wait for its first
/// pack byte deriving the versions the push left unknown). Do: push a line of [`PACED_HISTORY`] commits as one create,
/// a step at a time as the edge pushes, so the push ends with the shard lent to it; wait for the shard to derive; then
/// derive as the first fetch's request does. Expect: a derivation begun behind the push as it ended
/// (`vcs_store.git_derive.behind`) and the version derived then; and the fetch's own derivation deriving nothing. Red
/// before: nothing derived until the fetch, which derived the version itself.
#[test]
fn a_push_leaves_its_versions_derived_behind_it_so_the_first_fetch_derives_nothing() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let repository = git_repository_on(&daemon, "derived-behind");
  let (created, _) = pushed_in_steps(&daemon, repository, &paced_line_request()).unwrap();
  let behind_in_time = derived_by(&daemon, 1);
  let before_fetch = crate::daemon::observe_first(&daemon, derived_and_behind);
  derived_for_read(&daemon, repository);
  let after_fetch = crate::daemon::observe_first(&daemon, derived_and_behind);
  daemon.stop();
  assert_eq!(reasons(&created), [b"ok"]);
  assert!(
    behind_in_time && before_fetch.1 >= 1,
    "derived behind the push: {before_fetch:?}"
  );
  assert_eq!(
    after_fetch.0, before_fetch.0,
    "the first fetch derived nothing: {before_fetch:?} → {after_fetch:?}"
  );
}

/// Shape: the commits of [`tagged_line_request`]'s line, and every second one tagged: enough versions that their
/// derivation takes many turns, so a fetch arrives while it is under way.
const DERIVE_TAGGED_LINE: usize = 300;

/// A request creating `main` at a line of [`DERIVE_TAGGED_LINE`] commits and a lightweight tag at every second one, each tag a
/// version of its own to derive.
fn tagged_line_request() -> Vec<u8> {
  let mut objects = Vec::new();
  let mut commits = Vec::new();
  let mut parent = None;
  for at in 0..DERIVE_TAGGED_LINE {
    let (commit, made) = flat_commit(1, &format!("tagged line {at}"), parent);
    objects.extend(made);
    commits.push(commit.to_string());
    parent = Some(commit);
  }
  let names: Vec<String> = (0..DERIVE_TAGGED_LINE / 2)
    .map(|at| format!("refs/tags/v{at}"))
    .collect();
  let head = commits.last().expect("a head").clone();
  let mut created: Vec<(&str, &str, &[u8])> =
    vec![("", head.as_str(), b"refs/heads/main".as_slice())];
  for (at, name) in names.iter().enumerate() {
    created.push(("", commits[2 * at].as_str(), name.as_bytes()));
  }
  request(&created, CAPABILITIES, &pack_of_objects(&objects))
}

/// The derivation as this shard counted it: a reader's turns, the task behind's, handed over, and its nanoseconds.
fn derive_driven(state: &mut ShardState) -> [u64; 4] {
  use crate::vcs_store::git_derive::{BEHIND_HANDED, BEHIND_TURNS, NS, READ_TURNS};
  let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
  [
    count(READ_TURNS),
    count(BEHIND_TURNS),
    count(BEHIND_HANDED),
    count(NS),
  ]
}

/// A fetch's derivation of [`tagged_line_request`]'s repository `name` on `daemon`, the push made `behind` as the edge
/// makes it (a task of the shard, so a derivation begins behind it as it ends) or not (in a step, so none does): the
/// fetch's read turns, the hand-overs, and its own time, timed on its shard. Behind a push, the read arrives
/// mid-derivation by the shard's own state ([`read_on_the_shard`]), never by how soon the test's thread runs after the
/// push: main 01a20182's 8a loop of 2026-11-25 found the task behind done before the thread's read arrived (0 turns,
/// 0 handed). Red: with the test's thread held 3 s after the push, the thread's read arrived after it the same way
/// (0 turns, 0 handed); the read waiting on the shard was handed the derivation held or not (1 handed).
fn fetch_after_tagged_push(
  daemon: &crate::Daemon,
  name: &'static str,
  behind: bool,
) -> (u64, u64, u64) {
  let repository = git_repository_on(daemon, name);
  let request = tagged_line_request();
  let before = crate::daemon::observe_first(daemon, derive_driven);
  let read = if behind {
    // Shared by this thread and the shard's task: leaked for the test's life, so neither owns it (D-8, no `Arc`).
    let pushed: &'static std::sync::atomic::AtomicBool =
      Box::leak(Box::new(std::sync::atomic::AtomicBool::new(false)));
    let read = read_on_the_shard(daemon, repository, Some(pushed));
    let (created, _) = pushed_in_steps(daemon, repository, &request).unwrap();
    pushed.store(true, std::sync::atomic::Ordering::Release);
    assert!(reasons(&created).iter().all(|reason| reason == b"ok"));
    read
  } else {
    let said = reasons(&pushed_on(daemon, repository, &request));
    assert!(said.iter().all(|reason| reason == b"ok"));
    read_on_the_shard(daemon, repository, None)
  };
  let took_ns = read
    .recv()
    .expect("the read answered")
    .unwrap_or_else(|missed| panic!("{missed}"));
  let after = crate::daemon::observe_first(daemon, derive_driven);
  (after[0] - before[0], after[2] - before[2], took_ns)
}

/// A fetch's read of `repository` ([`git_derive::derive_for_read`]) as a task of the shard, timed there from its start to
/// its end. Given `pushed` (set once the push behind which the read must arrive has answered), it waits first, a step
/// quantum at a time, until the derivation behind that push has kept a turn under way while its task still runs, and
/// begins in that same poll, so the task behind cannot finish between the look and the read. Refused where the task
/// behind ended before a look found its derivation under way, or where none began behind a push that answered.
///
/// [`git_derive::derive_for_read`]: crate::vcs_store::git_derive::derive_for_read
fn read_on_the_shard(
  daemon: &crate::Daemon,
  repository: RepositoryId,
  pushed: Option<&'static std::sync::atomic::AtomicBool>,
) -> std::sync::mpsc::Receiver<Result<u64, String>> {
  use crate::vcs_store::git_derive::{BEHIND, behind_and_under_way, derive_for_read};
  let (done, read) = std::sync::mpsc::channel();
  crate::daemon::observe_first(daemon, move |state| {
    let begun_before = state.refusals.get(BEHIND).copied().unwrap_or(0);
    let quantum_ns = state.config.step_quantum_ns();
    let task = slates_rt::futures::spawn(async move {
      while let Some(pushed) = pushed {
        let look = crate::state::with_state_counted(|state| {
          let begun = state.refusals.get(BEHIND).copied().unwrap_or(0) > begun_before;
          (begun, behind_and_under_way(state, repository))
        });
        match look {
          Some((true, (true, true))) => break,
          Some((true, (false, _))) => {
            let _ = done.send(Err(
              "the derivation behind the push ended before a look found it under way".to_owned(),
            ));
            return;
          }
          Some((false, _)) if pushed.load(std::sync::atomic::Ordering::Acquire) => {
            let _ = done.send(Err("no derivation began behind the push".to_owned()));
            return;
          }
          _ => {
            let _ = slates_rt::futures::sleep(quantum_ns).await;
          }
        }
      }
      let began = slates_rt::futures::now_ns();
      derive_for_read(repository).await;
      let _ = done.send(Ok(slates_rt::futures::now_ns().saturating_sub(began)));
    })
    .expect("the read's task");
    slates_rt::futures::detach(task).expect("the read's task detached");
  });
  read
}

/// Main 01a20182's ruling of 2026-11-25 (derive_behind withdrawn: a clone arriving mid-derivation ran it in slivers, the
/// task behind and the fetch each yielding to the other at every unit, 721,260 turns against 338). Do: push
/// [`tagged_line_request`] as the edge does, so a derivation begins behind it, and at once derive as the first fetch's
/// request does; then the same to another repository pushed in a step, nothing behind it. Expect: the fetch handed the
/// derivation (the task behind stopped, `vcs_store.git_derive.behind_handed`), so it is the arrival mid-derivation this
/// measures; and the fetch no later than the one with nothing behind: no more of its own turns than it, but the
/// hand-over's ([`PACED_TURNS`]: its first turns meet the task behind waiting until that task's next poll stops it),
/// and no more than twice its time. Red before: the two yielding to each other, the fetch's turns many times the other's.
/// The derivation by driver as this shard counted it from its start: [turns, ns, derived] for a reader, for the task
/// behind, and in all; and the readers that left versions due.
fn by_driver(state: &mut ShardState) -> ([[u64; 3]; 3], u64) {
  use crate::vcs_store::git_derive::{
    BEHIND_TURNS, BY_DRIVER, DERIVED, NS, READ_LEFT_DUE, READ_TURNS, TURNS,
  };
  let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
  (
    [
      [count(READ_TURNS), count(BY_DRIVER[0]), count(BY_DRIVER[1])],
      [
        count(BEHIND_TURNS),
        count(BY_DRIVER[2]),
        count(BY_DRIVER[3]),
      ],
      [count(TURNS), count(NS), count(DERIVED)],
    ],
    count(READ_LEFT_DUE),
  )
}

/// Main 01a20182's order of 2026-11-25 (the clone after a push derived its units at twice the cost, 4,670 of its 5,110
/// turns the task behind's, and the counters could not say whose time or which side of the hand-over). Do: push
/// [`tagged_line_request`] as the edge does, a derivation begun behind it, and derive at once as the first fetch does.
/// Expect: the reader's and the task behind's turns, nanoseconds and versions each sum to the derivation's own
/// (`vcs_store.git_derive.{read,behind}_{ns,derived}` beside `turns`, `ns`, `derived`), whichever drove how much (the task
/// behind may finish before the reader begins), and no reader left versions due (`vcs_store.git_derive.read_left_due`). Red before: no split counted.
#[test]
fn a_derivations_turns_time_and_versions_are_counted_by_who_drove_them() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let _ = fetch_after_tagged_push(&daemon, "by-driver", true);
  let (counted, left_due) = crate::daemon::observe_first(&daemon, by_driver);
  daemon.stop();
  let [read, behind, all] = counted;
  eprintln!(
    "derive by driver [turns, ns, derived]: read {read:?}, behind {behind:?}, all {all:?}; left due {left_due}"
  );
  for at in 0..3 {
    assert_eq!(
      read[at] + behind[at],
      all[at],
      "reader and task behind sum to the whole: {counted:?}"
    );
  }
  assert!(all[2] > 0, "the derivation ran: {counted:?}");
  assert_eq!(left_due, 0, "no reader left versions due");
}

#[test]
fn a_fetch_arriving_mid_derivation_is_no_later_than_one_with_nothing_behind() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (turns, handed, took) = fetch_after_tagged_push(&daemon, "mid-derivation", true);
  let (alone_turns, _, alone_took) = fetch_after_tagged_push(&daemon, "nothing-behind", false);
  daemon.stop();
  eprintln!(
    "mid-derivation: {turns} turns, {took} ns, {handed} handed; alone: {alone_turns} turns, {alone_took} ns"
  );
  assert!(
    turns <= alone_turns + PACED_TURNS,
    "no more turns than alone, but the hand-over's: {turns} against {alone_turns}"
  );
  assert!(
    took <= 2 * alone_took,
    "no later than alone: {took} ns against {alone_took}"
  );
  assert!(
    handed >= 1,
    "the fetch arrived mid-derivation and was handed it"
  );
}

/// Lets go of every window of `pack` on this shard by hand, as a landed pack's let-go would (its home told first, each
/// window marked landed; the commit's own let-go is withdrawn, `vcs_store::LANDED_LET_GO`): the windows.
fn let_go_by_hand(state: &mut ShardState, pack: &[u8]) -> Vec<slates_vcs::ContentId> {
  let window = state
    .vcs_stores
    .content()
    .map_or(1 << 18, slates_vcs::ContentStore::window);
  let ids: Vec<slates_vcs::ContentId> =
    pack.chunks(window).map(slates_vcs::ContentId::of).collect();
  crate::vcs_store::homes::landed_pack_here(state, slates_vcs::ContentId::of(pack));
  crate::vcs_store::within_budget(state, 0, |stores| {
    let content = stores.content_mut().expect("the content store");
    for id in &ids {
      content.mark_landed(id);
    }
  });
  for id in &ids {
    let _ = crate::vcs_store::let_go_read(state, id);
  }
  ids
}

/// How many of `windows` this shard holds (not let go).
fn held_of(state: &ShardState, windows: &[slates_vcs::ContentId]) -> usize {
  let content = state.vcs_stores.content();
  windows
    .iter()
    .filter(|window| content.is_some_and(|content| !content.is_away(window)))
    .count()
}

/// Windows brought back from the node's store, as this shard counts them.
fn brought_back_count(state: &ShardState) -> u64 {
  let name = crate::vcs_store::read_through::BROUGHT_BACK;
  state.refusals.get(name).copied().unwrap_or(0)
}

/// Shape: the files of the first tree [`b1_a_thin_pushs_check_brings_its_trees_let_go_base_back_and_lands`] pushes:
/// enough that the second tree's delta against it is shorter than the tree, as git sends it.
const THIN_TREE_FILES: usize = 64;

/// Precondition B1 of the landed let-go's lift (main's order of 2026-11-26; w-git's read sweep: the push's quarantine
/// reads, `intake::Quarantined::content`, answered none on a let-go window and noted nothing, so the connectivity check
/// took the object as unreadable). Do: create `main` at a commit of a flat tree of [`THIN_TREE_FILES`] files; then
/// update it to a commit whose tree adds a file, sent as git sends a thin pack: the tree a REF_DELTA against the first
/// tree, which the pack does not carry. Drive the update as an edge does, the window each turn met brought back
/// ([`let_go_at_its_checks`]), and once its pack is read and its checks are due, let every window of the first push's
/// pack go by hand, as a landed pack's let-go does. Expect: `ok`; the check read the new tree through its base's
/// window, brought back after the let-go (`vcs_store.read_through.brought_back` moved past it). Red before: `missing
/// necessary objects`, the check's read of the tree from the quarantine answering none.
#[test]
fn b1_a_thin_pushs_check_brings_its_trees_let_go_base_back_and_lands() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (repository, update, pack) = thin_update_over_a_create(&daemon, "thin-check-away");
  let (updated, (at_let_go, after), _, _) = crate::test_store::run_on_first(
    &daemon,
    move || async move { let_go_at_its_checks(repository, &update, (&pack, Meddle::Not)).await },
    HISTORY_PATIENCE,
  )
  .unwrap();
  let refused = crate::daemon::observe_first(&daemon, bring_backs_refused);
  let one_call = crate::daemon::observe_first(&daemon, |state| {
    state
      .refusals
      .get(super::ONE_CALL_BUILDS)
      .copied()
      .unwrap_or(0)
  });
  daemon.stop();
  assert_eq!(
    reasons(&updated),
    [b"ok"],
    "{:?}; bring-backs refused, past patience, unanswered: {refused:?}",
    updated.outcomes
  );
  assert!(
    after > at_let_go,
    "the base's window brought back after its let-go: {at_let_go} -> {after}"
  );
  // B6 (main's ruling of 2026-11-27): the verdict took the build its ready prepared, never one made whole in one call.
  assert_eq!(one_call, 0, "no work built in one call");
}

/// B1's one rule keyed by the let-go met, not the window's place (main's order of 2026-11-26: the interleaving forced,
/// not met by luck in a concurrent run). Do: [`b1_a_thin_pushs_check_brings_its_trees_let_go_base_back_and_lands`]'s
/// update, but at the push's first stop on the let-go base, another reader brings that window back first (so the
/// push's driver finds it back and brings nothing), then lets it go again before the push's next step
/// ([`another_reader_brings_back_and_lets_go`]): the push meets the window let go with nothing read between. Expect:
/// `ok`, the window waited for again and brought back by the push's driver. Red with the stop keyed by the window's
/// offset (489aab9a1): `missing necessary objects`, the second let-go taken for the first met twice.
#[test]
fn b1_a_window_brought_back_and_let_go_again_by_another_reader_is_waited_for_again() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (repository, update, pack) = thin_update_over_a_create(&daemon, "thin-let-go-again");
  let (updated, (at_let_go, after), again, _) =
    crate::test_store::run_on_first(
      &daemon,
      move || async move {
        let_go_at_its_checks(repository, &update, (&pack, Meddle::LetGoAgain)).await
      },
      HISTORY_PATIENCE,
    )
    .unwrap();
  let refused = crate::daemon::observe_first(&daemon, bring_backs_refused);
  daemon.stop();
  assert!(
    again,
    "another reader brought the window back and let it go again"
  );
  assert_eq!(
    reasons(&updated),
    [b"ok"],
    "{:?}; bring-backs refused, past patience, unanswered: {refused:?}",
    updated.outcomes
  );
  assert!(
    after > at_let_go.saturating_add(1),
    "the other reader's bring-back and the push's own after it: {at_let_go} -> {after}"
  );
}

/// What this shard's bring-backs of let-go windows were refused, by its counters: refused (by the node's store, or with
/// no store: `upload::RESIDENT_REFUSED`), past their patience (`upload::BRING_BACK_PAST_PATIENCE`), and a piece the
/// store never answered (`read_through::UNANSWERED`); so a B1 test whose window was never brought back says why.
fn bring_backs_refused(state: &mut ShardState) -> (u64, u64, u64) {
  let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
  (
    count(crate::vcs_store::upload::RESIDENT_REFUSED),
    count(crate::vcs_store::upload::BRING_BACK_PAST_PATIENCE),
    count(crate::vcs_store::read_through::UNANSWERED),
  )
}

/// B1's second cause under load (main's lines of agents/w-git-b1-debug, run 15: "derivation turn cleared the
/// repository's note" between the push's stop and its driver): the push's wait is its own, not the repository's one
/// slot. Do: [`b1_a_thin_pushs_check_brings_its_trees_let_go_base_back_and_lands`]'s update, but at the push's first stop
/// on the let-go base, the repository's note of a miss is cleared before the push's driver looks, as a derivation's turn
/// clears it at its start (`git_derive::turn`). Expect: `ok`, the push's driver bringing back the window its own slot
/// noted. Red with the push's waits noted on the repository's slot (1c4ecec27): `missing necessary objects`, the driver
/// finding nothing to bring back and the walk meeting the same let-go again.
#[test]
fn b1_a_note_cleared_on_the_repository_between_a_pushs_stop_and_its_driver_leaves_the_push_its_own()
{
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (repository, update, pack) = thin_update_over_a_create(&daemon, "thin-note-cleared");
  let (updated, (at_let_go, after), cleared, _) =
    crate::test_store::run_on_first(
      &daemon,
      move || async move {
        let_go_at_its_checks(repository, &update, (&pack, Meddle::NoteCleared)).await
      },
      HISTORY_PATIENCE,
    )
    .unwrap();
  let refused = crate::daemon::observe_first(&daemon, bring_backs_refused);
  daemon.stop();
  assert!(cleared, "the repository's note cleared at the push's stop");
  assert_eq!(
    reasons(&updated),
    [b"ok"],
    "{:?}; bring-backs refused, past patience, unanswered: {refused:?}",
    updated.outcomes
  );
  assert!(
    after > at_let_go,
    "the base's window brought back after its let-go: {at_let_go} -> {after}"
  );
}

/// B1b of the landed let-go's lift (main's order of 2026-11-26; w-git's read sweep: unpack's read of an in-pack base
/// back from the quarantine). Do: on a daemon whose step budget is a nanosecond, so a resolution turn takes one unit,
/// create `main` at a commit of a flat tree of [`THIN_TREE_FILES`] files; then update it to a commit whose tree is an
/// OFS_DELTA on a second tree, itself a REF_DELTA on the first tree, which the pack does not carry. Once the second
/// tree is resolved and admitted to the quarantine, let every window of the first push's pack go by hand
/// ([`let_go_when`]): the third tree's base is then read back from the quarantine through the first tree's let-go
/// window. Expect: `ok`, that window brought back after the let-go (`vcs_store.read_through.brought_back` moved).
/// Red before: `unpacker error`, the pack refused, the read back's miss taken for a base held nowhere (unpack's
/// `DeltaBaseMissing`, its unit red `b1b_a_deltas_in_pack_base_read_back_through_a_let_go_window_stops_the_resolution_
/// until_it_is_back`).
#[test]
fn b1b_a_thin_pushs_delta_on_a_delta_whose_base_was_let_go_is_resolved_once_it_is_back() {
  let (_store, daemon) =
    crate::test_store::audit_daemon_stored_configured(|config| config.runtime.step_budget_ns = 1);
  let (repository, update, pack, middle) =
    thin_chain_update_over_a_create(&daemon, "thin-chain-away");
  let (updated, (at_let_go, after), _, _) = crate::test_store::run_on_first(
    &daemon,
    move || async move {
      let_go_when(
        repository,
        &update,
        (&pack, LetGoAt::Admitted(middle), Meddle::Not),
      )
      .await
    },
    HISTORY_PATIENCE,
  )
  .unwrap();
  let refused = crate::daemon::observe_first(&daemon, bring_backs_refused);
  daemon.stop();
  assert_eq!(
    reasons(&updated),
    [b"ok"],
    "{:?}; bring-backs refused, past patience, unanswered: {refused:?}",
    updated.outcomes
  );
  assert!(
    after > at_let_go,
    "the first tree's window brought back after its let-go: {at_let_go} -> {after}"
  );
}

/// [`thin_update_over_a_create`]'s create, then the request updating `main` to a commit whose tree adds two files, sent
/// as a chain: that tree an OFS_DELTA on a tree adding the first file, itself a REF_DELTA on the first tree, which the
/// pack does not carry. The repository, that request, the create's pack, and the middle tree's name.
fn thin_chain_update_over_a_create(
  daemon: &crate::Daemon,
  name: &str,
) -> (RepositoryId, Vec<u8>, Vec<u8>, ObjectId) {
  use slates_git::object_name::{Kind, name_of};
  let repository = git_repository_on(daemon, name);
  let (first, objects) = flat_commit(THIN_TREE_FILES, "base", None);
  let pack = pack_of_objects(&objects);
  let create = request(
    &[("", &first.to_string(), b"refs/heads/main")],
    CAPABILITIES,
    &pack,
  );
  let (created, _) = pushed_in_steps(daemon, repository, &create).unwrap();
  assert_eq!(reasons(&created), [b"ok"]);
  let base_tree = objects[1].1.clone();
  let (one, two) = (b"added\n".to_vec(), b"added again\n".to_vec());
  let mut middle = base_tree.clone();
  middle.extend_from_slice(b"100644 zz\0");
  middle.extend_from_slice(
    name_of(ObjectFormat::Sha1, Kind::Blob, &one)
      .unwrap()
      .as_bytes(),
  );
  let mut top = middle.clone();
  top.extend_from_slice(b"100644 zzz\0");
  top.extend_from_slice(
    name_of(ObjectFormat::Sha1, Kind::Blob, &two)
      .unwrap()
      .as_bytes(),
  );
  let commit = format!(
    "tree {}\nparent {first}\nauthor A <a@example.com> 1700000001 +0000\n\
     committer A <a@example.com> 1700000001 +0000\n\nadded twice\n",
    name_of(ObjectFormat::Sha1, Kind::Tree, &top).unwrap()
  )
  .into_bytes();
  let second = name_of(ObjectFormat::Sha1, Kind::Commit, &commit).unwrap();
  let thin = entries_pack(&[
    (Entry::Whole(Kind::Commit), commit),
    (
      Entry::Ref(name_of(ObjectFormat::Sha1, Kind::Tree, &base_tree).unwrap()),
      slates_git::store::delta_of(&base_tree, &middle).expect("a delta shorter than the tree"),
    ),
    (
      Entry::Ofs(1),
      slates_git::store::delta_of(&middle, &top).expect("a delta shorter than the tree"),
    ),
    (Entry::Whole(Kind::Blob), one),
    (Entry::Whole(Kind::Blob), two),
  ]);
  let update = request(
    &[(&first.to_string(), &second.to_string(), b"refs/heads/main")],
    CAPABILITIES,
    &thin,
  );
  let middle = name_of(ObjectFormat::Sha1, Kind::Tree, &middle).unwrap();
  (repository, update, pack, middle)
}

/// B2 of the landed let-go's lift (main's order of 2026-11-26; w-git's read sweep: the server's tag-headed green,
/// `ShardGreens::tree_of`, read the annotated tag at its green's head through `IndexedObjects`, and on a let-go window
/// answered no version, which the verdicts took as a ref that moved). Do: create `main` and `refs/tags/v1`, an
/// annotated tag of `main`'s commit lying in a window of its own past a filler blob ([`tag_headed_create`]); let every
/// window of that pack go and the repository go idle, its reads' cache given back; then update `refs/tags/v1` to
/// another annotated tag of the same commit, the pack that tag alone. Expect: `ok`, the old tag's window brought back
/// by the push's own driver ([`tag_headed_pushed`]: let go in the push's task, asserted let go before it reads). Red
/// before: `ng refs/tags/v1 failed to update ref` ("cannot lock ref"), the old value read as moved.
#[test]
fn b2_an_update_of_a_ref_whose_green_is_headed_by_a_let_go_tag_brings_it_back_and_lands() {
  let (updated, driven, refused) = tag_headed_pushed("tag-headed-update", |old, commit| {
    let (new, tag) = tag_object("v1 again", (commit, "commit"));
    let pack = pack_of_objects(&[(slates_git::object_name::Kind::Tag, tag)]);
    request(
      &[(&old.to_string(), &new.to_string(), b"refs/tags/v1")],
      CAPABILITIES,
      &pack,
    )
  });
  assert_eq!(
    reasons(&updated),
    [b"ok"],
    "{:?}; bring-backs refused, past patience, unanswered: {refused:?}",
    updated.outcomes
  );
  assert!(
    driven > 0,
    "the push's own driver brought the old tag's window back"
  );
}

/// B2's delete: [`b2_an_update_of_a_ref_whose_green_is_headed_by_a_let_go_tag_brings_it_back_and_lands`]'s create and
/// let-go, then a delete of `refs/tags/v1`. Expect: `ok`, the old tag's window brought back. Red before: `ng
/// refs/tags/v1`, the delete's check that its old value is its green's head reading no version.
#[test]
fn b2_a_delete_of_a_ref_whose_green_is_headed_by_a_let_go_tag_brings_it_back_and_lands() {
  let (deleted, driven, refused) = tag_headed_pushed("tag-headed-delete", |old, _| {
    request(
      &[(&old.to_string(), "", b"refs/tags/v1")],
      CAPABILITIES,
      &[],
    )
  });
  assert_eq!(
    reasons(&deleted),
    [b"ok"],
    "{:?}; bring-backs refused, past patience, unanswered: {refused:?}",
    deleted.outcomes
  );
  assert!(
    driven > 0,
    "the push's own driver brought the old tag's window back"
  );
}

/// On a stored daemon's repository `name`: [`tag_headed_create`] pushed, `ok`, and the derivation behind it ended, so
/// no write path holds or lets go its pack's windows after; then, in the next push's own task, before its first step
/// and with nothing between (main's order of 2026-11-26: B2's delete once brought nothing back under burners, the
/// window resident when it read it), every window of that pack let go by hand and the repository idle
/// (`git_encode::give_back_idle`, so no read is served from what its reads kept), the tag's window asserted let go by
/// its let-go's number; then the request `second` makes of the tag's id and its commit's, driven as an edge drives it
/// ([`let_go_when`]). What that push received, the windows its own driver brought back of ones its steps noted, and the
/// bring-backs refused.
fn tag_headed_pushed(
  name: &str,
  second: impl FnOnce(ObjectId, ObjectId) -> Vec<u8>,
) -> (Received, u64, (u64, u64, u64)) {
  pushed_over_a_let_go(name, |window| {
    let (create, pack, (tag, commit)) = tag_headed_create(window);
    ((create, pack, tag), second(tag, commit))
  })
}

/// On a stored daemon's repository `name`: the create `made` makes of the content store's window size pushed, `ok`
/// for each of its commands, and the derivation behind it ended, so no write path holds or lets go its pack's windows
/// after; then, in the next push's own task, before its first step and with nothing between, every window of that
/// pack let go by hand and the repository idle (`git_encode::give_back_idle`), the window of the object `made` names
/// asserted let go by its let-go's number; then the request `made` makes next, driven as an edge drives it
/// ([`let_go_when`]). What that push received, the windows its own driver brought back of ones its steps noted, and the
/// bring-backs refused.
fn pushed_over_a_let_go(
  name: &str,
  made: impl FnOnce(usize) -> ((Vec<u8>, Vec<u8>, ObjectId), Vec<u8>),
) -> (Received, u64, (u64, u64, u64)) {
  pushed_over_a_let_go_after(name, made, |_| {})
}

/// [`pushed_over_a_let_go`], `between` run on the repository's shard once the create's derivation has ended and before
/// the let-go (a policy set after the create it would refuse).
fn pushed_over_a_let_go_after(
  name: &str,
  made: impl FnOnce(usize) -> ((Vec<u8>, Vec<u8>, ObjectId), Vec<u8>),
  between: impl FnOnce(&mut ShardState) + Clone + Send + 'static,
) -> (Received, u64, (u64, u64, u64)) {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let repository = git_repository_on(&daemon, name);
  let window = crate::daemon::observe_first(&daemon, |state| {
    state
      .vcs_stores
      .content()
      .map(slates_vcs::ContentStore::window)
  })
  .expect("the content store");
  let ((create, pack, away), request) = made(window);
  let (created, _) = pushed_in_steps(&daemon, repository, &create).unwrap();
  assert!(
    reasons(&created).iter().all(|reason| reason == b"ok"),
    "{:?}",
    created.outcomes
  );
  assert!(
    derivation_ended(&daemon, repository),
    "the create's derivation behind it ended"
  );
  crate::daemon::observe_first(&daemon, between);
  let (received, _, _, driven) = crate::test_store::run_on_first(
    &daemon,
    move || async move {
      let number = on_state(|state| {
        let _ = let_go_by_hand(state, &pack);
        crate::vcs_store::git_encode::give_back_idle(state, repository);
        away_number_of(state, repository, &away)
      });
      assert!(
        number.is_some(),
        "the object's window let go before the push"
      );
      let_go_when(repository, &request, (&[], LetGoAt::Already, Meddle::Not)).await
    },
    HISTORY_PATIENCE,
  )
  .unwrap();
  let refused = crate::daemon::observe_first(&daemon, bring_backs_refused);
  daemon.stop();
  (received, driven, refused)
}

/// Whether repository `repository`'s derivation behind its last push has ended, within the test's patience
/// ([`HISTORY_PATIENCE`]): none behind it and none under way (`git_derive::behind_and_under_way`).
fn derivation_ended(daemon: &crate::Daemon, repository: RepositoryId) -> bool {
  let began = std::time::Instant::now();
  while crate::daemon::observe_first(daemon, move |state| {
    crate::vcs_store::git_derive::behind_and_under_way(state, repository)
  }) != (false, false)
  {
    if began.elapsed() > HISTORY_PATIENCE {
      return false;
    }
    std::thread::yield_now();
  }
  true
}

/// The let-go number of the window repository `repository`'s index places object `id` in (`ContentStore::away_number`):
/// none where it is held, or not in a pack.
fn away_number_of(state: &ShardState, repository: RepositoryId, id: &ObjectId) -> Option<u64> {
  let (content, store) = state.vcs_stores.git_ref(repository)?;
  match store.repository.stored(content, id)? {
    (_, slates_git::store::StoredAt::InPack(place), _) => {
      content.away_number(&place.pack, place.len, place.offset)
    }
    _ => None,
  }
}

/// Shape: the filler blob [`tag_headed_create`] lays between its commit and its tag, in content store windows: past
/// one, so the tag lies in a window of its own.
const FILLER_WINDOWS: usize = 2;

/// A create of `main` at a commit of one file and of `refs/tags/v1` at an annotated tag of it, the pack laid commit,
/// tree, file, a filler blob of [`FILLER_WINDOWS`] content store windows of bytes that do not compress, then the tag,
/// so the tag's window (each `window` bytes) is not its commit's: the request, its pack, and the tag's and the commit's
/// ids.
fn tag_headed_create(window: usize) -> (Vec<u8>, Vec<u8>, (ObjectId, ObjectId)) {
  use slates_git::object_name::Kind;
  let (commit, mut objects) = flat_commit(1, "tagged", None);
  objects.push((
    Kind::Blob,
    crate::vcs_store::tests::incompressible(window * FILLER_WINDOWS),
  ));
  let (tag, tag_bytes) = tag_object("v1", (commit, "commit"));
  objects.push((Kind::Tag, tag_bytes));
  let pack = pack_of_objects(&objects);
  let create = request(
    &[
      ("", &commit.to_string(), b"refs/heads/main"),
      ("", &tag.to_string(), b"refs/tags/v1"),
    ],
    CAPABILITIES,
    &pack,
  );
  (create, pack, (tag, commit))
}

/// B3 at the admission's own site (main's ruling of 2026-11-26: an admission answering ready over a swallowed typed
/// refusal is a lost error, CLAUDE.md §2 item 9, whatever re-read heals it later). Do: create `main` and
/// `refs/tags/v1`, the tag in a window of its own ([`tag_headed_create`]); once its derivation has ended, let every
/// window go and the repository idle; then run one turn of the admission of a create of `refs/tags/v2` at that tag
/// (`SealedPush:: admission_turn`), whose peel reads the tag's bytes through its let-go window. Expect: not ready, the
/// let-go noted in the push's own slot, nothing on the repository's shared one. Red before: ready, the refusal
/// swallowed, and the note left on the shared slot.
#[test]
fn b3_an_admission_turn_over_a_let_go_tag_answers_not_ready_and_notes_the_push_alone() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let repository = git_repository_on(&daemon, "admission-let-go");
  let window = crate::daemon::observe_first(&daemon, |state| {
    state
      .vcs_stores
      .content()
      .map(slates_vcs::ContentStore::window)
  })
  .expect("the content store");
  let (create, pack, (tag, _)) = tag_headed_create(window);
  let (created, _) = pushed_in_steps(&daemon, repository, &create).unwrap();
  assert_eq!(reasons(&created), [b"ok", b"ok"]);
  assert!(
    derivation_ended(&daemon, repository),
    "the create's derivation ended"
  );
  let (number, ready, shared) = crate::daemon::observe_first(&daemon, move |state| {
    let _ = let_go_by_hand(state, &pack);
    crate::vcs_store::git_encode::give_back_idle(state, repository);
    let number = away_number_of(state, repository, &tag);
    let mut sealed = crate::vcs_store::GitPush::begin(&state.vcs_stores, repository)
      .expect("a git store")
      .seal(state)
      .expect("sealed");
    let ns = state.config.step_quantum_ns();
    let waits = slates_git::away::Waits::default();
    let ready = sealed.admission_turn(
      state,
      (super::unheld_key(repository), tag),
      (&mut None, ns),
      &waits,
    );
    let shared = state
      .vcs_stores
      .git(repository)
      .and_then(|(_, store)| store.repository.peek_waiting());
    sealed.abandon(state);
    (number, (ready, waits.peek()), shared.is_some())
  });
  daemon.stop();
  let (ready, noted) = ready;
  assert!(number.is_some(), "the tag's window let go");
  assert!(!ready, "not ready over its let-go window");
  assert!(noted.is_some(), "the let-go noted in the push's own slot");
  assert!(!shared, "nothing on the repository's shared slot");
}

/// B4 of the landed let-go's lift (main's order of 2026-11-26; w-git's read sweep: the policy's reads, `store::protect`
/// `signature_refused` through `bytes_of`, refused on a let-go window, and the verdict declined the command
/// `Unjudged`). Do: from A-644's golden vectors (`crates/git/fixtures/protected`, the `signed` scenario, its commits
/// signed by the trusted key), create `refs/heads/base` at `second`, the pack laid so `second`'s and `first`'s commits
/// lie in a window apart from their trees ([`signed_base_create`]); once its derivation has ended, set a policy denying
/// unsigned commits on `main` that trusts the fixtures' key; let every window go in the next push's task; then create
/// `main` at `third`, its pack the scenario's. The policy judges `third`, then `second` and `first` through their
/// let-go window, which the command's preparation never reads (it reads `third` and `base`'s trees). Expect: `ok`,
/// every commit signed by the trusted key, the window brought back by the push's own driver. Red before: `ng
/// refs/heads/main`, "its commits cannot be read".
#[test]
fn b4_a_signed_commits_rule_reading_a_let_go_commit_brings_it_back_and_judges_it() {
  let (created, driven, refused) = pushed_over_a_let_go_after(
    "let-go-signed",
    |window| {
      let (create, pack, second) = signed_base_create(window);
      let (third, objects) = signed_third();
      let request = request(
        &[("", &third.to_string(), b"refs/heads/main")],
        CAPABILITIES,
        &pack_of_objects(&objects),
      );
      ((create, pack, second), request)
    },
    |state| signed_commits_on_main(state, "let-go-signed"),
  );
  assert_eq!(
    reasons(&created),
    [b"ok"],
    "{:?}; bring-backs refused, past patience, unanswered: {refused:?}",
    created.outcomes
  );
  assert!(driven > 0, "the push's own driver brought the window back");
}

/// Sets repository `name`'s policy: unsigned commits denied on `refs/heads/main`, the A-644 fixtures' trusted key its
/// one signer.
fn signed_commits_on_main(state: &mut ShardState, name: &str) {
  let line = include_str!("../../../git/fixtures/protected/trusted.pub");
  let field = line.split_whitespace().nth(1).expect("the key's base64");
  let blob = crate::signed_work::base64_decode(field).expect("the key's blob");
  let reply = dispatch(
    state,
    1,
    &owner(),
    RequestBody::SetRepositoryPolicy {
      repository: name.to_owned(),
      read: 0,
      rules: vec![deny(PolicyRight::UnsignedCommit, "refs/heads/main")],
      hooks: Vec::new(),
      hooks_env: Vec::new(),
      signers: vec![blob],
      hg_publishing: true,
    },
  );
  assert_eq!(reply, ReplyBody::Acknowledged);
}

/// The objects of A-644's `signed` scenario's repository before its push (`first` and `second`, signed by the trusted
/// key, their trees and blobs), as a create of `refs/heads/base` at `second` whose pack lays every tree and blob, a
/// filler blob of [`FILLER_WINDOWS`] content store windows (each `window` bytes) of bytes that do not compress, then
/// the commits, so the commits lie in no window a tree does: the request, its pack, and `second`'s id.
fn signed_base_create(window: usize) -> (Vec<u8>, Vec<u8>, ObjectId) {
  use slates_git::object_name::Kind;
  let before = include_bytes!("../../../git/fixtures/protected/signed.before.pack");
  let refs = include_str!("../../../git/fixtures/protected/signed.before.refs");
  let second = named(refs.split_whitespace().next().expect("main's id"));
  let objects = fixture_objects(before);
  let (commits, mut laid): (Vec<_>, Vec<_>) = objects
    .into_iter()
    .partition(|(kind, _)| *kind == Kind::Commit);
  laid.push((
    Kind::Blob,
    crate::vcs_store::tests::incompressible(window * FILLER_WINDOWS),
  ));
  laid.extend(commits);
  let pack = pack_of_objects(&laid);
  let create = request(
    &[("", &second.to_string(), b"refs/heads/base")],
    CAPABILITIES,
    &pack,
  );
  (create, pack, second)
}

/// A-644's `signed` scenario's pushed objects (`third`, signed by the trusted key, its tree and blob) and `third`'s id,
/// from what its `git push` sent.
fn signed_third() -> (ObjectId, Vec<(slates_git::object_name::Kind, Vec<u8>)>) {
  let sent = include_bytes!("../../../git/fixtures/protected/signed.request.bin");
  let at = sent
    .windows(4)
    .position(|magic| magic == b"PACK")
    .expect("its pack");
  let line = String::from_utf8_lossy(&sent[4..at]).into_owned();
  let third = named(line.split_whitespace().nth(1).expect("its new id"));
  (third, fixture_objects(&sent[at..]))
}

/// The objects of a fixture's `pack`, each whole: its type and bytes.
fn fixture_objects(pack: &[u8]) -> Vec<(slates_git::object_name::Kind, Vec<u8>)> {
  let unpacked = slates_git::unpack::read_whole(
    pack,
    ObjectFormat::Sha1,
    &slates_git::unpack::NoIndex,
    &mut slates_git::Admission::new(u64::MAX),
  )
  .expect("the fixture's pack");
  unpacked
    .objects
    .into_iter()
    .map(|object| (object.kind, object.content))
    .collect()
}

/// B4's linear rule at its own site (main's ruling of 2026-11-26: the rule's only byte reads are its peels of `to` and
/// of the old head, which the preparation's peel and base read heal on one push's path; the harm is another party's
/// let-go between the preparation and the verdict). Do: create `main` and `refs/tags/v1`, the tag in a window of its
/// own ([`tag_headed_create`]); once its derivation has ended, set a policy denying merge commits on `refs/tags/v1` (a
/// linear history); on the repository's shard, drive the judge of an update of `refs/tags/v1` to `main`'s commit
/// (`ServerJudge::ready`) until it answers ready, everything resident; then let the tag's window alone go and ask it
/// ready again. Expect: not ready, the let-go noted in the push's own slot and nothing on the shared one; the same
/// let-go met again with nothing brought back between, ready, its verdict the typed refusal; and the window brought
/// back, ready and its verdict `Ok`. Red before: ready over the let-go tag, the preparation done and nothing re-read,
/// its verdict declining "its commits cannot be read".
#[test]
fn b4_a_linear_rules_check_over_a_let_go_old_tag_waits_for_it_before_its_verdict() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let name = "let-go-linear";
  let repository = git_repository_on(&daemon, name);
  let window = crate::daemon::observe_first(&daemon, |state| {
    state
      .vcs_stores
      .content()
      .map(slates_vcs::ContentStore::window)
  })
  .expect("the content store");
  let (create, pack, (tag, commit)) = tag_headed_create(window);
  let (created, _) = pushed_in_steps(&daemon, repository, &create).unwrap();
  assert_eq!(reasons(&created), [b"ok", b"ok"]);
  assert!(
    derivation_ended(&daemon, repository),
    "the create's derivation ended"
  );
  crate::daemon::observe_first(&daemon, move |state| {
    set_policy(
      state,
      name,
      0,
      (
        vec![deny(PolicyRight::MergeCommit, "refs/tags/v1")],
        Vec::new(),
      ),
    );
  });
  let window_of_tag = (
    slates_vcs::ContentId::of(&pack),
    tag_window(&pack, window, &tag),
  );
  let laid = (
    slates_vcs::ContentId::of(&pack),
    u64::try_from(pack.len()).unwrap(),
    u64::try_from(window).unwrap(),
  );
  let answered = crate::test_store::run_on_first(
    &daemon,
    move || async move { judged_over_a_let_go(repository, (tag, commit), (window_of_tag, laid)).await },
    HISTORY_PATIENCE,
  );
  daemon.stop();
  let (not_ready, (noted, shared), (brought_ready, verdict), (again_ready, refused)) = answered;
  assert!(not_ready, "not ready over the let-go tag");
  assert!(noted, "the let-go noted in the push's own slot");
  assert!(!shared, "nothing on the repository's shared slot");
  assert!(brought_ready, "ready once the window is back");
  assert_eq!(verdict, Ok(()), "its verdict once the window is back");
  assert!(again_ready, "the same let-go met twice: ready");
  assert!(
    refused
      .as_ref()
      .is_err_and(|words| words.contains("its commits cannot be read")),
    "the same let-go met twice: the typed refusal: {refused:?}"
  );
}

/// The content id of the window of `pack` (`window` bytes each) the annotated tag `tag` lies in, found by its bytes.
fn tag_window(pack: &[u8], window: usize, tag: &ObjectId) -> slates_vcs::ContentId {
  let (_, bytes) = tag_object("v1", (*tag, "commit"));
  let _ = bytes;
  let found = pack
    .windows(4)
    .enumerate()
    .filter(|(_, magic)| *magic == b"tag ")
    .map(|(at, _)| at)
    .next_back()
    .expect("the tag's bytes");
  let index = found / window;
  slates_vcs::ContentId::of(pack.chunks(window).nth(index).expect("its window"))
}

/// On repository `repository`'s shard, the judge of an update of `refs/tags/v1` from `tag` to `commit` driven a ready
/// at a time over a sealed empty push, its let-go windows brought back from the push's own slot as an edge does, until
/// it answers ready; then every window of the pack `laid` names (its identity, length and window) brought back, its
/// window `away` let go and the repository idle, and ready asked; asked again with nothing brought back between, and
/// judged; then the window it noted brought back, ready asked and judged. What each answered.
async fn judged_over_a_let_go(
  repository: RepositoryId,
  (tag, commit): (ObjectId, ObjectId),
  (away, laid): (
    (slates_vcs::ContentId, slates_vcs::ContentId),
    (slates_vcs::ContentId, u64, u64),
  ),
) -> (
  bool,
  (bool, bool),
  (bool, Result<(), String>),
  (bool, Result<(), String>),
) {
  let mut judging = Judging::new(repository, (tag, commit));
  let began = std::time::Instant::now();
  while !judging.ready() {
    assert!(began.elapsed() < HISTORY_PATIENCE, "the judge never ready");
    crate::vcs_store::upload::brought_back_own(judging.waits.take()).await;
    slates_rt::futures::yield_now().await;
  }
  // The derivation behind the create let the pack go: its home told it landed once, then every window brought back,
  // so the tag's alone is let go next.
  let (_, _, window) = laid;
  on_state(|state| crate::vcs_store::homes::landed_pack_here(state, away.0));
  let (pack, pack_len) = on_state(|state| {
    let (content, store) = state.vcs_stores.git_ref(repository).expect("the git store");
    match store.repository.stored(content, &tag) {
      Some((_, slates_git::store::StoredAt::InPack(place), _)) => (place.pack, place.len),
      _ => panic!("the tag in a pack"),
    }
  });
  for index in 0..pack_len.div_ceil(window) {
    let waited = slates_git::store::packed::Waited {
      pack,
      len: pack_len,
      offset: index * window,
    };
    let _ = crate::vcs_store::read_through::bring_back_window(waited).await;
  }
  on_state(|state| let_go_window(state, repository, away));
  let not_ready = !judging.ready();
  let shared = on_state(|state| {
    state
      .vcs_stores
      .git(repository)
      .and_then(|(_, store)| store.repository.peek_waiting())
      .is_some()
  });
  let noted = judging.waits.take();
  let was_noted = noted.is_some();
  // The same let-go met again with nothing brought back between: ready, its verdict the typed refusal.
  let again_ready = judging.ready();
  let refused = judging.verdict();
  let _ = judging.waits.take();
  crate::vcs_store::upload::brought_back_own(noted).await;
  let brought_ready = judging.ready();
  let verdict = judging.verdict();
  on_state(|state| judging.sealed.take().map(|sealed| sealed.abandon(state)));
  (
    not_ready,
    (was_noted, shared),
    (brought_ready, verdict),
    (again_ready, refused),
  )
}

/// The window of repository `repository`'s pack `away` names (the pack's identity and the window's) let go by hand, as
/// a landed pack's let-go lets it go (its home told before, [`judged_over_a_let_go`]; the window marked landed), and
/// what the repository's reads keep lately given back, so no read is served from it.
fn let_go_window(
  state: &mut ShardState,
  repository: RepositoryId,
  away: (slates_vcs::ContentId, slates_vcs::ContentId),
) {
  let (_, window) = away;
  crate::vcs_store::within_budget(state, 0, |stores| {
    if let Some(content) = stores.content_mut() {
      content.mark_landed(&window);
    }
  });
  // Already let go is let go: the derivation behind the create lets go at its end the windows it brought back.
  let _ = crate::vcs_store::let_go_read(state, &window);
  let away = state
    .vcs_stores
    .content()
    .is_some_and(|content| content.is_away(&window));
  assert!(away, "the window let go");
  // What its reads keep lately given back alone (`git_encode::give_back_idle` would let the whole landed pack go too).
  if let Some((content, store)) = state.vcs_stores.git(repository) {
    let _ = store.repository.let_go_recent(content);
  }
}

/// What [`ServerJudge`] keeps across a push's steps, for one command judged by hand ([`judged_over_a_let_go`]).
struct Judging {
  repository: RepositoryId,
  command: slates_git::receive::Command,
  sealed: Option<crate::vcs_store::SealedPush>,
  hooks: super::HookVerdicts,
  admitting: Option<slates_git::store::Admitting>,
  restaging: Option<slates_git::store::Staging>,
  preparing: Option<slates_git::receive::works::Preparing>,
  peel_away: Option<u64>,
  policy_away: Option<u64>,
  making_room: Option<Vec<slates_db::catalog::VolumeId>>,
  door: Option<slates_db::catalog::VolumeId>,
  waits: slates_git::away::Waits,
}

impl Judging {
  /// The judge of an update of `refs/tags/v1` in `repository` from `tag` to `commit`, over a sealed empty push.
  fn new(repository: RepositoryId, (tag, commit): (ObjectId, ObjectId)) -> Self {
    let sealed = on_state(|state| {
      crate::vcs_store::GitPush::begin(&state.vcs_stores, repository)
        .expect("a git store")
        .seal(state)
        .expect("sealed")
    });
    Self {
      repository,
      command: slates_git::receive::Command {
        old: tag,
        new: commit,
        name: b"refs/tags/v1".to_vec(),
      },
      sealed: Some(sealed),
      hooks: super::HookVerdicts::default(),
      admitting: None,
      restaging: None,
      preparing: None,
      peel_away: None,
      policy_away: None,
      making_room: None,
      door: None,
      waits: slates_git::away::Waits::default(),
    }
  }

  /// One `ready` of the judge, made for the call over what it keeps.
  fn ready(&mut self) -> bool {
    self.with_judge(|judge, command, pushed| {
      slates_git::receive::Judge::ready(judge, command, pushed)
    })
  }

  /// The judge's verdict: `Ok` where it judged an entry, else its refusal's words.
  fn verdict(&mut self) -> Result<(), String> {
    self.with_judge(|judge, command, pushed| {
      slates_git::receive::Judge::judge(judge, command, pushed)
        .map(|_| ())
        .map_err(|declined| String::from_utf8_lossy(&declined.hook).into_owned())
    })
  }

  /// `act` over the judge made of what this keeps, on the shard's state.
  fn with_judge<T>(
    &mut self,
    act: impl FnOnce(
      &mut super::ServerJudge<'_>,
      &slates_git::receive::Command,
      &slates_git::receive::Pushed<'_>,
    ) -> T,
  ) -> T {
    let repository = self.repository;
    on_state(|state| {
      let record = super::record_of(state, repository).expect("the record");
      let pushed =
        slates_git::receive::Pushed::new(&[], None, &mut slates_git::Admission::new(u64::MAX))
          .expect("pushed");
      let mut judge = super::ServerJudge {
        state,
        push: self.sealed.as_mut().expect("the push"),
        record: &record,
        principal: &owner(),
        name: [0; 32],
        hooks: &mut self.hooks,
        admitting: &mut self.admitting,
        restaging: &mut self.restaging,
        preparing: &mut self.preparing,
        peel_away: &mut self.peel_away,
        policy_away: &mut self.policy_away,
        making_room: &mut self.making_room,
        door: &mut self.door,
        waits: &self.waits,
      };
      act(&mut judge, &self.command, &pushed)
    })
  }
}

/// B4's linear rule's healed path, kept as its net (main's ruling of 2026-11-26): on one push's path the rule's reads
/// (its peels of the new value and of the old head) meet what the preparation's peel and base read brought back. Do:
/// create `main` and `refs/tags/v1`, the tag in a window of its own ([`tag_headed_create`]); once its derivation has
/// ended, set a policy denying merge commits on `refs/tags/v1`; let every window go in the next push's task; update
/// `refs/tags/v1` to another annotated tag of `main`'s commit. Expect: `ok`.
#[test]
fn b4_a_linear_rules_update_of_a_let_go_tag_head_lands() {
  let (updated, _, refused) = pushed_over_a_let_go_after(
    "let-go-linear-net",
    |window| {
      let (create, pack, (tag, commit)) = tag_headed_create(window);
      let (new, bytes) = tag_object("v1 again", (commit, "commit"));
      let request = request(
        &[(&tag.to_string(), &new.to_string(), b"refs/tags/v1")],
        CAPABILITIES,
        &pack_of_objects(&[(slates_git::object_name::Kind::Tag, bytes)]),
      );
      ((create, pack, tag), request)
    },
    |state| {
      set_policy(
        state,
        "let-go-linear-net",
        0,
        (
          vec![deny(PolicyRight::MergeCommit, "refs/tags/v1")],
          Vec::new(),
        ),
      );
    },
  );
  assert_eq!(
    reasons(&updated),
    [b"ok"],
    "{:?}; bring-backs refused, past patience, unanswered: {refused:?}",
    updated.outcomes
  );
}

/// B5 of the landed let-go's lift (main's order of 2026-11-26; w-git's read sweep: `views::PushedObjects::bytes` of a
/// pushed object answered `Malformed` where `Push::bytes` met a let-go window, noting nothing, so the verdict's build
/// refused the command). On one push's own path the push's reads keep the pushed tree its checks resolved, so the build
/// reads it kept; it bites once the push's read cache has turned that tree over. Its red at its site: do, after
/// [`thin_update_over_a_create`]'s create, let every window of its pack go; hand a push that has read nothing the
/// update's thin pack (its new tree a REF_DELTA against the create's tree) and admit each object where it lies; read
/// the new tree through the view the verdict's build reads (`PushedObjects::tree`). Expect: not yet
/// (`Unavailable::NotYet`), the let-go noted in the push's own slot and nothing on the repository's shared one. Red
/// before: `Malformed`.
#[test]
fn b5_a_pushed_delta_read_over_a_let_go_base_is_not_yet_and_noted_for_the_push() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let repository = git_repository_on(&daemon, "pushed-delta-away");
  let (first, objects) = flat_commit(THIN_TREE_FILES, "base", None);
  let pack = pack_of_objects(&objects);
  let create = request(
    &[("", &first.to_string(), b"refs/heads/main")],
    CAPABILITIES,
    &pack,
  );
  let (created, _) = pushed_in_steps(&daemon, repository, &create).unwrap();
  assert_eq!(reasons(&created), [b"ok"]);
  assert!(
    derivation_ended(&daemon, repository),
    "the create's derivation ended"
  );
  let (thin, (tree_id, tree)) = thin_tree_entries(&objects[1].1);
  let (read, noted, shared) = crate::daemon::observe_first(&daemon, move |state| {
    let _ = let_go_by_hand(state, &pack);
    if let Some((content, store)) = state.vcs_stores.git(repository) {
      let _ = store.repository.let_go_recent(content);
    }
    let mut push = slates_git::store::Push::new(ObjectFormat::Sha1);
    crate::vcs_store::within_budget(state, 0, |stores| {
      let content = stores.content_mut().expect("the content store");
      push.extend_pack(content, &thin.0).expect("the pack put");
    });
    let naming = slates_git::store::GitNaming {
      format: ObjectFormat::Sha1,
    };
    let (content, store) = state.vcs_stores.git_ref(repository).expect("the git store");
    for (offset, kind, bytes, depth) in &thin.1 {
      let incoming = slates_git::store::Incoming {
        kind: *kind,
        claimed: None,
        content: bytes,
        depth: *depth,
      };
      push
        .admit_placed::<_, std::convert::Infallible>(content, &naming, (incoming, *offset))
        .expect("admitted");
    }
    let waits = slates_git::away::Waits::default();
    let objects = slates_git::store::views::PushedObjects::new(&store.repository, content, &push)
      .noting(&waits);
    let key = slates_merge::push::ObjectKey::new(tree_id.as_bytes()).expect("its key");
    let read = slates_merge::push::PushObjects::tree(&objects, key).map(|entries| entries.len());
    let _ = tree;
    let shared = store.repository.peek_waiting().is_some();
    (read, waits.peek().is_some(), shared)
  });
  daemon.stop();
  assert_eq!(
    read,
    Err(slates_merge::push::Unavailable::NotYet),
    "not yet over the let-go base"
  );
  assert!(noted, "the let-go noted in the push's own slot");
  assert!(!shared, "nothing on the repository's shared slot");
}

/// The thin pack of [`thin_update_over_a_create`]'s update over the create's tree `base`: its bytes and each entry's
/// offset, type, whole bytes and delta depth (the new tree a REF_DELTA against `base`, which the pack does not carry);
/// and the new tree's id and bytes.
#[allow(clippy::type_complexity)]
fn thin_tree_entries(
  base: &[u8],
) -> (
  (
    Vec<u8>,
    Vec<(u64, slates_git::object_name::Kind, Vec<u8>, u64)>,
  ),
  (ObjectId, Vec<u8>),
) {
  use slates_git::object_name::{Kind, name_of};
  let blob = b"added\n".to_vec();
  let mut tree = base.to_vec();
  tree.extend_from_slice(b"100644 zz\0");
  tree.extend_from_slice(
    name_of(ObjectFormat::Sha1, Kind::Blob, &blob)
      .unwrap()
      .as_bytes(),
  );
  let tree_id = name_of(ObjectFormat::Sha1, Kind::Tree, &tree).unwrap();
  let delta = slates_git::store::delta_of(base, &tree).expect("a delta shorter than the tree");
  let base_id = name_of(ObjectFormat::Sha1, Kind::Tree, base).unwrap();
  let entries = [
    (Entry::Ref(base_id), delta, Kind::Tree, tree.clone(), 1u64),
    (
      Entry::Whole(Kind::Blob),
      blob.clone(),
      Kind::Blob,
      blob,
      0u64,
    ),
  ];
  let laid: Vec<(Entry, Vec<u8>)> = entries
    .iter()
    .map(|(entry, data, ..)| (*entry, data.clone()))
    .collect();
  let (pack, offsets) = entries_pack_at(&laid);
  let objects = entries
    .into_iter()
    .zip(offsets)
    .map(|((_, _, kind, bytes, depth), offset)| (offset, kind, bytes, depth))
    .collect();
  ((pack, objects), (tree_id, tree))
}

/// B5's healed path, kept as its net: on one push's own path its reads keep the pushed tree its checks resolved, so the
/// verdict's build reads it kept. Do: [`b1_a_thin_pushs_check_brings_its_trees_let_go_base_back_and_lands`]'s update,
/// every window of the first push's pack let go once its verdicts are being decided ([`LetGoAt::Deciding`]). Expect:
/// `ok`.
#[test]
fn b5_a_verdicts_build_of_a_pushed_delta_over_a_let_go_base_lands() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (repository, update, pack) = thin_update_over_a_create(&daemon, "thin-verdict-away");
  let (updated, _, _, _) = crate::test_store::run_on_first(
    &daemon,
    move || async move {
      let_go_when(repository, &update, (&pack, LetGoAt::Deciding, Meddle::Not)).await
    },
    HISTORY_PATIENCE,
  )
  .unwrap();
  let refused = crate::daemon::observe_first(&daemon, bring_backs_refused);
  daemon.stop();
  assert_eq!(
    reasons(&updated),
    [b"ok"],
    "{:?}; bring-backs refused, past patience, unanswered: {refused:?}",
    updated.outcomes
  );
}

/// B3's healed path, kept as its net (main's ruling of 2026-11-26): a create at a commit a green holds is made at that
/// green (A-753), no base search read. Do: create `main` at `C2` over `C1` over `C0`, `C0`'s window apart from `C1`'s
/// ([`three_commit_create`]); let every window go in the next push's task; create `refs/heads/x` at `C1`, the empty
/// pack stock git sends for a commit the server holds. Expect: `ok`, what the push read brought back by its own driver.
#[test]
fn b3_a_create_at_a_commit_a_green_holds_lands_over_let_go_windows() {
  let (created, driven, refused) = pushed_over_a_let_go("let-go-held-create", |window| {
    let (create, pack, (zero, one)) = three_commit_create(window, Apart::Zero);
    let request = request(
      &[("", &one.to_string(), b"refs/heads/x")],
      CAPABILITIES,
      empty_pack(),
    );
    ((create, pack, zero), request)
  });
  assert_eq!(
    reasons(&created),
    [b"ok"],
    "{:?}; bring-backs refused, past patience, unanswered: {refused:?}",
    created.outcomes
  );
  assert!(
    driven > 0,
    "the push's own driver brought back what it read"
  );
}

/// B3's healed path, kept as its net: a create's search back for its base reads its commits' parents from the index's
/// rows, never their bytes. Do: [`b3_a_create_at_a_commit_a_green_holds_lands_over_let_go_windows`]'s create with
/// `C1`'s window apart from `C2`'s; let every window go; create `refs/heads/x` at a new commit over `C1`, its pack that
/// commit's objects. Expect: `ok`.
#[test]
fn b3_a_create_whose_base_search_runs_over_a_let_go_commit_lands() {
  let (created, _, refused) = pushed_over_a_let_go("let-go-base-search", |window| {
    let (create, pack, (_, one)) = three_commit_create(window, Apart::One);
    let (new, objects) = flat_commit(1, "over one", Some(one));
    let request = request(
      &[("", &new.to_string(), b"refs/heads/x")],
      CAPABILITIES,
      &pack_of_objects(&objects),
    );
    ((create, pack, one), request)
  });
  assert_eq!(
    reasons(&created),
    [b"ok"],
    "{:?}; bring-backs refused, past patience, unanswered: {refused:?}",
    created.outcomes
  );
}

/// Which of [`three_commit_create`]'s commits lies past its filler blob, in a window apart from the commits before it.
#[derive(Clone, Copy)]
enum Apart {
  /// `C0`, its window apart from `C1`'s.
  Zero,
  /// `C1` (and `C0` with it), its window apart from `C2`'s.
  One,
}

/// A create of `main` at `C2` over `C1` over `C0`, each a commit of one file, the pack laid in that order with a filler
/// blob of [`FILLER_WINDOWS`] content store windows (each `window` bytes) of bytes that do not compress before the
/// commit `apart` names: the request, its pack, and `C0`'s and `C1`'s ids.
fn three_commit_create(window: usize, apart: Apart) -> (Vec<u8>, Vec<u8>, (ObjectId, ObjectId)) {
  use slates_git::object_name::Kind;
  let (zero, under) = flat_commit(1, "zero", None);
  let (one, middle) = flat_commit(1, "one", Some(zero));
  let (two, mut objects) = flat_commit(1, "two", Some(one));
  let filler = (
    Kind::Blob,
    crate::vcs_store::tests::incompressible(window * FILLER_WINDOWS),
  );
  match apart {
    Apart::Zero => {
      objects.extend(middle);
      objects.push(filler);
    }
    Apart::One => {
      objects.push(filler);
      objects.extend(middle);
    }
  }
  objects.extend(under);
  let pack = pack_of_objects(&objects);
  let create = request(
    &[("", &two.to_string(), b"refs/heads/main")],
    CAPABILITIES,
    &pack,
  );
  (create, pack, (zero, one))
}

/// B3 of the landed let-go's lift (main's order of 2026-11-26; w-git's read sweep: the admission's reads,
/// `admitting.rs` through `Repository::bytes_of`, refused on a let-go window, and the server's `admission_turn` took
/// the refusal as ready, so the append's own admission answered it). Do: create `main` at `C1` and `side` at `S`, both
/// over `C0`, `S`'s commit in a window of its own past a filler blob ([`branched_create`]); let every window go in the
/// next push's task ([`pushed_over_a_let_go`]); then move `main` to a merge of `C1` and `S`, its pack the merge and its
/// tree. The admission's walk of what the move brings reads `S`, which `main` has not reached, through its let-go
/// window; the command's preparation never reads it (it builds the merge's tree, whose files are `C1`'s and `S`'s
/// blobs). Expect: `ok`, the push's own driver bringing the window back. Red before: `ng refs/heads/main`, the append's
/// admission refusing what the admission's turn had answered ready.
#[test]
fn b3_a_merge_whose_admission_walks_a_let_go_parent_brings_it_back_and_lands() {
  let (moved, driven, refused) = pushed_over_a_let_go("let-go-admission-walk", |window| {
    let (create, pack, (one, side), files) = branched_create(window);
    let (merge, objects) = merge_of((one, side), files);
    let request = request(
      &[(&one.to_string(), &merge.to_string(), b"refs/heads/main")],
      CAPABILITIES,
      &pack_of_objects(&objects),
    );
    ((create, pack, side), request)
  });
  assert_eq!(
    reasons(&moved),
    [b"ok"],
    "{:?}; bring-backs refused, past patience, unanswered: {refused:?}",
    moved.outcomes
  );
  assert!(driven > 0, "the push's own driver brought the window back");
}

/// [`branched_create`]'s request, its pack, `C1`'s and `S`'s ids, and the ids of `C1`'s and `S`'s blobs.
type BranchedCreate = (Vec<u8>, Vec<u8>, (ObjectId, ObjectId), (ObjectId, ObjectId));

/// A create of `main` at `C1` and `side` at `S`, each a commit of one file over `C0`, the pack laid `C1`'s objects,
/// `S`'s tree and blob, a filler blob of [`FILLER_WINDOWS`] content store windows (each `window` bytes) of bytes that
/// do not compress, then `S`'s commit and `C0`'s objects, so `S`'s commit lies in no window its files do: the request,
/// its pack, `C1`'s and `S`'s ids, and the ids of `C1`'s and `S`'s blobs.
fn branched_create(window: usize) -> BranchedCreate {
  use slates_git::object_name::{Kind, name_of};
  let (zero, under) = flat_commit(1, "zero", None);
  let (one, mut objects) = flat_commit(1, "one", Some(zero));
  let (side, mut sided) = flat_commit(1, "side", Some(zero));
  let blob_of = |objects: &[(Kind, Vec<u8>)]| {
    let (_, blob) = objects
      .iter()
      .find(|(kind, _)| *kind == Kind::Blob)
      .unwrap();
    name_of(ObjectFormat::Sha1, Kind::Blob, blob).unwrap()
  };
  let files = (blob_of(&objects), blob_of(&sided));
  let side_commit = sided.remove(0);
  objects.extend(sided);
  objects.push((
    Kind::Blob,
    crate::vcs_store::tests::incompressible(window * FILLER_WINDOWS),
  ));
  objects.push(side_commit);
  objects.extend(under);
  let pack = pack_of_objects(&objects);
  let create = request(
    &[
      ("", &one.to_string(), b"refs/heads/main"),
      ("", &side.to_string(), b"refs/heads/side"),
    ],
    CAPABILITIES,
    &pack,
  );
  (create, pack, (one, side), files)
}

/// A merge of `C1` and `S` (`parents`) whose tree holds `C1`'s file as `f00000` and `S`'s as `g` (`files`, their blobs'
/// ids): its id and its objects (the merge, its tree), the blobs the repository holds.
fn merge_of(
  (one, side): (ObjectId, ObjectId),
  (first, second): (ObjectId, ObjectId),
) -> (ObjectId, Vec<(slates_git::object_name::Kind, Vec<u8>)>) {
  use slates_git::object_name::{Kind, name_of};
  let mut tree = b"100644 f00000\0".to_vec();
  tree.extend_from_slice(first.as_bytes());
  tree.extend_from_slice(b"100644 g\0");
  tree.extend_from_slice(second.as_bytes());
  let tree_id = name_of(ObjectFormat::Sha1, Kind::Tree, &tree).unwrap();
  let commit = format!(
    "tree {tree_id}\nparent {one}\nparent {side}\nauthor A <a@example.com> 1700000002 +0000\n\
     committer A <a@example.com> 1700000002 +0000\n\nmerge side\n"
  )
  .into_bytes();
  let merge = name_of(ObjectFormat::Sha1, Kind::Commit, &commit).unwrap();
  (merge, vec![(Kind::Commit, commit), (Kind::Tree, tree)])
}

/// On a repository `name` of `daemon`: `main` created, `ok`, at a commit of a flat tree of [`THIN_TREE_FILES`] files,
/// then the request updating it to a commit whose tree adds a file, sent as git sends a thin pack (the tree a REF_DELTA
/// against the first tree, which the pack does not carry): the repository, that request, and the create's pack.
fn thin_update_over_a_create(
  daemon: &crate::Daemon,
  name: &str,
) -> (RepositoryId, Vec<u8>, Vec<u8>) {
  use slates_git::object_name::{Kind, name_of};
  let repository = git_repository_on(daemon, name);
  let (first, objects) = flat_commit(THIN_TREE_FILES, "base", None);
  let pack = pack_of_objects(&objects);
  let create = request(
    &[("", &first.to_string(), b"refs/heads/main")],
    CAPABILITIES,
    &pack,
  );
  let (created, _) = pushed_in_steps(daemon, repository, &create).unwrap();
  assert_eq!(reasons(&created), [b"ok"]);
  let base_tree = objects[1].1.clone();
  let blob = b"added\n".to_vec();
  let mut tree = base_tree.clone();
  tree.extend_from_slice(b"100644 zz\0");
  tree.extend_from_slice(
    name_of(ObjectFormat::Sha1, Kind::Blob, &blob)
      .unwrap()
      .as_bytes(),
  );
  let tree_id = name_of(ObjectFormat::Sha1, Kind::Tree, &tree).unwrap();
  let commit = format!(
    "tree {tree_id}\nparent {first}\nauthor A <a@example.com> 1700000001 +0000\n\
     committer A <a@example.com> 1700000001 +0000\n\nadded\n"
  )
  .into_bytes();
  let second = name_of(ObjectFormat::Sha1, Kind::Commit, &commit).unwrap();
  let thin = thin_tree_pack((&commit, &blob), (&base_tree, &tree));
  let update = request(
    &[(&first.to_string(), &second.to_string(), b"refs/heads/main")],
    CAPABILITIES,
    &thin,
  );
  (repository, update, pack)
}

/// A thin pack of `commit` and `blob` whole and `tree` a REF_DELTA against `base`, which it does not carry, its data in
/// stored zlib blocks, as git's client sends a push over a history the server holds.
fn thin_tree_pack((commit, blob): (&[u8], &[u8]), (base, tree): (&[u8], &[u8])) -> Vec<u8> {
  use slates_git::object_name::{Kind, name_of};
  let delta = slates_git::store::delta_of(base, tree).expect("a delta shorter than the tree");
  let base = name_of(ObjectFormat::Sha1, Kind::Tree, base).unwrap();
  entries_pack(&[
    (Entry::Whole(Kind::Commit), commit.to_vec()),
    (Entry::Ref(base), delta),
    (Entry::Whole(Kind::Blob), blob.to_vec()),
  ])
}

/// How [`entries_pack`] writes an entry.
#[derive(Clone, Copy)]
enum Entry {
  /// Whole, of this type.
  Whole(slates_git::object_name::Kind),
  /// A REF_DELTA against the object of this name.
  Ref(ObjectId),
  /// An OFS_DELTA against the entry written this many places before it in the list.
  Ofs(usize),
}

/// A pack of `entries`, each its data in stored zlib blocks, in order.
fn entries_pack(entries: &[(Entry, Vec<u8>)]) -> Vec<u8> {
  entries_pack_at(entries).0
}

/// [`entries_pack`], with where each entry begins in it.
fn entries_pack_at(entries: &[(Entry, Vec<u8>)]) -> (Vec<u8>, Vec<u64>) {
  use slates_git::entry::{self, Header, Representation};
  let mut pack = Vec::new();
  let count = u32::try_from(entries.len()).unwrap();
  let mut writer = slates_git::pack::Writer::new(ObjectFormat::Sha1, count, &mut pack);
  let mut at = entry::PACK_HEADER_LEN;
  let mut begun: Vec<u64> = Vec::new();
  for (written, data) in entries {
    let representation = match *written {
      Entry::Whole(kind) => Representation::Whole(kind),
      Entry::Ref(base) => Representation::RefDelta { base },
      Entry::Ofs(back) => Representation::OfsDelta {
        base: begun[begun.len() - back],
      },
    };
    let header = Header {
      representation,
      size: u64::try_from(data.len()).unwrap(),
    };
    let zlib = stored_zlib(data);
    begun.push(at);
    entry::write(header, at, &mut writer).unwrap();
    slates_git::out::Out::put(&mut writer, &zlib);
    at += entry::written_len(header, at).unwrap() + u64::try_from(zlib.len()).unwrap();
  }
  writer.finish().unwrap();
  (pack, begun)
}

/// What another reader of the repository does at the push's first stop after its let-go, in [`let_go_at_its_checks`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Meddle {
  /// Nothing: the push's driver brings the window back, as an edge's.
  Not,
  /// It brings the window back first and lets it go again before the push's next step
  /// ([`another_reader_brings_back_and_lets_go`]).
  LetGoAgain,
  /// It clears the repository's note of a miss, as a derivation's turn does at its start (`git_derive::turn`, "a miss
  /// an earlier step left noted is not this turn's"), before the push's driver looks.
  NoteCleared,
}

/// `request` pushed to `repository` by its owner from a task of its shard, driven as an edge drives it (each turn's
/// let-go window brought back at once, [`driven_bring_back`], then its homing carried), every window of `let_go` let
/// go by hand ([`let_go_by_hand`]) once the push's pack is read and its checks are due, and the first stop after that
/// met by `meddle`. What was received, the windows brought back at the let-go and at the end, and whether the meddling
/// was done at a stop.
async fn let_go_at_its_checks(
  repository: RepositoryId,
  request: &[u8],
  (let_go, meddle): (&[u8], Meddle),
) -> Result<(Received, (u64, u64), bool, u64), String> {
  let_go_when(repository, request, (let_go, LetGoAt::Checks, meddle)).await
}

/// When [`let_go_when`] lets the first push's pack go.
#[derive(Clone, Copy)]
enum LetGoAt {
  /// Once the push's pack is read and its checks are due.
  Checks,
  /// Once the object of this name is admitted to the push's quarantine.
  Admitted(ObjectId),
  /// Before the push began: its test let the pack go itself, and `let_go` is empty.
  Already,
  /// Once the push's pack is checked and sealed and its verdicts are being decided.
  Deciding,
}

/// [`let_go_at_its_checks`], the pack let go at `at`.
async fn let_go_when(
  repository: RepositoryId,
  request: &[u8],
  (let_go, at, meddle): (&[u8], LetGoAt, Meddle),
) -> Result<(Received, (u64, u64), bool, u64), String> {
  let (_, mut push) = on_state(|state| started(state, repository));
  for piece in request.chunks(PIECE) {
    on_state(|state| push.input(state, piece)).map_err(|failed| format!("{failed:?}"))?;
  }
  on_state(|state| push.end(state)).map_err(|failed| format!("{failed:?}"))?;
  let mut at_let_go = None;
  let mut meddled = meddle == Meddle::Not;
  let mut driven = 0u64;
  let mut answer = Vec::new();
  let received = loop {
    if at_let_go.is_none() && is_due(&push, at) {
      at_let_go = Some(on_state(|state| {
        if !let_go.is_empty() {
          let _ = let_go_by_hand(state, let_go);
        }
        brought_back_count(state)
      }));
    }
    let step = on_state(|state| push.answer_step(state, &mut answer))
      .map_err(|failed| format!("a step failed: {failed:?}"))?;
    match step {
      Answering::Answered(received) => break *received,
      Answering::Turned => {
        if at_let_go.is_some() && !meddled {
          meddled = meddled_at_a_stop(&mut push, repository, meddle).await;
        } else {
          driven = driven.saturating_add(logged_bring_back(&mut push, repository).await);
        }
        slates_rt::futures::yield_now().await;
        carry_awaited(&mut push).await?;
      }
      #[cfg(unix)]
      step @ Answering::Hook(_) => return Err(format!("no hook runs here: {step:?}")),
    }
  };
  dropped_in_a_step(push);
  let at_let_go = at_let_go.ok_or("the push's checks were never due")?;
  let after = on_state(|state| brought_back_count(state));
  Ok((
    received,
    (at_let_go, after),
    meddled && meddle != Meddle::Not,
    driven,
  ))
}

/// Whether `push` is where `at` lets the first push's pack go.
fn is_due(push: &ReceivePack, at: LetGoAt) -> bool {
  match at {
    LetGoAt::Checks => push.session.is_checking() && !push.session.is_resolving(),
    LetGoAt::Admitted(id) => push
      .quarantine
      .as_ref()
      .is_some_and(|quarantine| quarantine.quarantine().object(&id).is_some()),
    LetGoAt::Already => true,
    LetGoAt::Deciding => matches!(push.answer, super::AnswerState::Deciding(_)),
  }
}

/// The let-go window the push's last step met and noted for its driver in the push's own slot, left as it is.
fn noted_wait(
  push: &ReceivePack,
  _repository: RepositoryId,
) -> Option<slates_git::store::packed::Waited> {
  push.waits.peek()
}

/// The push's driver's bring-back after a step, as the edges do it: the window the step met brought back at once from
/// the push's own slot, before anything else on the shard runs.
async fn driven_bring_back(push: &mut ReceivePack, _repository: RepositoryId) {
  crate::vcs_store::upload::brought_back_own(push.take_waited()).await;
}

/// [`driven_bring_back`], each stop printed (libtest shows it for a failing test alone: what main's runs under load
/// read; main's order of 2026-11-26, a cause shown, never guessed): its offset and let-go number, and the windows
/// brought back before and after with the bring-backs refused. The windows it brought back of one the push noted.
async fn logged_bring_back(push: &mut ReceivePack, repository: RepositoryId) -> u64 {
  let noted = noted_wait(push, repository).map(|waited| {
    on_state(|state| {
      let number = state
        .vcs_stores
        .content()
        .and_then(|content| content.away_number(&waited.pack, waited.len, waited.offset));
      (waited.offset, number)
    })
  });
  let before = on_state(|state| brought_back_count(state));
  driven_bring_back(push, repository).await;
  if noted.is_none() {
    return 0;
  }
  let after = on_state(|state| (brought_back_count(state), bring_backs_refused(state)));
  eprintln!("b1 stop: (offset, let-go number) {noted:?}; brought back {before} -> {after:?}");
  after.0.saturating_sub(before)
}

/// `meddle` done at the push's stop, then its driver's bring-back: whether the step stopped (a window noted for the
/// driver), so the meddling met a stop; where it did not, the driver's bring-back alone, as an edge's.
async fn meddled_at_a_stop(
  push: &mut ReceivePack,
  repository: RepositoryId,
  meddle: Meddle,
) -> bool {
  let Some(waited) = noted_wait(push, repository) else {
    driven_bring_back(push, repository).await;
    return false;
  };
  match meddle {
    Meddle::Not => driven_bring_back(push, repository).await,
    Meddle::LetGoAgain => another_reader_brings_back_and_lets_go(push, repository, waited).await,
    Meddle::NoteCleared => {
      on_state(|state| {
        if let Some((_, store)) = state.vcs_stores.git(repository) {
          let _ = store.repository.waiting();
        }
      });
      driven_bring_back(push, repository).await;
    }
  }
  true
}

/// `waited` brought back as the product's drivers bring a window back: asked again where the node's store did not
/// answer within the read-through's patience (`ReadThroughRefused::Unanswered`, a load's legitimate answer: main's run
/// 10 of 16 under 12 burners), at most as many asks as the test's own patience holds of it ([`HISTORY_PATIENCE`] over
/// `read_through::PATIENCE_NS`). The last answer, and how many asks it took.
async fn brought_back_asking_again(
  waited: slates_git::store::packed::Waited,
) -> (
  Result<Option<(slates_vcs::ContentId, u64)>, crate::vcs_store::read_through::ReadThroughRefused>,
  u64,
) {
  let patience = u64::try_from(HISTORY_PATIENCE.as_nanos()).unwrap_or(u64::MAX);
  let most = (patience / crate::vcs_store::read_through::PATIENCE_NS.max(1)).max(1);
  let mut asks = 0u64;
  loop {
    asks += 1;
    let brought = crate::vcs_store::read_through::bring_back_window(waited).await;
    let unanswered = matches!(
      brought,
      Err(crate::vcs_store::read_through::ReadThroughRefused::Unanswered { .. })
    );
    if !unanswered || asks >= most {
      return (brought, asks);
    }
  }
}

/// The interleaving a concurrent reader makes (a derivation, which lets go at its end the windows it brought back),
/// forced: `waited`, the window the push's step met and noted, is brought back by another reader first
/// (`read_through::bring_back_window`); the push's driver then finds it back and brings nothing
/// ([`driven_bring_back`]); then the other reader lets it go again, as a landed window (`vcs_store::let_go_read`).
async fn another_reader_brings_back_and_lets_go(
  push: &mut ReceivePack,
  repository: RepositoryId,
  waited: slates_git::store::packed::Waited,
) {
  let (brought, asks) = brought_back_asking_again(waited).await;
  let Ok(Some((window, _))) = brought else {
    panic!("the other reader brought the window back, in {asks} asks: {brought:?}");
  };
  driven_bring_back(push, repository).await;
  let let_go = on_state(|state| {
    crate::vcs_store::within_budget(state, 0, |stores| {
      if let Some(content) = stores.content_mut() {
        content.mark_landed(&window);
      }
    });
    crate::vcs_store::let_go_read(state, &window).is_ok()
  });
  assert!(let_go, "the other reader let the window go again");
}

/// Main 01a20182's ruling of 2026-11-24 (w-hg's sweep of the bring-back sites: a write path's bring-back never let go).
/// Do: push a line of [`PACED_HISTORY`] commits, let its pack's windows go by hand before it is derived, then derive it
/// as a fetch's request does (`git_derive::derive_for_read`). Expect: every window let go first; the derivation brought
/// windows back to read them (so it is the bring-back this measures); and once it ended, none of the pack's windows held
/// (its bring-backs let go again, `vcs_store.read_through.write_path_let_go`). Red before: the windows it brought back
/// stayed held.
#[test]
fn a_derivation_lets_go_the_windows_it_brought_back_once_it_ends() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let repository = git_repository_on(&daemon, "derive-brought-back");
  let pack = paced_line_pushed_on(&daemon, repository);
  let (windows, let_go_first) = crate::daemon::observe_first(&daemon, move |state| {
    let windows = let_go_by_hand(state, &pack);
    let held = held_of(state, &windows);
    (windows, held)
  });
  let brought_before = crate::daemon::observe_first(&daemon, |state| brought_back_count(state));
  derived_for_read(&daemon, repository);
  let (brought, held_after) = crate::daemon::observe_first(&daemon, move |state| {
    (
      brought_back_count(state) - brought_before,
      held_of(state, &windows),
    )
  });
  daemon.stop();
  assert_eq!(let_go_first, 0, "every window let go first");
  assert!(
    brought > 0,
    "the derivation brought windows back: {brought}"
  );
  assert_eq!(
    held_after, 0,
    "the windows it brought back let go again once it ended"
  );
}

/// What a derivation's pace allowed: its wall time, the step's ceiling and quantum.
type Paced = (u64, u64, u64);

/// `repository`'s derivation run as a fetch runs it, a task of the shard, this thread waiting on a channel so nobody else
/// waits there: how long it took, and the ceiling and quantum its turns ran under.
fn derived_for_read(daemon: &crate::Daemon, repository: RepositoryId) -> Paced {
  let (done, finished) = std::sync::mpsc::channel();
  let (ceiling_ns, quantum_ns) = crate::daemon::observe_first(daemon, |state| {
    let quantum = state.config.step_quantum_ns();
    (
      slates_rt::futures::step_ceiling_ns()
        .unwrap_or(quantum)
        .max(quantum),
      quantum,
    )
  });
  let began = std::time::Instant::now();
  crate::daemon::observe_first(daemon, move |_| {
    let task = slates_rt::futures::spawn(async move {
      crate::vcs_store::git_derive::derive_for_read(repository).await;
      let _ = done.send(());
    })
    .unwrap();
    slates_rt::futures::detach(task).unwrap();
  });
  finished.recv().unwrap();
  let took_ns = u64::try_from(began.elapsed().as_nanos()).unwrap_or(u64::MAX);
  (took_ns, ceiling_ns, quantum_ns)
}

/// The pace's rule (A-783, `git_derive::paced`): a turn runs to its ceiling less a quantum unless someone else waits on
/// the shard, so the turns are the work's time over that span, those that yielded to a waiter (counted), and
/// [`PACED_TURNS`] besides; never the history's objects. The bound is the rule, judged at any machine's speed: a slow
/// lane's turns are fewer of its longer time, never a scaled count (`crate::test_lane`: a bound the daemon keeps is
/// never scaled).
fn assert_paced(
  (took_ns, ceiling_ns, quantum_ns): &Paced,
  (before, after): (DeriveCounts, DeriveCounts),
) {
  let (turns, derived, yielded) = (after.0 - before.0, after.1 - before.1, after.2 - before.2);
  assert_eq!(derived, 1);
  let span = ceiling_ns.saturating_sub(*quantum_ns).max(1);
  let filled = took_ns.div_ceil(span);
  assert!(
    turns <= PACED_TURNS + filled + yielded,
    "{turns} turns over {took_ns} ns at a {ceiling_ns} ns ceiling and a {quantum_ns} ns quantum, {yielded} yielded"
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
  let repository = git_repository_on(&daemon, "unborn");
  let (repository, green, before) = crate::daemon::observe_first(&daemon, move |state| {
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

/// Shape: the files of [`a_green_build_lets_go_the_windows_it_brought_back_once_it_is_made`]'s commit: a few, so its pack
/// is one window or two and the test small.
const BUILT_FILES: usize = 40;

/// Repository `name`'s unborn default branch `main` pushed at a commit of [`BUILT_FILES`] files, so its green owes the
/// push (P2): the repository, the green and the pack it landed.
fn owed_default_pushed(
  daemon: &crate::Daemon,
  name: &'static str,
) -> (RepositoryId, slates_db::catalog::VolumeId, Vec<u8>) {
  let (head, objects) = flat_commit(BUILT_FILES, name, None);
  let repository = git_repository_on(daemon, name);
  let (repository, green) = crate::daemon::observe_first(daemon, move |state| {
    let repositories = state.db.partition().repositories();
    (repository, repositories.branch(repository, "main").unwrap())
  });
  let pack = pack_of_objects(&objects);
  let create = request(
    &[("", &head.to_string(), b"refs/heads/main")],
    CAPABILITIES,
    &pack,
  );
  let (created, _) = pushed_in_steps(daemon, repository, &create).unwrap();
  assert_eq!(reasons(&created), [b"ok"]);
  (repository, green, pack)
}

/// `green` reached through the door (`green_ready`) and waited for until it owes nothing: whether it was made in time.
fn reached_and_made(daemon: &crate::Daemon, green: slates_db::catalog::VolumeId) -> bool {
  crate::daemon::observe_first(daemon, move |state| {
    crate::green_ready::green_ready(state, green);
  });
  let began = std::time::Instant::now();
  while crate::daemon::observe_first(daemon, move |state| state.greens.is_owing(&green)) {
    if began.elapsed() > std::time::Duration::from_secs(120) {
      return false;
    }
  }
  true
}

/// Main 01a20182's ruling of 2026-11-24 (w-hg's sweep: a write path's bring-back never let go; its red for the green
/// build). Do: push the unborn default branch at a commit of [`BUILT_FILES`] files, so its green owes the push; let the
/// pack's windows go by hand; then reach the green through the door and wait for it to be made. Expect: every window let
/// go first; the build brought windows back to read the files (`merge.green.materialize_brought_back`, so it is the
/// bring-back this measures); the green made; and none of the pack's windows held once it was
/// (`vcs_store.read_through.write_path_let_go`). Red before: the windows it brought back stayed held.
#[test]
fn a_green_build_lets_go_the_windows_it_brought_back_once_it_is_made() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (_, green, pack) = owed_default_pushed(&daemon, "built-brought-back");
  let (windows, held_first) = crate::daemon::observe_first(&daemon, move |state| {
    let windows = let_go_by_hand(state, &pack);
    let held = held_of(state, &windows);
    (windows, held)
  });
  let made = reached_and_made(&daemon, green);
  let (brought, held_after) = crate::daemon::observe_first(&daemon, move |state| {
    let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
    (
      count(crate::green_ready::MATERIALIZE_BROUGHT_BACK),
      held_of(state, &windows),
    )
  });
  daemon.stop();
  assert_eq!(held_first, 0, "every window let go first");
  assert!(made, "the green made");
  assert!(brought > 0, "the build brought windows back: {brought}");
  assert_eq!(
    held_after, 0,
    "the windows it brought back let go again once it was made"
  );
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
  on_stored_shard_awaited(["p2-oracle"], |[repository]| async move {
    pushed_from_the_fixtures_awaited(repository, &["create", "update", "tag", "copy"]).await;
    on_state(|state| {
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
  let (outcomes, restaged) = interleaved_creates("overtaken", CAPABILITIES);
  assert!(
    outcomes.iter().all(|reasons| reasons == &[b"ok".to_vec()]),
    "{outcomes:?}"
  );
  assert_eq!(restaged, Some(0));
}

/// P2 piece 4 (CLAUDE.md §3; A-722 part 4e's owed "a stale restage"): an atomic push judges its commands a step each
/// and appends in a step of its own, so a landing between its last command's turn and its append overtook its stage,
/// and the append's commit staged it again whole. Do: as A-722 part 4e's test above ([`interleaved_creates`]), each
/// push `atomic`. Expect: every push `ok`, and no commit staged a push again whole (`restaged` none): the append waits
/// for the stage made again in its push's turns. Red before: commits staged pushes again whole.
#[cfg(unix)]
#[test]
fn an_atomic_pushs_stage_a_landing_overtook_before_its_append_is_made_again_in_turns() {
  let (outcomes, restaged) = interleaved_creates("overtaken-atomic", ATOMIC);
  assert!(
    outcomes.iter().all(|reasons| reasons == &[b"ok".to_vec()]),
    "{outcomes:?}"
  );
  assert_eq!(restaged, Some(0));
}

/// [`INTERLEAVED`] pushes to repository `name`, each creating its own branch at the fixtures' first commit with its
/// whole pack under `capabilities`, answered a step of each in turn: each push's reasons, and the store's `restaged`.
#[cfg(unix)]
fn interleaved_creates(
  name: &'static str,
  capabilities: &'static [u8],
) -> (Vec<Vec<Vec<u8>>>, Option<u64>) {
  on_stored_shard_awaited([name], move |[repository]| async move {
    let pack = pack_of(scenario!("create").request);
    let mut pushes: Vec<Option<ReceivePack>> = on_state(|state| {
      (0..INTERLEAVED)
        .map(|k| {
          let name = format!("refs/heads/p{k}");
          let created = [("", FIRST, name.as_bytes())];
          let body = request(&created, capabilities, pack);
          let (_, mut push) = started(state, repository);
          for piece in body.chunks(PIECE) {
            push.input(state, piece).expect("taken");
          }
          push.end(state).expect("ended");
          Some(push)
        })
        .collect()
    });
    let mut outcomes = vec![Vec::new(); INTERLEAVED];
    while pushes.iter().any(Option::is_some) {
      for (at, slot) in pushes.iter_mut().enumerate() {
        let Some(push) = slot.as_mut() else {
          continue;
        };
        if let Answering::Answered(received) =
          on_state(|state| push.answer_step(state, &mut Vec::new())).expect("answered")
        {
          outcomes[at] = reasons(&received);
          // Dropped where the state is lent, as a step drops it ([`pushed_awaited`]).
          let done = slot.take();
          on_state(|_| drop(done));
        } else {
          slates_rt::futures::yield_now().await;
          carry_awaited(push).await.expect("carried");
        }
      }
    }
    let restaged = on_state(|state| {
      state
        .vcs_stores
        .git_ref(repository)
        .map(|(_, git)| git.repository.counters().restaged)
    });
    (outcomes, restaged)
  })
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
        // As the HTTP edge does: a let-go window the turn met brought back from the push's own slot (B9 of the landed
        // let-go's lift, a hook's view sealed over a let-go pack), then the homing carried.
        crate::vcs_store::upload::brought_back_own(push.take_waited()).await;
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
  let repository = git_repository_on(daemon, name);
  crate::daemon::observe_first(daemon, move |state| {
    set_policy(state, name, 0, (Vec::new(), configs));
    repository
  })
}

/// B9 of the landed let-go's lift, its push-level net (main 01a20182's ruling of 2026-11-27 07:21: a hook's view, a
/// reader the read sweep missed). Do: configure a `pre-receive` hook that reads its input and exits 0; push stock git's
/// `create` as the HTTP edge pushes; once its derivation behind it ends, let its pack go as a landed let-go does
/// ([`let_go_by_hand`]) and give back what the repository's reads keep lately; then push a new branch at `main`'s
/// commit, which brings no object, so nothing but the hook's view reads `main`'s objects, held whole in it where they
/// lie in the let-go pack. Expect: the branch lands, its hook run in one view shown and withdrawn, and windows brought
/// back for it by the push's driver. Red before: the view's seal refused `Read(Unreadable)`, the hook was not run, and
/// the branch was declined.
#[cfg(unix)]
#[test]
fn b9_a_hooked_push_over_a_let_go_pack_brings_its_views_windows_back_and_lands() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let hook = script(
    HookPointName::GitPreReceive,
    "pre-receive",
    "cat >/dev/null",
  );
  let repository = hooked_repository(&daemon, "b9-hooked", &[&hook]);
  let create = scenario!("create").request;
  let (_, created, _) = pushed_with_hooks(&daemon, repository, create).unwrap();
  assert_eq!(reasons(&created), [b"ok"], "the create landed");
  assert!(
    derivation_ended(&daemon, repository),
    "the create's derivation behind it ended"
  );
  let at = create
    .windows(4)
    .position(|magic| magic == b"PACK")
    .unwrap();
  let pack = create[at..].to_vec();
  let before = crate::daemon::observe_first(&daemon, move |state| {
    assert!(!let_go_by_hand(state, &pack).is_empty(), "the pack let go");
    if let Some((content, store)) = state.vcs_stores.git(repository) {
      let _ = store.repository.let_go_recent(content);
    }
    brought_back_count(state)
  });
  let branch = request(
    &[("", FIRST, b"refs/heads/other")],
    CAPABILITIES,
    empty_pack(),
  );
  let (_, received, shown) = pushed_with_hooks(&daemon, repository, &branch).unwrap();
  let after = crate::daemon::observe_first(&daemon, |state| brought_back_count(state));
  daemon.stop();
  assert_eq!(
    reasons(&received),
    [b"ok"],
    "the branch landed: {:?}",
    received.outcomes
  );
  assert_eq!(shown, (1, 1), "its hook's view shown and withdrawn");
  assert!(
    after > before,
    "windows brought back for the view: {before} then {after}"
  );
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

/// Shape: the files of [`a_pushs_pack_is_put_as_it_arrives_its_buffer_a_window_and_its_cache_its_chains`]'s commit.
const BOUNDED_FILES: usize = 32;
/// Shape: each one's bytes, incompressible: the pack many windows long, each object a small fraction of it.
const BOUNDED_FILE: usize = 64 << 10;

/// Main's order of 2026-11-26 (C3 (b): content once on arrival, a bounded working set a push). Do: push a create of
/// `main` at a commit of [`BOUNDED_FILES`] incompressible files of [`BOUNDED_FILE`] bytes each, its pack many windows
/// of the content store long, its request handed to the driver a piece at a time. Expect: `main` created; the most the
/// push itself held of its pack at once (`git_push.pack_buffer_most`) within one window, the pack put in the content
/// store as it arrived; its read cache's budget at most (`git_push.read_cache_budget_most`) what one longest object's
/// chain asks (`Recent::budget_keeping`, its objects whole), a fraction of the pack, and never refused; the pack homed
/// on this one-shard node by its identity, the homing holding no copy of it (b2a, `git_push.carry_buffer_most` 0), the
/// landing's `Payload` read a piece at a time into one buffer (b2b, `git_push.land_buffer_most` within one piece). Red
/// before: the push held its pack whole in a buffer of its own, and its read cache's budget was the pack's length;
/// before b2a, the homing two copies; before b2b, the landing's buffer the whole pack (2,213,177 B at 0f3e49ab9).
#[test]
fn a_pushs_pack_is_put_as_it_arrives_its_buffer_a_window_and_its_cache_its_chains() {
  use crate::vcs_store::{
    PUSH_PACK_BUFFER_MOST, PUSH_READ_CACHE_BUDGET_MOST, PUSH_READ_CACHE_REFUSED,
  };
  on_stored_shard_awaited(["bounded"], |[repository]| async move {
    let bytes = crate::vcs_store::tests::incompressible(BOUNDED_FILES * BOUNDED_FILE);
    let files: Vec<(Vec<u8>, Vec<u8>)> = bytes
      .chunks(BOUNDED_FILE)
      .enumerate()
      .map(|(n, file)| (format!("f{n:02}").into_bytes(), file.to_vec()))
      .collect();
    let (objects, head) =
      crate::vcs_store::tests::commit_of(&files, &[], crate::vcs_store::tests::FIRST_DATE);
    let pack = slates_git::pack::of_whole(
      ObjectFormat::Sha1,
      objects
        .iter()
        .map(|(kind, _, bytes)| (*kind, bytes.as_slice())),
    )
    .unwrap();
    let request = request(
      &[("", &head.to_string(), b"refs/heads/main")],
      CAPABILITIES,
      &pack,
    );
    let received = pushed_awaited(repository, &request).await;
    assert_eq!(reasons(&received), [b"ok".to_vec()], "main created");
    let (window, buffer, budget, refused, (shards, carried, landed)) = on_state(|state| {
      let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
      let carry = (
        state.shards.len(),
        count(crate::vcs_store::homes::CARRY_BUFFER_MOST),
        count(crate::git_push::LAND_BUFFER_MOST),
      );
      let window = state
        .vcs_stores
        .content()
        .map_or(0, slates_vcs::ContentStore::window);
      (
        u64::try_from(window).unwrap(),
        count(PUSH_PACK_BUFFER_MOST),
        count(PUSH_READ_CACHE_BUDGET_MOST),
        count(PUSH_READ_CACHE_REFUSED),
        carry,
      )
    });
    let pack_len = u64::try_from(pack.len()).unwrap();
    let chain =
      slates_git::store::packed::Recent::budget_keeping(u64::try_from(BOUNDED_FILE).unwrap() + 64);
    eprintln!(
      "a {pack_len}-byte pack: the push held at most {buffer} B, window {window} B; cache budget {budget} B"
    );
    assert!(pack_len > 8 * window, "the pack many windows long");
    assert!(
      buffer > 0 && buffer <= window,
      "the push held {buffer} B of its pack"
    );
    assert!(
      budget > 0 && budget <= chain && budget < pack_len / 4,
      "its read cache's budget {budget} B, a chain's {chain} B"
    );
    assert_eq!(refused, 0, "never refused");
    assert_eq!(
      shards, 1,
      "a one-shard node: the origin is every content's home"
    );
    assert_eq!(carried, 0, "the homing held {carried} B of the pack");
    let piece = u64::try_from(slates_store::store::PIECE_BYTES).unwrap();
    assert!(
      landed > 0 && landed <= piece,
      "the landing held {landed} B of the pack, one piece {piece} B"
    );
  });
}

/// A-719 part 3 (§4.17.1 "A push's intake", rule 1). Do: push a create of `main` at a commit whose one file is a
/// mebibyte, handing the driver its request a piece at a time, then answer it. Expect: once its bytes are all taken,
/// the file in the push's quarantine and the push's admission holding the pack as sent (A-723's record) and less than
/// a sixteenth of the file beside it (each object's bytes given back once the quarantine took it); then `main`
/// created.
#[test]
fn t_a719_a_pushs_pack_is_read_into_its_quarantine_holding_none_of_its_bytes() {
  on_stored_shard_awaited(["large"], |[repository]| async move {
    let (_, mut push) = on_state(|state| started(state, repository));
    let (request, blob) = large_create(LARGE);
    let room = push.admission.left();
    for piece in request.chunks(LARGE / 16) {
      on_state(|state| push.input(state, piece)).expect("taken");
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
    on_state(|state| push.end(state)).expect("ended");
    let mut answer = Vec::new();
    let received = loop {
      match on_state(|state| push.answer_step(state, &mut answer)).expect("answered") {
        Answering::Answered(received) => break *received,
        Answering::Turned => {
          slates_rt::futures::yield_now().await;
          carry_awaited(&mut push).await.expect("carried");
        }
        #[cfg(unix)]
        Answering::Hook(_) => panic!("no hook is configured"),
      }
    };
    dropped_in_a_step(push);
    assert_eq!(reasons(&received), [b"ok"]);
  });
}

/// A-724 piece 1 (§4.17.5 "What a repository costs, counted"). Do: push stock git's fixture pushes into one repository
/// while a second repository on the same shard is left alone. Expect: the pushed repository's usage row counts every
/// push begun and answered, every byte the client sent, and steps that took the shard's time, with its owner the one
/// principal named for all of them; the idle repository has no row.
#[test]
fn t_a724_a_repositorys_pushes_are_counted_on_its_own_usage_row() {
  on_stored_shard_awaited(["pushed", "idle"], |[pushed_to, idle]| async move {
    let mut sent = 0u64;
    for (scenario, _) in chain() {
      let (_, mut push) = on_state(|state| started(state, pushed_to));
      answered_awaited(&mut push, scenario.request).await;
      dropped_in_a_step(push);
      sent += u64::try_from(scenario.request.len()).unwrap();
    }
    let pushes = u64::try_from(chain().len()).unwrap();
    on_state(|state| {
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
  on_stored_shard_awaited(["many"], |[repository]| async move {
    let (_, mut push) = on_state(|state| started(state, repository));
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
      on_state(|state| push.input(state, piece)).expect("taken");
    }
    on_state(|state| push.end(state)).expect("ended");
    let mut answer = Vec::new();
    let mut deciding = 0;
    let received = loop {
      let was_deciding = matches!(push.answer, super::AnswerState::Deciding(_));
      match on_state(|state| push.answer_step(state, &mut answer)).expect("answered") {
        Answering::Answered(received) => break *received,
        Answering::Turned => {
          deciding += usize::from(was_deciding);
          slates_rt::futures::yield_now().await;
          carry_awaited(&mut push).await.expect("carried");
        }
        #[cfg(unix)]
        Answering::Hook(_) => panic!("no hook is configured"),
      }
    };
    dropped_in_a_step(push);
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
  on_stored_shard_awaited(["deferred"], |[repository]| async move {
    let mut chained = chain().into_iter();
    let (create, _) = chained.next().unwrap();
    pushed_awaited(repository, create.request).await;
    let (update, _) = chained.next().unwrap();
    let (_, mut push) = on_state(|state| started(state, repository));
    on_state(|state| push.input(state, update.request)).expect("taken");
    on_state(|state| push.end(state)).expect("ended");
    assert!(
      push.session.is_resolving(),
      "the thin pack's deltas were resolved inside an input step"
    );
    let mut answer = Vec::new();
    let mut turns = 0;
    let received = loop {
      turns += 1;
      match on_state(|state| push.answer_step(state, &mut answer)).expect("answered") {
        Answering::Answered(received) => break received,
        Answering::Turned => {
          slates_rt::futures::yield_now().await;
          carry_awaited(&mut push).await.expect("carried");
        }
        #[cfg(unix)]
        step @ Answering::Hook(_) => panic!("no hook runs here: {step:?}"),
      }
    };
    dropped_in_a_step(push);
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
  on_stored_shard_awaited(["again"], |[repository]| async move {
    pushed_from_the_fixtures_awaited(repository, &["create", "update"]).await;
    on_state(|state| {
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
  });
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
        // As the edges do (goal 93 item 2b(b), push reader (b)): the window a step met let go, brought back from the
        // push's own slot at once, then the turn yielded.
        crate::vcs_store::upload::brought_back_own(push.take_waited()).await;
        slates_rt::futures::yield_now().await;
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
/// handing its names' homing back fallen short (no region home answered). Expect: the push refused `Unlanded(HomedShort)`, and the
/// repository's register holding no record. Red before part 2b: answered `ok`, its record version 2.
#[test]
fn t_a767_c5b_a_push_whose_homing_falls_short_is_refused_and_records_nothing() {
  let (_store, daemon) = crate::test_store::audit_daemon_of_stored(1);
  let (sent, got) = std::sync::mpsc::channel();
  let repository = git_repository_on(&daemon, "short");
  crate::daemon::observe_first(&daemon, move |_| {
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
  assert_eq!(outcome, Err("Unlanded(HomedShort)".to_owned()));
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
        if let Some(crate::git_push::Carry::Home(source, _)) = push.take_homing() {
          let fallen = crate::git_push::Carried::Homed(crate::vcs_store::homes::Homed {
            outcome: Err(crate::vcs_store::homes::HomeRefused::Unanswered),
            bytes: source.into_given_back(),
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
  let repository = git_repository_on(&daemon, "owed-away");
  let (repository, green) = crate::daemon::observe_first(&daemon, move |state| {
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
  on_stored_shard_awaited(["targets"], |[repository]| async move {
    let (first, objects) = flat_commit(1, "first", None);
    let created = [("", first.to_string(), b"refs/heads/main".to_vec())];
    let created: Vec<(&str, &str, &[u8])> = created
      .iter()
      .map(|(old, new, name)| (*old, new.as_str(), name.as_slice()))
      .collect();
    let received = pushed_awaited(
      repository,
      &request(&created, CAPABILITIES, &pack_of_objects(&objects)),
    )
    .await;
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
    let received = pushed_awaited(
      repository,
      &request(&commands, CAPABILITIES, &pack_of_objects(&objects)),
    )
    .await;
    on_state(|state| {
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
  on_stored_shard_awaited(["tagged-line"], |[repository]| async move {
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
    let received = pushed_awaited(
      repository,
      &request(&commands, CAPABILITIES, &pack_of_objects(&objects)),
    )
    .await;
    assert!(
      reasons(&received).iter().all(|reason| reason == b"ok"),
      "{:?}",
      received.outcomes
    );
    on_state(|state| {
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
  let repository = git_repository_on(&daemon, "encoding-under-way");
  let reasons_pushed = reasons(&pushed_on(&daemon, repository, &pushing));
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
  on_stored_shard_awaited(["underived"], |[repository]| async move {
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
    let received = pushed_awaited(
      repository,
      &request(&created, CAPABILITIES, &pack_of_objects(&objects)),
    )
    .await;
    assert_eq!(reasons(&received), [b"ok".to_vec()]);
    let fetched = on_state(|state| fetched_ids(state, repository, &[head]));
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
        crate::vcs_store::upload::brought_back_own(push.take_waited()).await;
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
  // Kind (3) (main's rule of 2026-11-27 10:33): its fetches are answered in one synchronous step, which cannot await a
  // bring-back, and its point is each object brought once; the fetch over let-go windows:
  // `vcs_http::daemon_edge::tests::a_clone_of_a_pushed_pack_let_go_is_whole_holding_at_most_a_turns_windows`.
  crate::test_store::keep_landed(&daemon);
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
  let repository = git_repository_on(&daemon, "r16-shape");
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

/// What a push driven step by step on a daemon's shard answered, and how its steps ran; or why the drive stopped.
type Stepped = Result<(Received, Drive), String>;

/// How a push's steps ran ([`pushed_in_steps`]).
#[derive(Debug, Default, Clone, Copy)]
struct Drive {
  /// Its longest step, nanoseconds: the whole step, its build's turn and whatever else ran in that poll.
  longest_ns: u64,
  /// Its steps that ran units of its work's build and also recorded its entry (A-752: none; a step finishing the build
  /// yields before the verdict and the landing).
  built_and_landed: u64,
}

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
  let mut drive = Drive::default();
  let mut timed = |began: std::time::Instant| {
    let took = u64::try_from(began.elapsed().as_nanos()).unwrap_or(u64::MAX);
    drive.longest_ns = drive.longest_ns.max(took);
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
  let mut mixed = 0u64;
  let received = loop {
    let seen = with_state(|state| step_seen(state, repository)).ok_or("no state to see")?;
    let began = std::time::Instant::now();
    let step = with_state(|state| push.answer_step(state, &mut answer))
      .ok_or("no state for a step")?
      .map_err(|failed| format!("a step failed: {failed:?}"))?;
    timed(began);
    let after = with_state(|state| step_seen(state, repository)).ok_or("no state to see")?;
    mixed = mixed.saturating_add(u64::from(after.0 != seen.0 && after.1 != seen.1));
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
  drive.built_and_landed = mixed;
  Ok((received, drive))
}

/// What a step of [`stepped`] is judged by: the shard's build units so far, and `repository`'s recorded objects (an
/// entry's landing records them).
fn step_seen(state: &ShardState, repository: RepositoryId) -> (u64, usize) {
  let objects = state.db.partition().repositories().object_count(repository);
  (state.push_turns.units, objects)
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
  let repository = git_repository_on(&daemon, "flat");
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
  let (created, created_drive) = created.unwrap();
  let (updated, updated_drive) = updated.unwrap();
  eprintln!(
    "A-752 flat {FLAT_FILES}: create's {created_drive:?}, update's {updated_drive:?}; builds before {before:?} after \
     {after:?}"
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
  a752_steps_within_their_ceiling(updated_drive, &after);
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

/// A-752 part 3 (main's ruling of 2026-11-27 23:21): the whole step, not only its build's turn, within the ceiling by a
/// unit and its opening; and no step both building the work's units and landing its entry. The last build turn and the
/// landing ran in one poll before: 80.8 + 42.8 ms, a 124 ms step, while the turn alone kept its bound.
fn a752_steps_within_their_ceiling(drive: Drive, turns: &super::PushTurns) {
  assert_eq!(
    drive.built_and_landed, 0,
    "a step built the work's last units and landed its entry in one poll: {drive:?}"
  );
  let ceiling = crate::daemon::HEARTBEAT_NS;
  let bound = ceiling
    .saturating_add(turns.longest_unit_ns)
    .saturating_add(turns.longest_opening_ns);
  assert!(
    drive.longest_ns <= bound,
    "a step of {} ns past its {ceiling} ns ceiling by more than a unit and its opening",
    drive.longest_ns
  );
}

/// A-724 piece 8c and A-722's record v4 (main's rulings of 2026-11-20: a record names its names and its pack only once
/// they are durable at `f + 1` in the region). Do: with version 4 on for the shard, push stock git's `create` and
/// `update`, driving each answer as an edge does (a turn, a yield, the names and then the pack it asks to home homed in
/// the region, the home publishing itself), then make the store again from the log. Expect: both pushes answered `ok`;
/// each record that names objects version 4, naming its names and its pack by the identities the region homed; and
/// the store made again from those records equal to the live one. Every wait
/// is on its event (an answer, a channel), none on a fixed time: a loaded machine is the norm.
/// A-916 (main 01a20182's ruling (b1) of 2026-11-25, t_a724's replay diff diagnosed by instrument). Do: t_a724's two
/// pushes to `main`, each followed by a fetch's derivation (`git_derive::derive_for_read`), then the store made again
/// from its records compared with the live one (`assert_made_again_whole`). Expect: equal. Red before: the second
/// push's commit kept a reach value (`keep_values`) because the fetch had derived the version before it, a value no
/// code read and the replay, committing with nothing derived between, never made: live [false, true] against rebuilt
/// [false, false].
#[test]
fn a916_a_fetch_derived_between_two_pushes_leaves_the_store_its_records_make_again() {
  let (_store, daemon) = crate::test_store::audit_daemon_of_stored(1);
  crate::test_store::keep_landed(&daemon);
  let (sent, got) = std::sync::mpsc::channel();
  let repository = git_repository_on(&daemon, "derived-between");
  crate::daemon::observe_first(&daemon, move |_| {
    let requests: Vec<Vec<u8>> = chain()
      .into_iter()
      .take(2)
      .map(|(scenario, _)| scenario.request.to_vec())
      .collect();
    let task = slates_rt::futures::spawn(async move {
      let mut answered = true;
      for request in requests {
        answered &= homed_drive(repository, &request).await.is_ok();
        crate::vcs_store::git_derive::derive_for_read(repository).await;
      }
      let _ = sent.send((repository, answered));
    })
    .expect("the pushes' task");
    slates_rt::futures::detach(task).expect("detached");
  });
  let (repository, answered) = got.recv().expect("the pushes answered");
  let (checked, sent) = std::sync::mpsc::channel();
  crate::daemon::observe_first(&daemon, move |state| {
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
    let _ = checked.send(());
  });
  let made_again = sent.recv().is_ok();
  daemon.stop();
  assert!(answered, "both pushes answered");
  assert!(made_again, "the store made again equal to the live one");
}

#[test]
fn t_a724_a_pushs_record_names_its_names_once_they_are_homed_in_the_region() {
  let (_store, daemon) = crate::test_store::audit_daemon_of_stored(1);
  crate::test_store::keep_landed(&daemon);
  let (sent, got) = std::sync::mpsc::channel();
  let repository = git_repository_on(&daemon, "homed");
  crate::daemon::observe_first(&daemon, move |_| {
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
  let repository = crate::test_store::create_on(
    daemon,
    partition,
    &owner(),
    (name, VcsTool::Git, WireFormat::Sha1, "main"),
  )
  .0;
  crate::daemon::observe_at(daemon, partition, move |_| {
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
  on_stored_shard_awaited(["shipped"], |[repository]| async move {
    let _ = on_state(|state| state.db.take_state_log_records());
    pushed_from_the_fixtures_awaited(repository, &["create"]).await;
    on_state(|state| {
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
  });
}

/// A-724 piece 8c and C5 part 2b piece 2 (a decision never waits for a homing nobody runs, found by the takeover test;
/// and no record names a push's objects inline). Do: push stock git's `create` through a driver that never takes the
/// homing its decision asks for. Expect: the push refused `Unlanded(HomingUntaken)`, the untaken homing counted, and the repository's
/// register holding no record. Red before piece 2: answered `ok`, its record version 2.
#[test]
fn t_a767_c5b_a_push_whose_homing_no_edge_takes_is_refused() {
  on_stored_shard_with(["untaken"], |state, [repository]| {
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
    assert_eq!(refused.as_deref(), Some("Unlanded(HomingUntaken)"));
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
  on_stored_shard_awaited(["reused"], |[repository]| async move {
    let (objects, named) = tagged_line();
    let pack = deltified_pack(&objects);
    let ids: Vec<String> = named.iter().map(|(id, _)| id.to_string()).collect();
    let commands: Vec<(&str, &str, &[u8])> = named
      .iter()
      .zip(&ids)
      .map(|((_, name), id)| ("", id.as_str(), name.as_slice()))
      .collect();
    let received = pushed_awaited(repository, &request(&commands, CAPABILITIES, &pack)).await;
    assert!(
      reasons(&received).iter().all(|reason| reason == b"ok"),
      "{:?}",
      received.outcomes
    );
    on_state(|state| {
      while crate::vcs_store::git_derive::turn(state, repository)
        != crate::vcs_store::git_derive::Turned::Idle
      {}
      let wants: Vec<ObjectId> = named.iter().map(|(id, _)| *id).collect();
      let fetch = crate::vcs_store::upload::tests::fetch(&wants, &[]);
      let bound = pack.len() + objects.len() * REUSED_ENTRY_SLACK;
      // Cloned before any version is encoded: every object from the stream's loose stage.
      let (streamed, counted_streamed) = crate::vcs_store::upload::tests::fetched_with_counters(
        state,
        (repository, &owner()),
        &fetch,
      );
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
      assert_the_stream_sent_every_pushed_entry_as_it_lies(
        &counted_streamed,
        deltas,
        objects.len(),
      );
      assert_whole_clone(&streamed, objects.len());
      while crate::vcs_store::git_encode::turn(state, repository)
        != crate::vcs_store::git_encode::Turned::Idle
      {}
      let (answer, counted) = crate::vcs_store::upload::tests::fetched_with_counters(
        state,
        (repository, &owner()),
        &fetch,
      );
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
  on_stored_shard_awaited(["based"], |[repository]| async move {
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
    let received = pushed_awaited(repository, &request(&commands, CAPABILITIES, &pack)).await;
    assert!(
      reasons(&received).iter().all(|reason| reason == b"ok"),
      "{:?}",
      received.outcomes
    );
    on_state(|state| {
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
      let (answer, _) = crate::vcs_store::upload::tests::fetched_with_counters(
        state,
        (repository, &owner()),
        &fetch,
      );
      assert_whole_clone(
        &crate::vcs_store::upload::tests::pack_of(&answer),
        objects.len(),
      );
    });
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
          crate::git_push::Carry::Home(source, _) if homings > 1 => {
            crate::git_push::Carried::Homed(crate::vcs_store::homes::Homed {
              outcome: Err(crate::vcs_store::homes::HomeRefused::Unanswered),
              bytes: source.into_given_back(),
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
/// handed back fallen short. Expect: the push refused `Unlanded(HomedShort)`, its attempt's release settled, and nothing held for the
/// region. Failing first: the names' region hold stayed, no record naming them, for the life of the home.
#[test]
fn t_a727_d1a_a_push_refused_after_its_names_were_homed_leaves_no_hold_in_the_region() {
  let (_store, daemon) = crate::test_store::audit_daemon_of_stored(1);
  let (sent, got) = std::sync::mpsc::channel();
  let repository = git_repository_on(&daemon, "half-homed");
  crate::daemon::observe_first(&daemon, move |_| {
    let request = chain()
      .into_iter()
      .next()
      .map(|(scenario, _)| scenario.request.to_vec())
      .expect("create");
    let task = slates_rt::futures::spawn(async move {
      let outcome = half_homed_drive(repository, &request).await;
      // One settle, its release of the names (homed) and the pack (its homing fallen) together.
      let settled = settled(1).await;
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
  assert_eq!(outcome, Err("Unlanded(HomedShort)".to_owned()));
  assert!(settled, "the attempt's release settled");
  assert_eq!(unanswered, Some(0), "every home answered it");
  assert!(held.is_empty(), "nothing held for the region: {held:?}");
}

/// A-727 (d1a), two repositories on one identity (main's ruling: a refused attempt's release leaves another repository's
/// hold of the same object untouched). Do: on a store-backed shard, push stock git's `create` to repository `a` whole,
/// its holds settled to its records; then the same push to repository `b`, its names homed (the same object as `a`'s)
/// and its pack's homing fallen short. Expect: `a`'s push answered, `b`'s refused `Unlanded(HomedShort)`; afterwards every object
/// held for the region held for `a`'s records alone, as many as after `a`'s push: `b`'s release took nothing of `a`'s.
#[test]
fn t_a727_d1a_a_refused_push_leaves_another_repositorys_hold_of_the_same_object() {
  let (_store, daemon) = crate::test_store::audit_daemon_of_stored(1);
  let (sent, got) = std::sync::mpsc::channel();
  let first = git_repository_on(&daemon, "a");
  let second = git_repository_on(&daemon, "b");
  crate::daemon::observe_first(&daemon, move |_| {
    let request = chain()
      .into_iter()
      .next()
      .map(|(scenario, _)| scenario.request.to_vec())
      .expect("create");
    let task = slates_rt::futures::spawn(async move {
      let answered = homed_drive(first, &request).await.is_ok();
      // One settle moving the names and the pack; then b's one releasing its two.
      let moved = settled(1).await;
      let after_first = crate::vcs_store::homes::region_held_here();
      let refused = half_homed_drive(second, &request).await;
      let released = settled(2).await;
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
  assert_eq!(refused, Err("Unlanded(HomedShort)".to_owned()));
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

/// A-921 part 2 (main 01a20182's ruling of 2026-11-25: the home healer waits the shared owner lookup within its own
/// budget). Do: on a store-backed shard, hold three objects for earlier-process attempts of repositories no shard holds
/// (nobody will claim them), then run one resolution pass within the live budget. Expect the pass over within that
/// budget, the most the lookup's own waits came back late and its longest look, each measured in the lookup
/// (`owner_location::WAIT_LATE_MOST`, `LOOK_MOST`; the lane never held past it), each hold kept, counted, and no hold
/// let go or moved. Before, the shared lookup waited its own horizon for each unclaimed attempt in turn; and then a
/// whole pace past its bound, which the test allowed as one look's pace and the timer's lateness overran (main's sets
/// of 2026-11-26: 10.08 to 10.57 ms past a 1,998 ms budget, 6 times in 4 sets;
/// `owner_location::tests::a921_a_bounded_owner_lookup_ends_within_its_bound_and_its_waits_lateness`).
#[test]
fn a_resolution_pass_over_unclaimed_attempts_ends_within_its_budget() {
  use crate::vcs_store::homes;
  use slates_cluster::homes::HomeOwner;
  let (_store, daemon) = crate::test_store::audit_daemon_of_stored(1);
  let (sent, got) = std::sync::mpsc::channel();
  crate::daemon::observe_first(&daemon, move |state| {
    let (node, process) = (state.fleet.host(), state.member_process_nonce ^ 1);
    let held: Option<Vec<slates_vcs::ContentId>> = (0u8..3)
      .map(|number| {
        let owner = HomeOwner::Attempt {
          repository: [0xE0 | number; 16],
          node,
          process,
          attempt: u64::from(number),
        };
        homes::keep_for_region(state, &[b'u', number], &[owner])
          .ok()
          .map(|(identity, _)| identity)
      })
      .collect();
    let (origin, shards) = (state.shard, state.shards.clone());
    let task = slates_rt::futures::spawn(async move {
      let Some(held) = held else {
        let _ = sent.send(Err("not held".to_owned()));
        return;
      };
      let budget = crate::fleet::live_budget();
      homes::resolve_owed();
      let began = slates_rt::futures::now_ns();
      homes::resolve_ended(origin, &shards, budget).await;
      let took = slates_rt::futures::now_ns().saturating_sub(began);
      let (late, look) = crate::state::with_state(|state| {
        let most = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
        (
          most(crate::owner_location::WAIT_LATE_MOST),
          most(crate::owner_location::LOOK_MOST),
        )
      })
      .unwrap_or_default();
      let seen = crate::state::with_state(|state| {
        let counted = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
        (
          held
            .iter()
            .map(homes::region_holds_here)
            .collect::<Vec<_>>(),
          [
            counted(homes::RESOLVED_RECORD),
            counted(homes::RESOLVED_LET_GO),
            counted(homes::RESOLVE_KEPT),
          ],
        )
      });
      let _ = sent.send(Ok((took, (budget.max_deadline_ns(), late, look), seen)));
    })
    .expect("the task");
    slates_rt::futures::detach(task).expect("detached");
  });
  let (took, (budget_ns, late, look), seen) = got.recv().expect("the pass ended").expect("held");
  let (holds, counts) = seen.expect("the shard's state");
  println!(
    "a919: a resolution pass over three unclaimed attempts took {took} ns of its {budget_ns} ns budget, its waits \
     {late} ns late, a look {look} ns"
  );
  assert!(
    took <= budget_ns.saturating_add(late).saturating_add(look),
    "the pass ended within its budget, its lookup's waits' lateness and its longest look: {took} ns of {budget_ns} \
     ns, {late} ns late, a look {look} ns"
  );
  assert_eq!(holds, vec![1, 1, 1], "every unclaimed hold kept");
  assert_eq!((counts[0], counts[1]), (0, 0), "none moved or let go");
  assert!(counts[2] >= 3, "each kept, counted: {counts:?}");
}

/// A-727 (d1a-3) (main 01a20182's ruling (u) of 2026-11-23: a hold an attempt a crash left is resolved from the homes'
/// side once its process ended, by its repository's current holder, from its records; never on a timer). Do: on a
/// store-backed shard (one node, `f = 0`: the home and the holder are this node), push stock git's `create` whole, its
/// holds settled to its records; then hold one of its objects again, and an object no record names, for an attempt of
/// the repository in an earlier process of this member; hold another object for a live attempt of this process, and one
/// for an earlier process's attempt of a repository no shard here holds; run a resolution pass. Expect: the named
/// object's hold moved to the repository's records (nothing of the attempt left), the unnamed one let go, the live
/// attempt's hold untouched (never asked), the unheld repository's hold kept and counted; each counted by its outcome.
/// Failing first: every crash-left hold stayed for the home's life.
#[test]
fn t_a727_d1a3_a_crash_left_attempts_holds_resolve_by_the_repositorys_records() {
  use crate::vcs_store::homes;
  use slates_cluster::homes::HomeOwner;
  let (_store, daemon) = crate::test_store::audit_daemon_of_stored(1);
  let (sent, got) = std::sync::mpsc::channel();
  let repository = git_repository_on(&daemon, "resolved");
  crate::daemon::observe_first(&daemon, move |_| {
    let request = chain()
      .into_iter()
      .next()
      .map(|(scenario, _)| scenario.request.to_vec())
      .expect("create");
    let task = slates_rt::futures::spawn(async move {
      let pushed = homed_drive(repository, &request).await.is_ok() && settled(1).await;
      let outcome = crate::state::with_state(|state| {
        let (node, process) = (state.fleet.host(), state.member_process_nonce ^ 1);
        let earlier = move |repository: [u8; 16], attempt| HomeOwner::Attempt {
          repository,
          node,
          process,
          attempt,
        };
        let named = homes::region_held_here().first().copied()?;
        homes::join_owner(&named, earlier(repository.bytes, 1));
        let (unnamed, _) = homes::keep_for_region(
          state,
          b"an object no record of the repository names",
          &[earlier(repository.bytes, 2)],
        )
        .ok()?;
        let live = homes::attempt_owner(state, repository.bytes).ok()?;
        let (lived, _) =
          homes::keep_for_region(state, b"an object a live attempt holds", &[live]).ok()?;
        let (orphan, _) = homes::keep_for_region(
          state,
          b"an object of a repository nobody here holds",
          &[earlier([0xEE; 16], 3)],
        )
        .ok()?;
        Some((
          (named, unnamed),
          (lived, live),
          orphan,
          (state.shard, state.shards.clone()),
        ))
      })
      .flatten();
      let Some(((named, unnamed), (lived, live), orphan, (origin, shards))) = outcome else {
        let _ = sent.send(Err("not held".to_owned()));
        return;
      };
      homes::resolve_owed();
      homes::resolve_ended(origin, &shards, crate::fleet::live_budget()).await;
      let seen = crate::state::with_state(|state| {
        let counted = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
        (
          homes::region_owners(&named),
          homes::region_holds_here(&unnamed),
          homes::region_owners(&lived),
          homes::region_holds_here(&orphan),
          [
            counted(homes::RESOLVED_RECORD),
            counted(homes::RESOLVED_LET_GO),
            counted(homes::RESOLVE_KEPT),
          ],
        )
      });
      let _ = sent.send(Ok((pushed, repository, live, seen)));
    })
    .expect("the task");
    slates_rt::futures::detach(task).expect("detached");
  });
  let (pushed, repository, live, seen) = got.recv().expect("the pass ended").expect("held");
  let (named, unnamed, lived, orphan, counts) = seen.expect("the shard's state");
  assert!(pushed, "the push answered, its holds settled");
  assert_eq!(
    named,
    vec![HomeOwner::Record {
      repository: repository.bytes
    }],
    "the named object's hold the records' alone"
  );
  assert_eq!(unnamed, 0, "the unnamed object let go");
  assert_eq!(lived, vec![live], "the live attempt's hold untouched");
  assert_eq!(orphan, 1, "the unheld repository's hold kept");
  assert_eq!(counts, [1, 1, 1], "each counted by its outcome");
}

/// Shape: the new refs one push makes in [`landing_counts_of_a_push_of_new_refs`]: several, so a step making them all is
/// told from one making one.
const NEW_REFS: usize = 5;

/// A push of [`NEW_REFS`] new refs to repository `name` on an audit shard, each a tag at one commit, its first command
/// asking `capabilities`: its reasons, and what the landing counted most of the creates one step made itself and of one
/// step's time.
fn landing_counts_of_a_push_of_new_refs(
  name: &'static str,
  capabilities: &'static [u8],
) -> (Vec<Vec<u8>>, u64, u64) {
  on_stored_shard_awaited([name], move |[repository]| async move {
    let (head, objects) = flat_commit(1, "a tagged commit", None);
    let head = head.to_string();
    let refs: Vec<String> = (0..NEW_REFS).map(|at| format!("refs/tags/v{at}")).collect();
    let commands: Vec<(&str, &str, &[u8])> = refs
      .iter()
      .map(|name| ("", head.as_str(), name.as_bytes()))
      .collect();
    let received = pushed_awaited(
      repository,
      &request(&commands, capabilities, &pack_of_objects(&objects)),
    )
    .await;
    on_state(|state| {
      let count = |name: &str| state.refusals.get(name).copied().unwrap_or(0);
      (
        reasons(&received),
        count(crate::repository::push::LANDING_STEP_CREATES),
        count(crate::repository::push::LANDING_STEP_NS),
      )
    })
  })
}

/// w-git's measure before slicing git's creates (main 01a20182, 2026-11-24: a git create is P2's deferred one, with no
/// work, so `repository.push.landing_step_works_most` cannot see it). Do: push five new tags at one commit to a fresh
/// repository, as stock git pushes them (no `atomic`), then again to another with `atomic`. Expect: every ref `ok` in
/// both; without `atomic` each ref lands in a step of its own (one create the most a step made), so git/git's ~1,027 new
/// refs are ~1,027 small steps, never one; with `atomic` all five in one step; and each step's time counted.
#[test]
fn a_push_of_new_refs_counts_the_creates_its_landing_step_made_and_the_steps_time() {
  let (said, creates, took) = landing_counts_of_a_push_of_new_refs("landing-counts", CAPABILITIES);
  assert_eq!(said, vec![b"ok".to_vec(); NEW_REFS]);
  assert_eq!((creates, took > 0), (1, true), "a ref a step, timed");
  let (said, creates, took) = landing_counts_of_a_push_of_new_refs("landing-atomic", ATOMIC);
  assert_eq!(said, vec![b"ok".to_vec(); NEW_REFS]);
  let all = u64::try_from(NEW_REFS).unwrap();
  assert_eq!((creates, took > 0), (all, true), "all in one step, timed");
}

/// Main 01a20182's ruling of 2026-11-25 (b), from the readers' code: a git reader that holds no pin reads again after a
/// let-go between its bring-back and its read, and brings the window back again, so a read-back's let-go there costs a
/// second fetch, never the answer. Do: push the same line of [`PACED_HISTORY`] commits to two repositories (one pack,
/// the same windows) and let its windows go by hand; derive the first as a fetch's request does
/// (`git_derive::derive_for_read`), its windows let go again as it ends; then derive the second with the window its
/// first bring-back puts back let go once more before the derivation's next turn reads it
/// (`vcs_share::read_backs::FORCE_LET_GO_AFTER_BRING_BACK`). Expect: each derivation derives its one version; the
/// second brought one window back once more than the first (the re-bring); and once both end, none of the pack's
/// windows held.
#[test]
fn a_derivation_brings_a_window_let_go_under_it_back_again_and_derives_the_same() {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (plain, forced) = (
    git_repository_on(&daemon, "derive-plain"),
    git_repository_on(&daemon, "derive-forced"),
  );
  paced_line_pushed_on(&daemon, plain);
  let pack = paced_line_pushed_on(&daemon, forced);
  let windows = crate::daemon::observe_first(&daemon, move |state| let_go_by_hand(state, &pack));
  let counts = |daemon: &crate::Daemon| {
    crate::daemon::observe_first(daemon, |state| {
      (brought_back_count(state), derive_counts(state).1)
    })
  };
  let before = counts(&daemon);
  derived_for_read(&daemon, plain);
  let between = counts(&daemon);
  let held_between = crate::daemon::observe_first(&daemon, {
    let windows = windows.clone();
    move |state| held_of(state, &windows)
  });
  crate::daemon::observe_first(&daemon, |_| {
    crate::vcs_share::read_backs::FORCE_LET_GO_AFTER_BRING_BACK.with(|force| force.set(true));
  });
  derived_for_read(&daemon, forced);
  let after = counts(&daemon);
  let held_after = crate::daemon::observe_first(&daemon, move |state| held_of(state, &windows));
  daemon.stop();
  let (plain_brought, forced_brought) = (between.0 - before.0, after.0 - between.0);
  assert_eq!(
    (between.1 - before.1, after.1 - between.1),
    (1, 1),
    "each derivation derived its version"
  );
  assert!(
    plain_brought > 0,
    "the plain derivation brought windows back"
  );
  assert_eq!(
    forced_brought,
    plain_brought + 1,
    "the forced one brought the window let go under it back again"
  );
  assert_eq!(
    (held_between, held_after),
    (0, 0),
    "the windows let go again"
  );
}

/// `green`'s id in a refusal's words: its bytes in lower-case hex.
fn hex_of(green: slates_db::catalog::VolumeId) -> String {
  green
    .bytes
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect()
}

/// Asserts that `received` refused its one command naming `words` on band 2, never "holds files no commit names".
fn assert_refused_naming(received: &Received, words: &str) {
  assert_eq!(reasons(received), [b"failed to update ref".to_vec()]);
  assert_first_line_names(received, words.as_bytes());
  assert!(
    !received.outcomes.iter().any(|outcome| matches!(
      outcome,
      Outcome::Ng(Declined { message: Some(line), .. })
        if line.windows(NO_COMMIT_NAMES.len()).any(|seen| seen == NO_COMMIT_NAMES)
    )),
    "the old false reason"
  );
}

/// The words of the reason A-927 retired for a default green this node could not read.
const NO_COMMIT_NAMES: &[u8] = b"holds files no commit names";

/// A-927 (main's ruling of 2026-11-25; the a894 loop, docs/design/amendments/A-927): a successor that gave a
/// repository's root green up answered the first push's create "the default branch holds files no commit names", which
/// it did not know. Do: make a git repository; record its default green not recovered, as a takeover that gave it up
/// records it; push `main`'s create. Expect: the create refused naming the green and its cause's kind on band 2,
/// never "holds files no commit names" (before, it landed on the green this node had not recovered).
#[test]
fn t_a927_a_create_on_a_default_green_not_recovered_is_refused_naming_it() {
  let asked = ("given-up", VcsTool::Git, WireFormat::Sha1, "main");
  let (received, green) = on_stored_shard_made_awaited(asked, |(repository, green)| async move {
    let cause = slates_ipc::protocol::RecoveryCause::TakeoverIncomplete {
      waited_on: "inputs".to_owned(),
    };
    on_state(|state| state.unrecovered.insert(green, cause));
    (
      pushed_awaited(repository, chain()[0].0.request).await,
      green,
    )
  });
  let words = format!(
    "the default branch's green {} is not recovered here (TakeoverIncomplete)",
    hex_of(green)
  );
  assert_refused_naming(&received, &words);
}

/// A-927: a successor still materializing an adopted repository holds its record before its root green's engine. Do:
/// make a git repository; take its default green's engine out of its lineage; push `main`'s create; put it back.
/// Expect: the create refused naming the green as not made here yet, to retry, on band 2, never "holds files no commit
/// names".
#[test]
fn t_a927_a_create_on_a_default_green_not_made_here_is_refused_to_retry() {
  let asked = ("unmade", VcsTool::Git, WireFormat::Sha1, "main");
  let (received, green) = on_stored_shard_made_awaited(asked, |(repository, green)| async move {
    let engine = on_state(|state| state.greens.take_any(&green).expect("the green was made"));
    let received = pushed_awaited(repository, chain()[0].0.request).await;
    on_state(|state| state.greens.put_back_any(&green, engine));
    (received, green)
  });
  let words = format!(
    "the default branch's green {} is not made here yet; retry",
    hex_of(green)
  );
  assert_refused_naming(&received, &words);
}
