//! A-665 part 2's tests (§4.17.2 "Hooks, in git's terms"): a git hook run in its view at each of
//! receive-pack's points, the view opened on the owner shard over the repository's store, sealed
//! there and written into a scratch directory by the tests' own presenter (part 3 presents it as
//! an attachment, writing nothing); every hold and charge a view took given back at its close;
//! and each refusal by kind, with nothing run and nothing held.

use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::channel;

use slates_db::catalog::{HookConfig, HookPointName, RepositoryId, RepositoryPolicy};
use slates_git::store::hook_view::Node;
use slates_ipc::protocol::VcsTool;
use slates_rt::runtime::{LocalRuntime, RuntimeConfig};

use super::{
  GitHookRan, GitHookRefused, GitHookSite, GitHookView, PresentView, Presented, ReadRef, ShownPush,
  Shows, run,
};
use crate::daemon::LIVENESS_BUDGET_NS;
use crate::repository::push::{BranchAppended, BranchChange};
use crate::state::ShardState;
use crate::vcs_hooks::{
  CommittedView, EnvValue, HookInput, HookPoint, Outcome, PendingView, RefUpdate, git_post_receive,
  git_pre_receive, git_update,
};
use crate::vcs_store::Target;
use crate::vcs_store::tests::{
  appended, begun, commit_of, hex, push_through, repository, tree_of, work_of,
};

/// Format: the variable that makes this test binary, re-invoked, the hook that reports what it
/// sees.
const ROLE: &str = "SLATES_GIT_HOOK_ROLE";
/// Format: the libtest name of the role test the binary is re-invoked with.
const ROLE_TEST: &str = "git_hooks::tests::a_git_hook_role";
/// Format: the tag before each line the role writes, since libtest's banner shares its words.
const TAG: &str = "slates-git-hook:";
/// Format: what the role writes for a variable it was not handed.
const UNSET: &str = "<unset>";
/// Format: the variables the role reports: receive-pack's for the view, and the push options'
/// count.
const VARIABLES: [&str; 5] = [
  "GIT_DIR",
  "GIT_OBJECT_DIRECTORY",
  "GIT_QUARANTINE_PATH",
  "GIT_ALTERNATE_OBJECT_DIRECTORIES",
  "GIT_PUSH_OPTION_COUNT",
];
/// Format: the default branch's full name, `HEAD`'s target and the ref the pushes move.
const MAIN: &[u8] = b"refs/heads/main";
/// Format: a version 2 pack index's first four bytes (`\377tOc`), in hex.
const IDX_MAGIC: &str = "ff744f63";
/// Format: a pack's first four bytes (`PACK`), in hex.
const PACK_MAGIC: &str = "5041434b";
/// Format: the first committer date the pushes use, seconds since the epoch.
const FIRST_DATE: u64 = 1_700_000_000;
/// Shape: how many bytes of a hook's words the tests keep.
const CAP: usize = 1 << 16;
/// Shape: how long a test lets a run take before giving up on it: ten liveness windows.
pub(crate) const TEST_BOUND_NS: u64 = 10 * LIVENESS_BUDGET_NS;
/// Shape: the most turns a view's sealing may take here, far past what these packs need a unit of
/// work a turn.
const TURNS: u32 = 1 << 20;
/// Shape: the presenter's read, short, so a pack file crosses several.
const READ_WINDOW: u64 = 1 << 8;

// ---------------------------------------------------------------------------------------------
// The presentation (the tests' own: part 3 presents the view as an attachment).

/// A scratch directory in the build output beside the test binary, named with the process id and
/// a count, removed when dropped, even when an assertion fails: where the tests' presenter writes
/// a view.
#[derive(Debug)]
pub(crate) struct ScratchDir {
  /// Its canonical path.
  pub(crate) path: PathBuf,
}

impl ScratchDir {
  /// A fresh, empty one.
  #[allow(clippy::disallowed_methods)] // test code: its own scratch directory, removed with it
  pub(crate) fn new() -> ScratchDir {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let binary = std::env::current_exe().expect("the test binary");
    let count = NEXT.fetch_add(1, Ordering::Relaxed);
    let name = format!("slates-git-hook-{}-{count}", std::process::id());
    let path = binary.parent().expect("its directory").join(name);
    std::fs::create_dir_all(&path).expect("the scratch directory");
    let path = std::fs::canonicalize(&path).expect("its canonical path");
    ScratchDir { path }
  }
}

impl Drop for ScratchDir {
  #[allow(clippy::disallowed_methods)] // test code: its own scratch directory
  fn drop(&mut self) {
    let _ = std::fs::remove_dir_all(&self.path);
  }
}

/// Writes `view` into `root` as a presentation serves it: each directory it lists made, each file
/// it lists read from the store a window at a time, and what each path names asked of the view
/// as a presentation answers a lookup, which must be what its directory listed.
#[allow(clippy::disallowed_methods)] // test code: its own scratch directory
fn present(state: &ShardState, view: &GitHookView, root: &Path) {
  let mut directories: Vec<Vec<u8>> = vec![Vec::new()];
  while let Some(directory) = directories.pop() {
    for listed in view.list(&directory).expect("a directory") {
      let path = if directory.is_empty() {
        listed.name.clone()
      } else {
        [
          directory.as_slice(),
          b"/".as_slice(),
          listed.name.as_slice(),
        ]
        .concat()
      };
      let at = root.join(std::ffi::OsStr::from_bytes(&path));
      assert_eq!(view.node(&path), Some(listed.node), "{}", at.display());
      match listed.node {
        Node::Directory => {
          std::fs::create_dir_all(&at).expect("a directory written");
          directories.push(path);
        }
        Node::File { len } => {
          std::fs::write(&at, read_whole(state, view, &path, len)).expect("a file written");
        }
      }
    }
  }
}

/// The tests' presenter (A-665 part 3): each view it shows written whole into a scratch directory
/// of its own, removed as the view is withdrawn or the presenter dropped.
#[derive(Debug, Default)]
pub(crate) struct ScratchPresenter {
  /// The directories of the views it shows.
  shown: Vec<ScratchDir>,
  /// How many views it has shown: the push tests' count that their hooks ran in views.
  pub(crate) presented: u32,
  /// How many it has withdrawn.
  pub(crate) withdrawn: u32,
}

impl PresentView for ScratchPresenter {
  fn can_present(&self) -> bool {
    true
  }

  fn present(&mut self, state: &ShardState, view: &GitHookView) -> Option<PathBuf> {
    let dir = ScratchDir::new();
    present(state, view, &dir.path);
    let root = dir.path.clone();
    self.shown.push(dir);
    self.presented += 1;
    Some(root)
  }

  fn withdraw(&mut self, root: &Path) {
    self.shown.retain(|dir| dir.path != root);
    self.withdrawn += 1;
  }
}

/// The file `path` of `view`, `len` bytes, read whole a window at a time.
fn read_whole(state: &ShardState, view: &GitHookView, path: &[u8], len: u64) -> Vec<u8> {
  let mut bytes = Vec::new();
  let mut at = 0;
  while at < len {
    view
      .read(&state.vcs_stores, path, (at, READ_WINDOW), &mut bytes)
      .expect("a sealed view reads");
    at += READ_WINDOW;
  }
  assert_eq!(u64::try_from(bytes.len()).unwrap(), len);
  bytes
}

// ---------------------------------------------------------------------------------------------
// The owner shard.

/// Every hold the content store counts, over every content it holds.
fn held(state: &ShardState) -> u64 {
  state.vcs_stores.content().map_or(0, |content| {
    content
      .identities()
      .map(|identity| content.holders(identity))
      .sum()
  })
}

/// What the shard holds and charges for the stores: the holds, and the shard's charge.
fn footprint(state: &ShardState) -> (u64, u64) {
  (held(state), state.vcs_stores.charged())
}

/// Seals every pack of `view` a turn at a time, as its task would between yields: each turn the
/// shard's quantum, or with `one_unit` a unit of work. The turns taken.
fn sealed_whole(state: &mut ShardState, view: &mut GitHookView, one_unit: bool) -> u32 {
  for turn in 1..=TURNS {
    let done = if one_unit {
      view.seal_within(state, 0)
    } else {
      view.seal_turn(state)
    };
    if done.expect("a turn of sealing") {
      return turn;
    }
  }
  panic!("the view was not sealed within {TURNS} turns");
}

/// One point's view as the shard made it: presented in its scratch directory.
#[derive(Debug)]
struct Shown {
  /// The directory the view was written into.
  dir: ScratchDir,
  /// The view as a hook sees it there.
  presented: Presented,
}

/// Opens `repository`'s view at `refs` with `pushed` on the shard, seals it (with `one_unit`, a
/// unit of work a turn), presents it in a scratch directory and closes it, holding the shard's
/// footprint to the module docs: more held and charged once open, more charged once sealed, all of
/// it given back at the close; a sliced seal taking more turns than the view has packs.
fn shown(
  state: &mut ShardState,
  (repository, refs): (RepositoryId, &[ReadRef<'_>]),
  pushed: Option<ShownPush<'_>>,
  one_unit: bool,
) -> Shown {
  let before = footprint(state);
  let mut view =
    GitHookView::open(state, repository, (refs, MAIN), pushed).expect("the view opened");
  let opened = footprint(state);
  let packs = u32::try_from(view.view.packs().count()).unwrap();
  let turns = sealed_whole(state, &mut view, one_unit);
  let sealed = state.vcs_stores.charged();
  let dir = ScratchDir::new();
  present(state, &view, &dir.path);
  let presented = view.presented_at(&dir.path);
  view.close(state);
  assert_balanced(before, opened, sealed, footprint(state));
  assert!(
    !one_unit || turns > packs,
    "{turns} turns for {packs} packs"
  );
  Shown { dir, presented }
}

/// The footprint around one view (module docs): more held and charged once open, more charged
/// once sealed, and all of it given back at the close.
fn assert_balanced(before: (u64, u64), opened: (u64, u64), sealed: u64, closed: (u64, u64)) {
  assert!(
    opened.0 > before.0 && opened.1 > before.1,
    "open: {before:?}, then {opened:?}"
  );
  assert!(sealed > opened.1, "sealed: {opened:?}, then {sealed}");
  assert_eq!(closed, before, "given back");
}

/// `main` at a first commit, then a push of a second over it, seen by `pre-receive` in its
/// quarantine (sealed a unit of work a turn) and by `update` among the packs, then appended and
/// seen by `post-receive`.
#[derive(Debug)]
struct Scene {
  /// `pre-receive`'s view.
  pre_receive: Shown,
  /// `update`'s view.
  update: Shown,
  /// `post-receive`'s view.
  post_receive: Shown,
  /// `main`'s head before the push, in hex.
  old: String,
  /// The pushed head, in hex.
  new: String,
}

/// [`Scene`], made on the owner shard.
fn scene() -> Scene {
  crate::daemon::audit_on_shard(|state| {
    let (repository, main) = repository(state, "hooked", VcsTool::Git);
    let readme = |text: &[u8]| [(b"README".to_vec(), text.to_vec())];
    let (first, old) = commit_of(&readme(b"first\n"), &[], FIRST_DATE);
    let read = push_through(
      state,
      (repository, main),
      &first,
      (None, old, tree_of(&first)),
    );
    let (second, new) = commit_of(&readme(b"second\n"), &[old], FIRST_DATE + 1);
    let push = begun(state, repository, &second);
    let base = (read, Some(tree_of(&first)));
    let work = work_of(state, (&push, main), base, (new, tree_of(&second)));
    let mut sealed = push.seal(state).expect("sealed");
    let refs = [ReadRef {
      name: MAIN,
      green: main,
      version: read,
    }];
    let quarantined = Some(ShownPush::Quarantined(&sealed));
    let pre_receive = shown(state, (repository, &refs), quarantined, true);
    let migrated = Some(ShownPush::Migrated(&sealed));
    let update = shown(state, (repository, &refs), migrated, false);
    let changes = vec![BranchChange::Update { green: main, work }];
    let moves = [(Target::Green(main), Some(new))];
    let made = appended(state, repository, changes, (&mut sealed, &moves)).expect("appended");
    sealed.abandon(state);
    let Some(BranchAppended::Moved { version, .. }) = made.changes.first() else {
      panic!("not moved: {:?}", made.changes);
    };
    let after = [ReadRef {
      name: MAIN,
      green: main,
      version: *version,
    }];
    let post_receive = shown(state, (repository, &after), None, false);
    Scene {
      pre_receive,
      update,
      post_receive,
      old: hex(old),
      new: hex(new),
    }
  })
}

// ---------------------------------------------------------------------------------------------
// The hook.

/// The hook this binary plays when re-invoked as one ([`ROLE`]): it reports its working
/// directory, the variables it was handed, what it reads of the repository it runs in, and what
/// stock `git` says of the pushed head there; a test that does nothing otherwise.
#[test]
fn a_git_hook_role() {
  if std::env::var_os(ROLE).is_none() {
    return;
  }
  let mut stdin = Vec::new();
  let _ = std::io::stdin().read_to_end(&mut stdin);
  let args: Vec<String> = std::env::args()
    .skip_while(|arg| arg != "--")
    .skip(1)
    .collect();
  println!("{TAG}cwd={}", std::env::current_dir().unwrap().display());
  for name in VARIABLES {
    let value = std::env::var(name).unwrap_or_else(|_| UNSET.to_owned());
    println!("{TAG}{name}={value}");
  }
  report_files();
  report_git(&pushed_head(&stdin, &args));
  std::process::exit(0);
}

/// The pushed head: the second word of `pre-receive`'s and `post-receive`'s first line, else
/// `update`'s third argument.
fn pushed_head(stdin: &[u8], args: &[String]) -> String {
  String::from_utf8_lossy(stdin)
    .split_whitespace()
    .nth(1)
    .map(str::to_owned)
    .or_else(|| args.get(2).cloned())
    .unwrap_or_default()
}

/// What the role reads of the repository it runs in: `HEAD`, each line of `packed-refs`, and each
/// pack file of the object directory its `git` writes to, with its first four bytes in hex.
fn report_files() {
  let head = std::fs::read_to_string("HEAD").unwrap_or_default();
  println!("{TAG}HEAD={}", head.trim_end());
  for line in std::fs::read_to_string("packed-refs")
    .unwrap_or_default()
    .lines()
  {
    println!("{TAG}packed-refs={line}");
  }
  let objects = std::env::var("GIT_OBJECT_DIRECTORY").unwrap_or_else(|_| "objects".to_owned());
  let mut packs: Vec<(String, String)> = std::fs::read_dir(Path::new(&objects).join("pack"))
    .map(|entries| {
      entries
        .filter_map(Result::ok)
        .map(|entry| {
          (
            entry.file_name().to_string_lossy().into_owned(),
            magic(&entry.path()),
          )
        })
        .collect()
    })
    .unwrap_or_default();
  packs.sort();
  for (name, magic) in packs {
    println!("{TAG}pack={name}:{magic}");
  }
}

/// The first four bytes of the file at `path`, in hex.
fn magic(path: &Path) -> String {
  let mut first = [0_u8; 4];
  match std::fs::File::open(path).and_then(|mut file| file.read_exact(&mut first)) {
    Ok(()) => first.iter().map(|byte| format!("{byte:02x}")).collect(),
    Err(_) => "short".to_owned(),
  }
}

/// Stock `git`, in the hook's own environment, asked about the pushed head `new`: its type, what
/// `refs/heads/main` names, and how many commits it reaches, each answer `stdout|stderr`;
/// `git=absent` where no `git` runs.
fn report_git(new: &str) {
  let asked: [(&str, Vec<&str>); 3] = [
    ("type", vec!["cat-file", "-t", new]),
    ("main", vec!["rev-parse", "refs/heads/main"]),
    ("count", vec!["rev-list", "--count", new]),
  ];
  for (what, args) in asked {
    let Ok(out) = Command::new("git").args(&args).output() else {
      println!("{TAG}git=absent");
      return;
    };
    let said = String::from_utf8_lossy(&out.stdout);
    let warned = String::from_utf8_lossy(&out.stderr);
    println!("{TAG}git {what}={}|{}", said.trim(), warned.trim());
  }
}

/// A one-shard runtime for a driven run.
pub(crate) fn runtime() -> LocalRuntime {
  LocalRuntime::new(&RuntimeConfig {
    shards: 1,
    tasks_per_shard: 8,
    timers_per_shard: 8,
    ring_entries: 8,
    step_budget_ns: 1_000_000,
    timer_tick_ns: 100_000,
    batch: 8,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
    wake_tracking: None,
  })
  .expect("a local runtime")
}

/// A policy with the test binary as the hook at each of git's points.
fn policy() -> RepositoryPolicy {
  let program = std::env::current_exe()
    .expect("the test binary")
    .to_string_lossy()
    .into_owned();
  let hook = |point, name: &str| HookConfig {
    point,
    name: name.to_owned(),
    program: program.clone(),
  };
  RepositoryPolicy {
    version: 1,
    hooks: vec![
      hook(HookPointName::GitPreReceive, "pre-receive"),
      hook(HookPointName::GitUpdate, "update"),
      hook(HookPointName::GitPostReceive, "post-receive"),
    ],
    ..RepositoryPolicy::default()
  }
}

/// `input` as the role's: libtest's arguments before its own, and the role named.
fn as_role(mut input: HookInput) -> HookInput {
  let mut args: Vec<Vec<u8>> = [ROLE_TEST, "--exact", "--nocapture", "--"]
    .iter()
    .map(|arg| arg.as_bytes().to_vec())
    .collect();
  args.append(&mut input.args);
  input.args = args;
  input
    .env
    .push((ROLE.to_owned(), EnvValue::Bytes(b"report".to_vec())));
  input
}

/// Runs `input`'s hook under `policy` in `presented`, as the role, with no learned bound and a
/// client that stays; the test gives up past [`TEST_BOUND_NS`]. None when the run's task could not
/// be spawned or the run did not answer within the bound: the test fails on it.
fn ran(
  presented: &Presented,
  policy: &RepositoryPolicy,
  input: HookInput,
) -> Option<Result<GitHookRan, GitHookRefused>> {
  let runtime = runtime();
  let (sent, got) = channel();
  let (presented, policy, input) = (presented.clone(), policy.clone(), as_role(input));
  runtime
    .spawn(async move {
      let site = GitHookSite {
        policy: &policy,
        view: &presented,
        words_cap: CAP,
      };
      let running = run(&site, &input, None, std::future::pending());
      let answered = slates_rt::futures::within(TEST_BOUND_NS, running).await;
      let _ = sent.send(answered.ok().flatten());
    })
    .ok()?;
  runtime.run_until_idle();
  got.recv().ok().flatten()
}

/// The lines an accepted hook's role wrote, its tag stripped, in order. libtest writes a test's
/// name without ending its line, so a tag is looked for anywhere in a line.
fn lines_of(ran: &GitHookRan) -> Vec<String> {
  let Some(Outcome::Accepted { words }) = &ran.outcome else {
    panic!("not accepted: {ran:?}");
  };
  String::from_utf8_lossy(&words.kept)
    .lines()
    .filter_map(|line| line.split_once(TAG).map(|(_, said)| said.to_owned()))
    .collect()
}

/// The first bytes of each pack file the role listed, in its order (by name: an index before
/// its pack).
fn magics(lines: &[String]) -> Vec<String> {
  lines
    .iter()
    .filter_map(|line| line.strip_prefix("pack="))
    .filter_map(|listed| listed.rsplit_once(':').map(|(_, magic)| magic.to_owned()))
    .collect()
}

/// Each of `expected` among `lines`.
fn assert_lines(lines: &[String], expected: &[String]) {
  for line in expected {
    assert!(lines.contains(line), "{line} in {lines:#?}");
  }
}

/// What every point's hook sees of a view rooted at `root` with `main` at `main_head`: its working
/// directory the root, `GIT_DIR=.`, `HEAD` naming `main`, `packed-refs` naming `main_head`.
fn assert_in_view(lines: &[String], root: &Path, main_head: &str) {
  assert_lines(
    lines,
    &[
      format!("cwd={}", root.display()),
      "GIT_DIR=.".to_owned(),
      "HEAD=ref: refs/heads/main".to_owned(),
      format!("packed-refs={main_head} refs/heads/main"),
    ],
  );
}

/// The one quarantine directory of the view presented at `root`.
fn quarantine_in(root: &Path) -> String {
  let names: Vec<String> = std::fs::read_dir(root.join("objects"))
    .expect("objects")
    .filter_map(Result::ok)
    .map(|entry| entry.file_name().to_string_lossy().into_owned())
    .filter(|name| name.starts_with("tmp_objdir-incoming-"))
    .collect();
  assert_eq!(names.len(), 1, "{names:?}");
  names.into_iter().next().unwrap_or_default()
}

/// `pre-receive`'s quarantine as stock receive-pack hands it (A-665's `fixtures/view`): the object
/// directory and the quarantine path the view's one `tmp_objdir-incoming-` directory, as an
/// absolute path through `./objects`; the repository's objects the alternate; the push options'
/// count `0`; and the push's pack alone in the quarantine, an index and a pack by their first
/// bytes.
fn assert_quarantined(lines: &[String], root: &Path) {
  let objects = format!("{}/./objects", root.display());
  let quarantine = format!("{objects}/{}", quarantine_in(root));
  assert_lines(
    lines,
    &[
      format!("GIT_OBJECT_DIRECTORY={quarantine}"),
      format!("GIT_QUARANTINE_PATH={quarantine}"),
      format!("GIT_ALTERNATE_OBJECT_DIRECTORIES={objects}"),
      "GIT_PUSH_OPTION_COUNT=0".to_owned(),
    ],
  );
  assert_eq!(magics(lines), [IDX_MAGIC, PACK_MAGIC]);
}

/// `update`'s and `post-receive`'s view: no quarantine, so none of its variables; the push
/// options' count `count`; at least `packs` packs in `objects/pack`, each an index and a pack by
/// their first bytes.
fn assert_unquarantined(lines: &[String], count: &str, packs: usize) {
  assert_lines(
    lines,
    &[
      format!("GIT_OBJECT_DIRECTORY={UNSET}"),
      format!("GIT_QUARANTINE_PATH={UNSET}"),
      format!("GIT_ALTERNATE_OBJECT_DIRECTORIES={UNSET}"),
      format!("GIT_PUSH_OPTION_COUNT={count}"),
    ],
  );
  let magics = magics(lines);
  let pair = [IDX_MAGIC, PACK_MAGIC];
  assert!(
    magics.len() >= 2 * packs && magics.chunks(2).all(|files| files == pair.as_slice()),
    "{magics:?}"
  );
}

/// What stock `git`, run by the hook in its view, said of the pushed head `new`: a commit
/// reaching two, and `main` at `main_head`; skipped loudly where no `git` runs.
fn assert_stock_git(lines: &[String], new: &str, main_head: &str) {
  if lines.iter().any(|line| line == "git=absent") {
    eprintln!("SKIP stock git over the view: no `git` runs here; the hook read the view alone");
    return;
  }
  assert!(!new.is_empty(), "the hook read no pushed head");
  assert_lines(
    lines,
    &[
      "git type=commit|".to_owned(),
      format!("git main={main_head}|"),
      "git count=2|".to_owned(),
    ],
  );
}

/// A-665 part 2 (a git hook runs in its view at each of receive-pack's points). Do: push a second
/// commit to `main` over a first; on the owner shard open the push's views (`pre-receive`'s with
/// the push quarantined, `update`'s with it among the packs, `post-receive`'s once the push is
/// appended), seal each, the first a unit of work a turn, present each in a scratch directory and
/// close it; run the test binary as each point's hook there. Expect: each view held and charged
/// while open, its seals charged on top, everything given back at its close, and the sliced seal
/// taking more turns than packs; each hook accepted, run in its view's root with `GIT_DIR=.`,
/// `HEAD` naming `main` and `packed-refs` naming `main`'s head as the push read it (the pushed head
/// after the append); `pre-receive` handed git's quarantine as stock receive-pack hands it, the
/// push's pack alone in it; `update` and `post-receive` handed none, the push's pack among the
/// repository's for `update`; and stock `git`, where it runs, finding in each view the pushed
/// commit and the two commits it reaches.
#[test]
fn a_git_hook_runs_in_its_view_at_each_of_receive_packs_points() {
  let scene = scene();
  let policy = policy();
  let moved = RefUpdate {
    old: scene.old.clone(),
    new: scene.new.clone(),
    name: "refs/heads/main".to_owned(),
  };
  let input = git_pre_receive(PendingView(0), std::slice::from_ref(&moved), None);
  let pre = lines_of(
    &ran(&scene.pre_receive.presented, &policy, input)
      .expect("the run answered within the test's bound")
      .expect("ran"),
  );
  assert_in_view(&pre, &scene.pre_receive.dir.path, &scene.old);
  assert_quarantined(&pre, &scene.pre_receive.dir.path);
  assert_stock_git(&pre, &scene.new, &scene.old);
  let input = git_update(PendingView(0), &moved);
  let update = lines_of(
    &ran(&scene.update.presented, &policy, input)
      .expect("the run answered within the test's bound")
      .expect("ran"),
  );
  assert_in_view(&update, &scene.update.dir.path, &scene.old);
  assert_unquarantined(&update, UNSET, 2);
  assert_stock_git(&update, &scene.new, &scene.old);
  let input = git_post_receive(CommittedView(0), std::slice::from_ref(&moved), None);
  let post = lines_of(
    &ran(&scene.post_receive.presented, &policy, input)
      .expect("the run answered within the test's bound")
      .expect("ran"),
  );
  assert_in_view(&post, &scene.post_receive.dir.path, &scene.new);
  assert_unquarantined(&post, "0", 1);
  assert_stock_git(&post, &scene.new, &scene.new);
}

/// A-665 part 2 (a view the store cannot show, refused by kind with nothing held). Do: on the
/// owner shard, with `main` pushed once, open a view at `main`'s version after the one pushed;
/// one at the pushed version then that later one; one with another repository's push; and one of
/// a Mercurial repository. Expect: `NotInStore` naming the ref's place (0, then 1), `ForeignPush`,
/// `NoStore`; and after each, the shard holding and charging what it did before.
#[test]
fn a_view_the_store_cannot_show_is_refused_by_kind_and_holds_nothing() {
  let refused = crate::daemon::audit_on_shard(|state| {
    let (hooked, main) = repository(state, "hooked", VcsTool::Git);
    let (other, _) = repository(state, "other", VcsTool::Git);
    let (mercurial, _) = repository(state, "hg", VcsTool::Mercurial);
    let (first, old) = commit_of(
      &[(b"README".to_vec(), b"first\n".to_vec())],
      &[],
      FIRST_DATE,
    );
    let read = push_through(state, (hooked, main), &first, (None, old, tree_of(&first)));
    let at = |version| ReadRef {
      name: MAIN,
      green: main,
      version,
    };
    let foreign = begun(state, other, &first).seal(state).expect("sealed");
    let before = footprint(state);
    let mut refused = Vec::new();
    for (refs, pushed, on) in [
      (vec![at(read + 1)], None, hooked),
      (vec![at(read), at(read + 1)], None, hooked),
      (
        vec![at(read)],
        Some(ShownPush::Quarantined(&foreign)),
        hooked,
      ),
      (Vec::new(), None, mercurial),
    ] {
      refused.push(GitHookView::open(state, on, (refs.as_slice(), MAIN), pushed).err());
      assert_eq!(footprint(state), before);
    }
    foreign.abandon(state);
    refused
  });
  assert_eq!(
    refused,
    [
      Some(GitHookRefused::NotInStore { at: 0 }),
      Some(GitHookRefused::NotInStore { at: 1 }),
      Some(GitHookRefused::ForeignPush),
      Some(GitHookRefused::NoStore),
    ]
  );
}

/// A view as presented at the filesystem's root, showing the push as `shows`.
fn presented(shows: Shows) -> Presented {
  Presented {
    root: PathBuf::from("/"),
    shows,
    env: Vec::new(),
  }
}

/// A-665 part 2 (a view only where its point sees it so). Do: hand a quarantine to `update` and
/// to `post-receive`, a moved push to `pre-receive` and to `post-receive`, and a Subversion
/// point's input to a view; then run a git point whose policy configures no hook. Expect: each of
/// the first refused before any process starts, naming the point and where the view shows the
/// push, the Subversion one as no point of git's; the last let through with no outcome and
/// nothing to learn.
#[test]
fn a_view_is_handed_only_to_a_point_that_sees_it_so() {
  let moved = RefUpdate {
    old: "0".repeat(40),
    new: "1".repeat(40),
    name: "refs/heads/main".to_owned(),
  };
  let update = git_update(PendingView(0), &moved);
  let post = git_post_receive(CommittedView(0), std::slice::from_ref(&moved), None);
  let pre = git_pre_receive(PendingView(0), std::slice::from_ref(&moved), None);
  let subversion = HookInput {
    point: HookPoint::SvnPreCommit,
    ..update.clone()
  };
  let cases = [
    (Shows::Quarantined, update.clone(), HookPoint::GitUpdate),
    (Shows::Quarantined, post.clone(), HookPoint::GitPostReceive),
    (Shows::Migrated, pre.clone(), HookPoint::GitPreReceive),
    (Shows::Migrated, post, HookPoint::GitPostReceive),
  ];
  for (shows, input, point) in cases {
    assert_eq!(
      ran(&presented(shows), &policy(), input),
      Some(Err(GitHookRefused::ViewForAnotherPoint { point, shows }))
    );
  }
  assert_eq!(
    ran(&presented(Shows::NoPush), &policy(), subversion),
    Some(Err(GitHookRefused::NotGits {
      point: HookPoint::SvnPreCommit
    }))
  );
  let unconfigured = RepositoryPolicy::default();
  assert_eq!(
    ran(&presented(Shows::Quarantined), &unconfigured, pre),
    Some(Ok(GitHookRan {
      outcome: None,
      lesson: None,
    }))
  );
}
