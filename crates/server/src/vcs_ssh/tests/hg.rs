//! Tests of Mercurial's `hg -R <path> serve --stdio` behind the SSH edge (A-679), a child of the edge's tests so that a
//! session runs over the real edge with their client. The golden is the probe stock `hg serve --stdio` 7.2 answered
//! (`crates/hg/fixtures/serve`, made by `make-serve-fixtures.sh`), sent after the handshake 7.2's client opens every
//! session with, over the repository the probe asked rebuilt from the fixture's listing: the command answers it byte for
//! byte as 7.2 did at every cut, and so does the edge. Then what the command adds to the session (7.2's warning to a
//! client asking for hidden changesets, the store going away, the window's bookkeeping), the services starting it, and
//! the site refusing a repository argument as 7.2's dispatch refuses it before anything is opened. Then pushes (A-696):
//! a push handed to the lender as its client sent it, by the key's principal, its reply sent back as it is; and stock
//! 7.2's push over SSH (`crates/hg/fixtures/wire`'s `ssh-push`, made by `make-wire-fixtures.sh`) replayed whole against
//! a daemon's shard holding what 7.2's server held, through the daemon's lenders, every answer after `hello` 7.2's.

use std::borrow::Cow;
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use slates_db::catalog::{
  ObjectFormat, Principal, RepositoryId, RepositoryPolicy, RepositoryRecord, VcsTool,
};
use slates_git::Admission;
use slates_git::upload::{PackPlan, PackRefused, PackSource, Served};
use slates_hg::obsmarkers::Marker;
use slates_hg::phases::Phase;
use slates_hg::push::reply::{Replied, Reply};
use slates_hg::push::{Asks, BookmarkAt, Outcome, Taken, Unbundle};
use slates_hg::serve::getbundle::{Logged, Store};
use slates_hg::serve::stdio::{EXIT_STATUS, Progress, Stdio};
use slates_hg::serve::{Repository, Rev};
use slates_hg::{NULL_NODE, Node, capabilities, pushkey};
use slates_ipc::protocol::{ObjectFormat as IpcFormat, ReplyBody, RequestBody, VcsTool as IpcTool};
use slates_rt::tcp::SocketAddrV4;
use slates_ssh::command::Command as Asked;

use super::super::hg::HgCommand;
use super::super::site::{
  ClientGone, Command, Dispatched, HG_ABORT_STATUS, Next, Refused, Service, Services, SshSite,
  Start, Stepped, ToolCommand, ToolServices, ToolSite, Unserved,
};
use super::{
  HELD_SEED, HOLDER, Keys, ONE_SEAT, Peer, ROOM_BYTES, TICK_NS, Transcript, exec, query, send_data,
  served_by, sign, transcript,
};
use crate::repository::hg_push::tests::{Commit, changegroup, history_of};
use crate::repository::hg_push::{self, Pushing, line_name};
use crate::state::ShardState;
use crate::vcs_http::Bounds;
use crate::vcs_http::git::{Located, Repositories, StoreLender, ThisShard};
use crate::vcs_http::hg::HgStores;
use crate::vcs_store::hg_view::ServedViews;
use crate::vcs_store::upload::ShardStores;

/// Shape: the repository whose store the tests' lender holds.
const HG: RepositoryId = RepositoryId { bytes: [3; 16] };
/// Shape: a Mercurial repository whose store no lender here holds.
const UNHELD: RepositoryId = RepositoryId { bytes: [4; 16] };
/// Shape: the name the tests' clients reach [`HG`] by.
const HG_NAME: &str = "hg/r";
/// Shape: a name only the site's refusal of a repository argument starting `--` keeps from being served: it names
/// [`HG`] too.
const DASHED_NAME: &str = "--x";

/// Shape: the probe of stock `hg serve --stdio` 7.2 (`crates/hg/fixtures/serve/make-serve-fixtures.sh`): the served view
/// it answered from, oldest first, as the script's `dump` lists it.
const PROBE_STATE: &str = include_str!("../../../../hg/fixtures/serve/state-probe.txt");
/// Shape: the probed repository's bookmarks, as `dump` lists them.
const PROBE_BOOKMARKS: &str = include_str!("../../../../hg/fixtures/serve/bookmarks-probe.txt");
/// Shape: what the probe wrote to the server's standard input: eight requests as 7.2's `sshv1peer` writes them.
const PROBE_CLIENT: &[u8] = include_bytes!("../../../../hg/fixtures/serve/probe.client");
/// Shape: what the server wrote on its standard output in answer, each answer its length on a line and its bytes.
const PROBE_SERVER: &[u8] = include_bytes!("../../../../hg/fixtures/serve/probe.server");

/// Format: the request 7.2's client opens every session with (`sshpeer.py` `_performhandshake`).
const HELLO: &[u8] = b"hello\n";
/// Format: the request it sends next: `between` with the null pair.
const NULL_PAIR: &[u8] = concat!(
  "between\npairs 81\n",
  "0000000000000000000000000000000000000000-0000000000000000000000000000000000000000"
)
.as_bytes();
/// Format: 7.2's answer to the null pair (`wireprotov1server.py` `between`): one empty line, its length first.
const NULL_BETWEEN: &[u8] = b"1\n\n";
/// Format: a whole `heads` request, which takes no argument.
const HEADS: &[u8] = b"heads\n";
/// Shape: the cuts the client's bytes are handed to the command in besides the whole: a byte, seven bytes and a kilobyte
/// at a time.
const CUTS: [usize; 3] = [1, 7, 1024];

/// Format: 7.2's warning to a user its server does not let see hidden changesets (`commands.py` `serve`), before the
/// user it names.
const HIDDEN_WARNING: &str = "ignoring request to access hidden changeset by unauthorized user: ";
/// Format: 7.2's refusal of a `serve --stdio` whose repository argument starts `--` (`dispatch.py` `_runcatchfunc`),
/// before the arguments it lists.
const UNSAFE_INVOCATION: &str = "abort: potentially unsafe serve --stdio invocation: ";
/// Format: each repository argument stock Mercurial 7.2 was run with (`hg -R <path> serve --stdio`, with `--hidden`
/// after it where marked) and the arguments its refusal listed, recorded 2026-11-19 (Homebrew's 7.2 under
/// `HGPLAIN=1 HGRCPATH=` in an empty directory; standard output empty and exit status 255 for every one).
const UNSAFE: [(&[u8], bool, &str); 6] = [
  (b"--", false, r"['-R', '--', 'serve', '--stdio']"),
  (b"--x", false, r"['-R', '--x', 'serve', '--stdio']"),
  (
    b"--x",
    true,
    r"['-R', '--x', 'serve', '--stdio', '--hidden']",
  ),
  (
    b"--y'\xff",
    true,
    r"['-R', '--y\'\xff', 'serve', '--stdio', '--hidden']",
  ),
  (
    b"--a\tb\nc\rd\\e\x7fh\x01\"g",
    false,
    r#"['-R', '--a\tb\nc\rd\\e\x7fh\x01"g', 'serve', '--stdio']"#,
  ),
  (
    b"--\xc3\xa9",
    false,
    r"['-R', '--\xc3\xa9', 'serve', '--stdio']",
  ),
];
/// Format: a repository argument starting with one `-`, which 7.2 looked up and refused as missing, `--hidden` or not
/// (recorded with [`UNSAFE`]'s).
const ONE_DASH: &[u8] = b"-x";

/// Shape: a room of 24 bytes, and so a window of 24: too small for the null pair's argument, 81 bytes.
const SMALL_ROOM: u64 = 24;
/// Shape: 25 bytes: one past [`SMALL_ROOM`]'s window.
const PAST_SMALL_ROOM: usize = 25;
/// Shape: a request whose argument says 81 bytes follow: past [`SMALL_ROOM`]'s room, which refuses it before waiting
/// for them (slates-hg's `ssh::arguments`).
const TOO_LONG_REQUEST: &[u8] = b"between\npairs 81\n";

/// The room each request is admitted, and the window over it: the edge's, as the tests' edge derives it.
fn room() -> u64 {
  Bounds::of_shard(ONE_SEAT, ROOM_BYTES, TICK_NS).room
}

/// One changeset of the probed repository.
struct Changeset {
  /// Its node.
  node: Node,
  /// Its parents' revisions.
  parents: [Option<Rev>; 2],
  /// Its branch.
  branch: Vec<u8>,
  /// Whether it closes its branch.
  closes: bool,
  /// Its phase.
  phase: Phase,
}

/// The repository the probe asked, rebuilt from the fixture's listing: non-publishing, as the script's `hgrc` makes it.
/// The probe reads no text, so none is held.
#[derive(Default)]
struct Probed {
  /// Its changesets, oldest first.
  changesets: Vec<Changeset>,
  /// Each changeset's revision by its node.
  revisions: HashMap<Node, Rev>,
  /// Its bookmarks: each name and the node it names.
  bookmarks: Vec<(Vec<u8>, Node)>,
}

/// The node `hex` names.
fn node(hex: &str) -> Node {
  pushkey::hex_node(hex.as_bytes())
    .expect("a node")
    .expect("forty digits")
}

impl Probed {
  /// The repository `state` and `bookmarks` list (`make-serve-fixtures.sh`'s `dump`): each state line a node, its two
  /// parents, its phase, `1` where it closes its branch, then `|` and its branch; each bookmarks line a node and a name.
  fn listed(state: &str, bookmarks: &str) -> Probed {
    let mut probed = Probed::default();
    for line in state.lines() {
      let (fields, branch) = line.split_once('|').expect("a branch");
      let fields: Vec<&str> = fields.split(' ').collect();
      let parent = |hex: &str| (node(hex) != NULL_NODE).then(|| probed.revisions[&node(hex)]);
      let parents = [parent(fields[1]), parent(fields[2])];
      let phase = if fields[3] == "public" {
        Phase::Public
      } else {
        Phase::Draft
      };
      let rev = Rev::try_from(probed.changesets.len()).expect("a revision");
      probed.revisions.insert(node(fields[0]), rev);
      probed.changesets.push(Changeset {
        node: node(fields[0]),
        parents,
        branch: branch.as_bytes().to_vec(),
        closes: fields[4] == "1",
        phase,
      });
    }
    for line in bookmarks.lines() {
      let (hex, name) = line.split_once(' ').expect("a name");
      probed.bookmarks.push((name.as_bytes().to_vec(), node(hex)));
    }
    probed
  }

  /// The changeset at `rev`.
  fn changeset(&self, rev: Rev) -> Option<&Changeset> {
    usize::try_from(rev)
      .ok()
      .and_then(|at| self.changesets.get(at))
  }
}

impl Repository for Probed {
  fn count(&self) -> Rev {
    Rev::try_from(self.changesets.len()).expect("a revision count")
  }

  fn node(&self, rev: Rev) -> Option<Node> {
    self.changeset(rev).map(|changeset| changeset.node)
  }

  fn rev(&self, node: &Node) -> Option<Rev> {
    self.revisions.get(node).copied()
  }

  fn parents(&self, rev: Rev) -> [Option<Rev>; 2] {
    self
      .changeset(rev)
      .map_or([None, None], |changeset| changeset.parents)
  }

  fn branch(&self, rev: Rev) -> &[u8] {
    self
      .changeset(rev)
      .map_or(&b""[..], |changeset| changeset.branch.as_slice())
  }

  fn closes(&self, rev: Rev) -> bool {
    self
      .changeset(rev)
      .is_some_and(|changeset| changeset.closes)
  }

  fn phase(&self, rev: Rev) -> Phase {
    self
      .changeset(rev)
      .map_or(Phase::Public, |changeset| changeset.phase)
  }

  fn publishing(&self) -> bool {
    false
  }

  fn bookmarks(&self) -> impl Iterator<Item = (&[u8], Node)> {
    self
      .bookmarks
      .iter()
      .map(|(name, node)| (name.as_slice(), *node))
  }

  /// The repository is the served changesets a probe listed and hides nothing, so the roots its register would keep
  /// are its draft changesets none of whose parents is draft (A-691).
  fn draft_roots(&self) -> impl Iterator<Item = Node> {
    self
      .changesets
      .iter()
      .filter(|changeset| {
        changeset.phase == Phase::Draft
          && changeset
            .parents
            .iter()
            .flatten()
            .all(|parent| self.phase(*parent) == Phase::Public)
      })
      .map(|changeset| changeset.node)
  }
}

impl Store for Probed {
  fn changeset_text(
    &self,
    _rev: Rev,
    _room: &mut slates_hg::Admission,
  ) -> Result<Option<Cow<'_, [u8]>>, slates_hg::PastAdmission> {
    Ok(None)
  }

  fn manifest(
    &self,
    _node: &Node,
    _room: &mut slates_hg::Admission,
  ) -> Result<Option<Logged<'_>>, slates_hg::PastAdmission> {
    Ok(None)
  }

  fn file(
    &self,
    _path: &[u8],
    _node: &Node,
    _room: &mut slates_hg::Admission,
  ) -> Result<Option<Logged<'_>>, slates_hg::PastAdmission> {
    Ok(None)
  }

  fn markers(&self) -> impl Iterator<Item = Marker<'_>> {
    std::iter::empty()
  }

  fn manifest_linked(&self, _position: u64) -> Option<slates_hg::serve::getbundle::Linked> {
    None
  }

  fn file_linked_from(
    &self,
    _from: (&[u8], u64),
  ) -> Option<(Vec<u8>, u64, slates_hg::serve::getbundle::Linked)> {
    None
  }

  fn holds_manifest(&self, _node: &Node) -> bool {
    false
  }

  fn serves_every_changeset(&self) -> bool {
    true
  }
}

/// Lends the probed repository's store as [`HG`]'s, and no other, until the test says it went away.
#[derive(Clone, Copy)]
struct Lends {
  /// The store, kept for the test's life.
  probed: &'static Probed,
  /// Whether the store went away; it never does when none. A word every shard may read, since the edge's harness makes
  /// a site on each shard from one maker (A-651 part 4).
  gone: Option<&'static AtomicBool>,
}

impl Lends {
  /// A lender of the probed repository's store.
  fn probed() -> Lends {
    Lends {
      probed: Box::leak(Box::new(Probed::listed(PROBE_STATE, PROBE_BOOKMARKS))),
      gone: None,
    }
  }
}

impl HgStores for Lends {
  type Store<'s> = Probed;
  type Hold = RepositoryId;

  fn hold(&mut self, id: RepositoryId, _room: &mut slates_hg::Admission) -> Option<RepositoryId> {
    self.holds(id).then_some(id)
  }

  fn lend_held<R>(
    &mut self,
    hold: &RepositoryId,
    room: &mut slates_hg::Admission,
    step: impl for<'s> FnOnce(&Self::Store<'s>, &mut slates_hg::Admission) -> R,
  ) -> Option<R> {
    self.lend(*hold, room, step)
  }

  fn holds(&self, id: RepositoryId) -> bool {
    id == HG && !self.gone.is_some_and(|gone| gone.load(Ordering::Acquire))
  }

  fn lend<R>(
    &mut self,
    id: RepositoryId,
    room: &mut slates_hg::Admission,
    step: impl for<'s> FnOnce(&Self::Store<'s>, &mut slates_hg::Admission) -> R,
  ) -> Option<R> {
    self.holds(id).then(|| step(self.probed, room))
  }

  /// A push is answered with what reached the lender ([`echoed`]): a reply whose one `output` part holds it.
  async fn push(
    &mut self,
    id: RepositoryId,
    pushing: Pushing<'_>,
    _room: &mut slates_hg::Admission,
  ) -> Option<Outcome> {
    let echo = Replied::Output {
      in_reply_to: None,
      words: echoed(pushing),
    };
    self
      .holds(id)
      .then(|| Outcome::Accepted(Reply::accepted(&[echo], true)))
  }
}

/// What a push handed to the lender holds, as the tests' lender echoes it: its principal and its `heads` argument as
/// `Debug` writes them, a line end, then its bundle.
fn echoed((principal, heads, bundle): Pushing<'_>) -> Vec<u8> {
  let mut words = format!("{principal:?} {heads:?}\n").into_bytes();
  words.extend_from_slice(bundle);
  words
}

/// The pack source of a git lender that lends nothing: no value has this type.
#[derive(Clone, Copy)]
enum NoPacks {}

impl PackSource for NoPacks {
  type Pack = ();

  fn begin(
    &mut self,
    _served: &Served<'_>,
    _plan: &PackPlan<'_>,
    _admission: &mut Admission,
  ) -> Result<(), PackRefused> {
    match *self {}
  }

  fn write(
    &mut self,
    _served: &Served<'_>,
    _pack: &mut (),
    _admission: &mut Admission,
    _out: &mut Vec<u8>,
    _room: usize,
  ) -> Result<bool, PackRefused> {
    match *self {}
  }
}

/// Lends no git store: these tests serve Mercurial alone.
#[derive(Clone, Copy)]
struct NoGit;

impl StoreLender for NoGit {
  type Packs = NoPacks;

  fn holds(&self, _id: RepositoryId) -> bool {
    false
  }

  fn room(&mut self, _repository: RepositoryId) -> Option<u64> {
    None
  }

  fn lend<R>(
    &mut self,
    _asked: (RepositoryId, &Principal),
    _step: impl FnOnce(&Served<'_>, &mut NoPacks) -> R,
  ) -> Option<R> {
    None
  }

  fn lend_objects<R>(
    &mut self,
    _id: RepositoryId,
    _step: impl FnOnce(&Served<'_>, &mut NoPacks) -> R,
  ) -> Option<R> {
    None
  }
}

/// The Mercurial repository `id`, [`HOLDER`]'s, with no rules.
fn record(id: RepositoryId, name: &str) -> RepositoryRecord {
  RepositoryRecord {
    index_bits: 0,
    id,
    name: name.to_owned(),
    tool: VcsTool::Mercurial,
    format: ObjectFormat::Sha1,
    default_branch: None,
    owner: HOLDER,
    policy: RepositoryPolicy {
      version: 1,
      rules: Vec::new(),
      hooks: Vec::new(),
      hooks_env: Vec::new(),
      signers: Vec::new(),
    },
  }
}

/// Holds [`HG`] by the names [`HG_NAME`] and [`DASHED_NAME`].
struct OneRepository;

impl Repositories for OneRepository {
  fn locate(&self, _name: &str) -> Located {
    Located::Here
  }

  fn find(&self, name: &str) -> Option<RepositoryRecord> {
    [HG_NAME, DASHED_NAME]
      .contains(&name)
      .then(|| record(HG, name))
  }
}

/// The site the edge serves Mercurial with: the production site over [`OneRepository`], its services over the probed
/// store and no git store, each request admitted the edge's room.
type HgSite = ToolSite<Keys, OneRepository, ToolServices<NoGit, Lends, ThisShard>>;

/// The production site over the store `lends` lends.
fn site(lends: Lends) -> HgSite {
  ToolSite::new(
    Keys,
    OneRepository,
    ToolServices::new(NoGit, lends, ThisShard, room()),
  )
}

/// `future`'s output, polled once: the command's steps never wait.
fn ready<F: Future>(future: F) -> F::Output {
  let mut future = std::pin::pin!(future);
  match future
    .as_mut()
    .poll(&mut Context::from_waker(Waker::noop()))
  {
    Poll::Ready(output) => output,
    Poll::Pending => panic!("a step that waits"),
  }
}

/// What a command gave its channel: standard error and output as the edge sends them, the input it consumed (given back
/// to the window), and its exit status once it exited.
#[derive(Debug, Default, PartialEq, Eq)]
struct Channel {
  /// Standard error.
  stderr: Vec<u8>,
  /// Standard output.
  stdout: Vec<u8>,
  /// The input consumed.
  consumed: usize,
  /// The exit status: its own, or its refusal's.
  exited: Option<u32>,
}

impl Channel {
  /// Takes everything `command` wrote, standard error first, as a channel with room for all of it does.
  fn take<C: Command>(&mut self, command: &mut C) {
    let (stderr, stdout) = command.output();
    let taken = (stderr.len(), stdout.len());
    self.stderr.extend_from_slice(stderr);
    self.stdout.extend_from_slice(stdout);
    command.sent(taken.0, taken.1);
  }

  /// Drives `command` as the edge does until it waits for input or exits: what it wrote is taken before each step, and a
  /// refusal's words are sent with its status.
  fn drive<C: Command>(&mut self, command: &mut C) {
    ready(self.drive_awaiting(command));
  }

  /// [`Channel::drive`] awaiting each step: a step over the daemon's lenders may yield between its slices (A-720: a
  /// push taken into its fork a slice at a time), which the shard's executor serves as it does on the edge.
  async fn drive_awaiting<C: Command>(&mut self, command: &mut C) {
    while self.exited.is_none() {
      self.take(command);
      match command.step(ClientGone::never()).await {
        Ok(Stepped { consumed, next }) => {
          self.consumed += consumed;
          match next {
            Next::More => {}
            Next::Input => return,
            Next::Exited(status) => {
              self.take(command);
              self.exited = Some(status);
            }
          }
        }
        Err(refused) => {
          self.stderr.extend(refused.stderr);
          self.exited = Some(refused.exit_status);
        }
      }
    }
  }
}

/// A session over the store `lends` lends, the principal `hidden` names asking for hidden changesets, handed `input` in
/// pieces of `cut` bytes, driven after each, then the end of its input: what its channel took.
fn session(lends: Lends, hidden: Option<&Principal>, input: &[u8], cut: usize) -> Channel {
  let principal = hidden.cloned().unwrap_or(HOLDER);
  let mut command = HgCommand::new(lends, (HG, principal), room(), hidden.is_some());
  let mut channel = Channel::default();
  for piece in input.chunks(cut) {
    command.input(piece).expect("within the window");
    channel.drive(&mut command);
  }
  command.input_end();
  channel.drive(&mut command);
  channel
}

/// The session's answer to `hello` alone over `store`: its capabilities, which slates-hg's golden holds to 7.2's.
fn hello_answer(store: &Probed) -> Vec<u8> {
  let mut stdio = Stdio::new(HELLO.len());
  stdio.receive(HELLO).expect("within the window");
  assert!(matches!(
    stdio.step(store, room()),
    Progress::Stepped { .. }
  ));
  stdio.stdout().to_vec()
}

/// Whether `answer` is a `hello` answer as SSH version 1 frames one: its length on a line, then `capabilities: ` and the
/// rest of that length.
fn frames_capabilities(answer: &[u8]) -> bool {
  let Some(at) = answer.iter().position(|byte| *byte == b'\n') else {
    return false;
  };
  let length = std::str::from_utf8(&answer[..at])
    .ok()
    .and_then(|digits| digits.parse::<usize>().ok());
  let rest = &answer[at + 1..];
  length == Some(rest.len()) && rest.starts_with(b"capabilities: ")
}

/// What a client reads after opening a session as 7.2's does and sending the probe: the session's capabilities, 7.2's
/// answer to the null pair, then 7.2's answers to the probe, byte for byte.
fn probe_answered(store: &Probed) -> Vec<u8> {
  [hello_answer(store).as_slice(), NULL_BETWEEN, PROBE_SERVER].concat()
}

/// 7.2's warning to `principal`, who asked for hidden changesets.
fn hidden_warning(principal: &Principal) -> Vec<u8> {
  format!("{HIDDEN_WARNING}{principal:?}\n").into_bytes()
}

/// A-679 (golden; §4.17.3 piece 4; AC-HG.1). Do: open a session as 7.2's client does (`hello`, then `between` with the
/// null pair), then send the probe stock `hg serve --stdio` 7.2 answered (`heads`, `known`, `between`, `branches`,
/// `branchmap` and `listkeys` of three namespaces) over the repository it asked, a byte, seven bytes, a kilobyte and the
/// whole at a time, and end the input. Expect, at every cut: on standard output the session's capabilities framed as SSH
/// version 1 frames an answer, then 7.2's answer to the null pair and 7.2's eight answers to the probe, byte for byte;
/// nothing on standard error; every byte consumed, so given back to the window; and the command exited 0, as
/// `hg serve --stdio` exits at the end of its input.
#[test]
fn the_probe_is_answered_as_stock_7_2_answered_it_at_every_cut() {
  let lends = Lends::probed();
  let input = [HELLO, NULL_PAIR, PROBE_CLIENT].concat();
  let expected = Channel {
    stderr: Vec::new(),
    stdout: probe_answered(lends.probed),
    consumed: input.len(),
    exited: Some(EXIT_STATUS),
  };
  assert!(frames_capabilities(&hello_answer(lends.probed)));
  for cut in CUTS.into_iter().chain([input.len()]) {
    assert_eq!(session(lends, None, &input, cut), expected, "cut {cut}");
  }
}

/// A-679 (7.2's `serve` asked for `--hidden` by a user its `experimental.server.allow-hidden-access` does not name;
/// recorded from stock 7.2 on 2026-11-19). Do: start a session for a principal asking for hidden changesets, read what it
/// writes before any input, then run the probe's session through it. Expect: 7.2's warning naming the principal, alone on
/// standard error before anything else; then the session served as without it, byte for byte, exit 0.
#[test]
fn a_client_asking_for_hidden_changesets_is_warned_as_7_2_warns_and_served_the_view() {
  let lends = Lends::probed();
  let holder = HOLDER;
  let started = HgCommand::new(lends, (HG, holder.clone()), room(), true);
  let first = (started.output().0.to_vec(), started.output().1.to_vec());
  let input = [HELLO, NULL_PAIR, PROBE_CLIENT].concat();
  let warned = session(lends, Some(&holder), &input, input.len());
  assert_eq!(
    (first, warned.stderr, warned.stdout, warned.exited),
    (
      (hidden_warning(&holder), Vec::new()),
      hidden_warning(&holder),
      probe_answered(lends.probed),
      Some(EXIT_STATUS)
    )
  );
}

/// A-679 (the command's store). Do: open a session over the held store and send `hello`; then let the store go away and
/// send `heads`. Expect: `hello` answered; `heads` met with the store gone, which ends the session in the site's
/// not-served words with `abort`'s status, `heads` never consumed.
#[test]
fn a_store_that_goes_away_ends_the_session_as_not_served() {
  let gone: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
  let lends = Lends {
    gone: Some(gone),
    ..Lends::probed()
  };
  let mut command = HgCommand::new(lends, (HG, HOLDER), room(), false);
  let mut channel = Channel::default();
  command.input(HELLO).expect("within the window");
  channel.drive(&mut command);
  let hello = std::mem::take(&mut channel.stdout);
  gone.store(true, Ordering::Release);
  command.input(HEADS).expect("within the window");
  channel.drive(&mut command);
  let ended = Channel {
    stderr: b"abort: hg serve --stdio is not served yet\n".to_vec(),
    stdout: Vec::new(),
    consumed: HELLO.len(),
    exited: Some(HG_ABORT_STATUS),
  };
  assert_eq!((hello, channel), (hello_answer(lends.probed), ended));
}

/// A-679 (the command's bounds; A-650 part 2's window). Do: over a 24-byte room and its window, hand the command 25
/// bytes; then a request whose argument is longer than the room admits, and step; then, before that step's answer is
/// taken, 6 bytes more; then drive it as the edge does. Expect: the 25 refused in Mercurial's form naming both numbers,
/// nothing taken; the request answered out of band (an empty line on standard output, words ending `\n-\n` on standard
/// error) with all its bytes consumed, which ends the session; the 6 bytes dropped as an exited process drops them yet
/// given back to the window at the next step; and the command exited 0.
#[test]
fn input_the_session_drops_is_given_back_to_the_window() {
  let mut command = HgCommand::new(Lends::probed(), (HG, HOLDER), SMALL_ROOM, false);
  let past = command.input(&[b'x'; PAST_SMALL_ROOM]);
  command.input(TOO_LONG_REQUEST).expect("within the window");
  let refused_whole = ready(command.step(ClientGone::never()));
  command.input(HEADS).expect("dropped, not refused");
  let mut channel = Channel::default();
  channel.drive(&mut command);
  let past_window = Refused {
    stderr: b"abort: the session's input passed its window: 25 of 24 bytes\n".to_vec(),
    exit_status: HG_ABORT_STATUS,
  };
  let stepped = Stepped {
    consumed: TOO_LONG_REQUEST.len(),
    next: Next::More,
  };
  assert_eq!(
    (
      past,
      refused_whole,
      channel.stdout,
      channel.consumed,
      channel.exited
    ),
    (
      Err(past_window),
      Ok(stepped),
      b"\n".to_vec(),
      HEADS.len(),
      Some(EXIT_STATUS)
    )
  );
  assert!(channel.stderr.ends_with(b"\n-\n") && channel.stderr.len() > 3);
}

/// What `services` answers a start of `service` over `repository` for [`HOLDER`]: the command it started, by its kind
/// and whether it warns first, or not served.
fn started(
  services: &mut ToolServices<NoGit, Lends, ThisShard>,
  service: Service,
  repository: &RepositoryRecord,
) -> &'static str {
  let holder = HOLDER;
  let start = Start {
    service,
    repository: Some(repository),
    principal: &holder,
    git_protocol: None,
  };
  match services.start(&start) {
    Ok(ToolCommand::Mercurial(command)) if command.output().0.is_empty() => "session",
    Ok(ToolCommand::Mercurial(_)) => "warned session",
    Ok(ToolCommand::Upload(_)) => "upload-pack",
    Ok(ToolCommand::Receive(_)) => "receive-pack",
    Err(Unserved) => "not served",
  }
}

/// A-679 (the site's step 3). Do: start Mercurial's session over the repository whose store the lender holds, without and
/// with `--hidden`; over one whose store it does not hold; and git's upload-pack for the held one, no git store being
/// lent. Expect: both sessions over the held store started, the hidden one warning first; the others not served.
#[test]
fn mercurials_session_starts_only_over_a_store_the_lender_holds() {
  let mut services = ToolServices::new(NoGit, Lends::probed(), ThisShard, room());
  let (held, unheld) = (record(HG, HG_NAME), record(UNHELD, HG_NAME));
  let answers = [
    (Service::Mercurial { hidden: false }, &held),
    (Service::Mercurial { hidden: true }, &held),
    (Service::Mercurial { hidden: false }, &unheld),
    (Service::GitUploadPack, &held),
  ]
  .map(|(service, repository)| started(&mut services, service, repository));
  assert_eq!(
    answers,
    ["session", "warned session", "not served", "not served"]
  );
}

/// What `site` answers Mercurial's `serve --stdio` of `repository` for [`HOLDER`], with `--hidden` when `hidden`: the
/// refusal, or none when it runs.
fn refusal(site: &mut HgSite, repository: &[u8], hidden: bool) -> Option<Refused> {
  let asked = Asked::Mercurial {
    repository: repository.to_vec(),
    hidden,
  };
  match site.dispatch(&HOLDER, &asked, None) {
    Dispatched::Run(_) => None,
    Dispatched::Refuse(refused) => Some(refused),
  }
}

/// `stderr` refused with `abort`'s status.
fn aborted(stderr: String) -> Option<Refused> {
  Some(Refused {
    stderr: stderr.into_bytes(),
    exit_status: HG_ABORT_STATUS,
  })
}

/// A-679 (the site's step 1; 7.2's `dispatch.py` `_runcatchfunc`, recorded from stock 7.2 on 2026-11-19 with
/// `hg -R <path> serve --stdio [--hidden]` under `HGPLAIN=1 HGRCPATH=` in an empty directory). Do: ask for Mercurial's
/// session with repository arguments starting `--`: the bare `--`; `--x`, which names a held repository here, without and
/// with `--hidden`; and ones holding a quote, a tab, line ends, a backslash, DEL, a control byte, a double quote and
/// bytes past ASCII; then with `-x`, starting with one `-` alone, without and with `--hidden`; then the held repository
/// by its own name. Expect: each `--` argument refused before it is looked up, in 7.2's words byte for byte, its
/// arguments listed as 7.2 listed them, 255; `-x` refused as a missing repository, as 7.2 refused it; the held one run.
#[test]
fn a_repository_argument_starting_with_two_dashes_is_refused_as_7_2_refuses_it() {
  let mut site = site(Lends::probed());
  let answers = UNSAFE.map(|(repository, hidden, _)| refusal(&mut site, repository, hidden));
  let one_dash = [false, true].map(|hidden| refusal(&mut site, ONE_DASH, hidden));
  let served = refusal(&mut site, HG_NAME.as_bytes(), false);
  let not_found = || aborted("abort: repository -x not found\n".to_owned());
  assert_eq!(
    (answers, one_dash, served),
    (
      UNSAFE.map(|(_, _, listed)| aborted(format!("{UNSAFE_INVOCATION}{listed}\n"))),
      [not_found(), not_found()],
      None
    )
  );
}

/// Sends the end of the client's input on the session (`SSH_MSG_CHANNEL_EOF`, RFC 4254 §5.3).
fn send_eof(peer: &mut Peer) {
  let mut message = vec![96];
  message.extend_from_slice(&peer.channel.to_be_bytes());
  peer.send(&message);
}

/// A session with the held key: authenticate, run `command`, send `input` and, when `ends` says so, the end of the
/// input; then read to the end.
fn hg_session(
  address: SocketAddrV4,
  command: &[u8],
  input: &[u8],
  ends: bool,
) -> Option<Transcript> {
  let mut peer = Peer::connect(address)?;
  (query(&mut peer, &HELD_SEED)? == 60).then_some(())?;
  (sign(&mut peer, &HELD_SEED)? == 52).then_some(())?;
  exec(&mut peer, command).then_some(())?;
  if !input.is_empty() {
    send_data(&mut peer, input);
  }
  if ends {
    send_eof(&mut peer);
  }
  Some(transcript(&mut peer))
}

/// A-679 (§4.17.2 "Transports" item 2; §4.17.3 "The wire surface"; a scripted `hg` over SSH). Do: over the edge with the
/// production site, its services lending the probed repository's store and no git store, authenticate with a held key
/// and run `hg -R hg/r serve --stdio`, sending the handshake and the probe, then the end of the input; run it again with
/// `--hidden`, sending the handshake alone; then run it for a repository nobody has. Expect: the first session's standard
/// output the session's capabilities, 7.2's empty line and 7.2's eight answers byte for byte, then exit-status 0, EOF and
/// close in order; the second's 7.2's warning on standard error before its answers; the third refused in 7.2's words,
/// 255; each connection ended by the edge after the client's close; two commands run and one refused.
#[test]
fn an_hg_client_is_served_over_the_edge_as_7_2_serves_it() {
  let lends = Lends::probed();
  let ((plain, hidden, missing), seen) = served_by(
    ONE_SEAT,
    move || site(lends),
    |address| {
      let probe = [HELLO, NULL_PAIR, PROBE_CLIENT].concat();
      let handshake = [HELLO, NULL_PAIR].concat();
      (
        hg_session(address, b"hg -R hg/r serve --stdio", &probe, true),
        hg_session(
          address,
          b"hg -R hg/r serve --stdio --hidden",
          &handshake,
          true,
        ),
        hg_session(address, b"hg -R hg/none serve --stdio", &[], false),
      )
    },
  );
  let answered = [hello_answer(lends.probed).as_slice(), NULL_BETWEEN].concat();
  assert_eq!(
    (plain, hidden, missing),
    (
      Some(Transcript {
        out: probe_answered(lends.probed),
        err: Vec::new(),
        status: Some(EXIT_STATUS),
        order: vec![94, 98, 96, 97],
        ended: true,
      }),
      Some(Transcript {
        out: answered,
        err: hidden_warning(&HOLDER),
        status: Some(EXIT_STATUS),
        order: vec![95, 94, 98, 96, 97],
        ended: true,
      }),
      Some(Transcript {
        out: Vec::new(),
        err: b"abort: repository hg/none not found\n".to_vec(),
        status: Some(HG_ABORT_STATUS),
        order: vec![95, 98, 96, 97],
        ended: true,
      })
    )
  );
  assert_eq!(
    (seen.counts.commands_run, seen.counts.commands_refused),
    (2, 1)
  );
}

/// Format: a bundle as a push's chunks carry it: `<length>\n<data>` twice, then the empty chunk `0\n` that ends it.
const CHUNKED: &[u8] = b"5\nHG20\x003\n\0\0\x000\n";
/// Format: the bundle [`CHUNKED`] carries, its chunks joined.
const CHUNKED_BUNDLE: &[u8] = b"HG20\0\0\0\0";
/// Format: the request a bundle2 push opens with, as 7.2's client writes it: `unbundle`, its `heads` argument `force` in
/// hexadecimal (`wireprotov1peer.unbundle`: a bundle2 push sends `force`).
const UNBUNDLE_FORCE: &[u8] = b"unbundle\nheads 10\n666f726365";

/// A-696 (the command's pushes). Do: over the tests' lender, which answers a push with what reached it, send `unbundle`
/// with `force` and a bundle in two chunks, then end the input, a byte, seven bytes, a kilobyte and the whole at a time.
/// Expect at every cut: the empty answer that asks for the bundle, then the lender's reply as it is (a bundle2 holding
/// the principal the key proved, the `heads` argument read as `force` and the bundle's bytes joined); nothing on
/// standard error; every byte consumed; exit 0.
#[test]
fn a_push_reaches_the_lender_as_its_client_sent_it_and_its_reply_goes_back_as_it_is() {
  let input = [UNBUNDLE_FORCE, CHUNKED].concat();
  let echo = Replied::Output {
    in_reply_to: None,
    words: echoed((
      &HOLDER,
      &slates_hg::push::check::HeadsArgument::Force,
      CHUNKED_BUNDLE,
    )),
  };
  let mut reply = Vec::new();
  Reply::accepted(&[echo], true)
    .encode(&mut reply)
    .expect("an encodable reply");
  let expected = Channel {
    stderr: Vec::new(),
    stdout: [&b"0\n"[..], &reply].concat(),
    consumed: input.len(),
    exited: Some(EXIT_STATUS),
  };
  for cut in CUTS.into_iter().chain([input.len()]) {
    assert_eq!(
      session(Lends::probed(), None, &input, cut),
      expected,
      "cut {cut}"
    );
  }
}

/// Format: stock 7.2's push over SSH (`crates/hg/fixtures/wire`, `make-wire-fixtures.sh`'s `ssh-push`): what its client
/// wrote (the handshake, discovery, `unbundle` with `third` and its race checks, then `listkeys phases`), what its server
/// answered on standard output, and on standard error.
const SSH_PUSH_CLIENT: &[u8] = include_bytes!("../../../../hg/fixtures/wire/ssh-push.client");
/// Format: see [`SSH_PUSH_CLIENT`].
const SSH_PUSH_SERVER: &[u8] = include_bytes!("../../../../hg/fixtures/wire/ssh-push.server");
/// Format: see [`SSH_PUSH_CLIENT`].
const SSH_PUSH_STDERR: &[u8] = include_bytes!("../../../../hg/fixtures/wire/ssh-push.stderr");
/// Format: 7.2's progress words while it took that push, all its standard error held (`changegroup.apply`,
/// `scmutil.registersummarycallback`), which slates leaves out (`slates_hg::serve::stdio`'s module doc).
const PROGRESS_WORDS: &[u8] = b"adding changesets\nadding manifests\nadding file changes\n\
  added 1 changesets with 1 changes to 1 files\n";
/// Format: the history 7.2's server held before that push, as the script made it with stock 7.2 (user `test`, date `0
/// 0`): `base` and `second`, each adding one file, committed on the server, so both draft there.
const WIRE_HISTORY: [Commit; 2] = [
  ("base", "a", b"a\n", None, "default"),
  ("second", "b", b"b\n", Some(0), "default"),
];
/// Format: `third`, the changeset the push carried.
const THIRD: &str = "d62662368ebecf4e7b3e39404f37e3b36d25a79e";
/// Format: what 7.2's client reads in a server's capabilities to build a push's bundle (`exchange._pushbundle2` and the
/// part generators it runs: `changegroup`'s versions, `checkheads`, `phases`, `bookmarks`, `pushkey`, `error`'s
/// `pushkey`).
const PUSH_CAPABILITIES: [&[u8]; 6] = [
  b"bookmarks",
  b"changegroup",
  b"checkheads",
  b"error",
  b"phases",
  b"pushkey",
];
/// Shape: the room a daemon test's push is prepared within, far past these trees.
const PUSH_ROOM: u64 = 1 << 20;
/// Shape: the name the repository's history was pushed under.
const HISTORY_PUSH: [u8; 32] = [7; 32];
/// Shape: how long a daemon test waits for its session: four liveness budgets.
const PATIENCE: Duration = Duration::from_nanos(4 * crate::daemon::LIVENESS_BUDGET_NS);

/// The owner the daemon tests make their repository as, and push as.
fn owner() -> Principal {
  Principal::Uid {
    host: slates_db::HostId(0),
    uid: 1234,
  }
}

/// Makes Mercurial repository `name` on `state`'s shard holding what 7.2's server held before `ssh-push`: `base` and
/// `second` landed draft, as a non-publishing push lands them, with bookmark `main` at `base`; its record.
fn drafted_history(state: &mut ShardState, name: &str) -> RepositoryRecord {
  let reply = crate::verbs::dispatch(
    state,
    1,
    &owner(),
    RequestBody::CreateRepository {
      name: name.to_owned(),
      tool: IpcTool::Mercurial,
      format: IpcFormat::Sha1,
      default_branch: line_name(b"default", 0),
      green: None,
    },
  );
  assert!(matches!(reply, ReplyBody::GreenCreated { .. }), "{reply:?}");
  let id = state
    .db
    .partition()
    .repositories()
    .repository_named(name)
    .expect("the repository made")
    .id;
  let made = history_of(&WIRE_HISTORY);
  let mark = BookmarkAt {
    name: b"main".to_vec(),
    node: Some(made[0].node),
  };
  let unbundle = Unbundle {
    parts: vec![
      Taken {
        id: 0,
        mandatory: true,
        asks: Asks::Changegroup(changegroup(&made, &[0, 1])),
      },
      Taken {
        id: 1,
        mandatory: true,
        asks: Asks::Bookmarks(vec![mark]),
      },
    ],
  };
  let mut room = slates_hg::Admission::new(u64::MAX);
  let terms = (false, HISTORY_PUSH, PUSH_ROOM);
  let prepared =
    hg_push::prepare_served(state, id, &unbundle, terms, &mut room).expect("the history prepared");
  let landed = hg_push::land(state, (&owner(), id), prepared);
  assert!(
    landed.answer.is_ok() && landed.released.is_ok(),
    "{landed:?}"
  );
  state
    .db
    .partition()
    .repositories()
    .repository(id)
    .cloned()
    .expect("the record")
}

/// Stock 7.2's client bytes of `ssh-push` sent to `record`'s repository as the SSH site starts `serve --stdio` over the
/// daemon's lenders for its owner, a kilobyte at a time, then the end of its input: what its channel took.
async fn stock_push_session(record: &RepositoryRecord) -> Result<Channel, String> {
  let mut services = ToolServices::new(ShardStores, ServedViews, ThisShard, room());
  let principal = owner();
  let start = Start {
    service: Service::Mercurial { hidden: false },
    repository: Some(record),
    principal: &principal,
    git_protocol: None,
  };
  let mut command = services
    .start(&start)
    .map_err(|unserved| format!("{unserved:?}"))?;
  let mut channel = Channel::default();
  for piece in SSH_PUSH_CLIENT.chunks(CUTS[2]) {
    command
      .input(piece)
      .map_err(|refused| format!("{refused:?}"))?;
    channel.drive_awaiting(&mut command).await;
  }
  command.input_end();
  channel.drive_awaiting(&mut command).await;
  Ok(channel)
}

/// The first string answer of `stdout` and what follows it.
fn after_first_answer(stdout: &[u8]) -> (&[u8], &[u8]) {
  let line = stdout
    .iter()
    .position(|byte| *byte == b'\n')
    .expect("a length line");
  let length: usize = std::str::from_utf8(&stdout[..line])
    .expect("digits")
    .parse()
    .expect("a length");
  stdout[line + 1..].split_at(length)
}

/// Capabilities with their values: each capability's name and its values.
type CapabilityValues = Vec<(Vec<u8>, Vec<Vec<u8>>)>;

/// Each capability of `PUSH_CAPABILITIES` in the `bundle2` value of a `hello` answer's capabilities line, with its
/// values; and `unbundle`'s value.
fn push_capabilities(hello: &[u8]) -> (CapabilityValues, Option<Vec<u8>>) {
  let listed = capabilities::decode_hello(hello).expect("a capabilities line");
  let mut room = slates_hg::Admission::new(u64::MAX);
  let (mut quoted, mut unquoted) = (Vec::new(), Vec::new());
  let blob = listed
    .get(capabilities::BUNDLE2)
    .flatten()
    .expect("bundle2 listed");
  let blob = capabilities::decode_bundle2_value(blob, &mut room, &mut quoted).expect("a blob");
  let bundle2 = capabilities::decode_bundle2(blob, &mut room, &mut unquoted).expect("its list");
  let pushed = PUSH_CAPABILITIES
    .iter()
    .map(|name| {
      let values = bundle2
        .get(name)
        .map(|values| values.iter().map(|value| value.to_vec()).collect());
      (name.to_vec(), values.unwrap_or_default())
    })
    .collect();
  let unbundle = listed.get(b"unbundle").flatten().map(<[u8]>::to_vec);
  (pushed, unbundle)
}

/// A-696 (golden: stock 7.2's push over SSH, `crates/hg/fixtures/wire`'s `ssh-push`; §4.17.3 pieces 4 and 5). Do: on a
/// daemon's shard, make a Mercurial repository holding what 7.2's server held (`base` and `second` draft, `main` at
/// `base`); then, from a task there, start `hg serve --stdio` through the SSH site's services over the daemon's lenders
/// for its owner and send it what stock 7.2's client wrote: the handshake, discovery (`protocaps`, a `batch` of `heads`
/// and `known`, `listkeys` of phases and bookmarks, `branchmap`), `unbundle` with `third` and its race checks, and
/// `listkeys phases` after. Expect: the push's capabilities a 7.2 client builds its bundle from listed as 7.2 lists
/// them, and `unbundle` with 7.2's value, so a stock client sends this server the bytes it sent; every answer after
/// `hello` 7.2's byte for byte (discovery over the draft history; the empty answer asking for the bundle; the reply,
/// `reply:changegroup` naming the changegroup part with one head more; the draft root gone after it, `publishing`
/// alone); 7.2's standard error its progress words alone, which slates leaves out, so nothing there; every byte
/// consumed and exit 0; and `default`'s line at `third` afterwards.
#[test]
fn stock_7_2s_push_over_ssh_lands_through_the_daemons_lenders_as_7_2_answered_it() {
  let daemon = crate::daemon::audit_daemon();
  let record =
    crate::daemon::observe_first(&daemon, |state| drafted_history(state, "pushed-over-ssh"));
  let id = record.id;
  let (sent, outcome) = mpsc::sync_channel(1);
  crate::daemon::observe_first(&daemon, move |_state| {
    let task = slates_rt::futures::spawn(async move {
      let _ = sent.send(stock_push_session(&record).await);
    })
    .expect("the session's task");
    slates_rt::futures::detach(task).expect("detached");
  });
  let channel = outcome.recv_timeout(PATIENCE).expect("the session ended");
  let line = crate::daemon::observe_first(&daemon, move |state| {
    let green = state
      .db
      .partition()
      .repositories()
      .branch(id, &line_name(b"default", 0))?;
    let engine = state.greens.get(&green)?;
    let head = engine.xattr(slates_merge::push::ROOT, slates_merge::push::HEAD_ATTRIBUTE)?;
    Some(crate::verbs::hex_of(&Node::try_from(head).ok()?))
  });
  daemon.stop();
  let channel = channel.expect("the session served");
  let (ours_hello, ours) = after_first_answer(&channel.stdout);
  let (theirs_hello, theirs) = after_first_answer(SSH_PUSH_SERVER);
  assert_eq!(
    push_capabilities(ours_hello),
    push_capabilities(theirs_hello)
  );
  assert_eq!(
    String::from_utf8_lossy(ours),
    String::from_utf8_lossy(theirs)
  );
  assert_eq!(SSH_PUSH_STDERR, PROGRESS_WORDS);
  assert_eq!(
    (channel.stderr, channel.consumed, channel.exited, line),
    (
      Vec::new(),
      SSH_PUSH_CLIENT.len(),
      Some(EXIT_STATUS),
      Some(THIRD.to_owned())
    )
  );
}

/// Makes Mercurial repository `name` on `state`'s shard holding one changeset whose one file is `bytes`: its record and
/// the changeset's node.
fn one_file_history(
  state: &mut ShardState,
  name: &str,
  bytes: &'static [u8],
) -> (RepositoryRecord, Node) {
  let reply = crate::verbs::dispatch(
    state,
    1,
    &owner(),
    RequestBody::CreateRepository {
      name: name.to_owned(),
      tool: IpcTool::Mercurial,
      format: IpcFormat::Sha1,
      default_branch: line_name(b"default", 0),
      green: None,
    },
  );
  assert!(matches!(reply, ReplyBody::GreenCreated { .. }), "{reply:?}");
  let id = state
    .db
    .partition()
    .repositories()
    .repository_named(name)
    .expect("the repository made")
    .id;
  let made = history_of(&[("large", "large", bytes, None, "default")]);
  let unbundle = Unbundle {
    parts: vec![Taken {
      id: 0,
      mandatory: true,
      asks: Asks::Changegroup(changegroup(&made, &[0])),
    }],
  };
  let mut room = slates_hg::Admission::new(u64::MAX);
  // The push is past one request's room by design: it is admitted what the shard can hold.
  let terms = (false, HISTORY_PUSH, u64::MAX);
  let prepared =
    hg_push::prepare_served(state, id, &unbundle, terms, &mut room).expect("the history prepared");
  let landed = hg_push::land(state, (&owner(), id), prepared);
  assert!(
    landed.answer.is_ok() && landed.released.is_ok(),
    "{landed:?}"
  );
  let record = state
    .db
    .partition()
    .repositories()
    .repository(id)
    .cloned()
    .expect("the record");
  (record, made[0].node)
}

/// The SSH request of a clone of `head` as stock 7.2 asks it: `getbundle` with bundle2's changegroup 02 and 03 and
/// nothing common, then the empty line that ends the session.
fn clone_request(head: &Node) -> Vec<u8> {
  let mut request = b"getbundle\n* 3\n".to_vec();
  let bundlecaps = b"HG20,bundle2=HG20%0Achangegroup%3D02%2C03";
  let common = "0".repeat(40);
  let heads = crate::verbs::hex_of(head);
  for (key, value) in [
    (&b"bundlecaps"[..], &bundlecaps[..]),
    (b"common", common.as_bytes()),
    (b"heads", heads.as_bytes()),
  ] {
    request.extend_from_slice(key);
    request.extend_from_slice(format!(" {}\n", value.len()).as_bytes());
    request.extend_from_slice(value);
  }
  request.push(b'\n');
  request
}

/// A-745 (§4.17.1 "A fetch's answer"; found by use, w-interop: `ssh-clone` of Mercurial's own history refused
/// `PastAdmission`). Do: on a daemon's shard make a Mercurial repository of one changeset whose one file is twice a
/// request's room, then clone it over SSH as stock 7.2 asks, through the daemon's lenders for its owner. Expect the
/// session to exit cleanly, nothing on standard error, and its standard output a bundle2 whose changegroup carries the
/// file's bytes whole: the answer made a turn at a time, each slice within the shard's room, never one request's.
#[test]
fn a_mercurial_clone_larger_than_a_requests_room_is_answered_over_ssh() {
  let mut state = 0x9e37_79b9_7f4a_7c15u64;
  let large: Vec<u8> = (0..usize::try_from(room() * 2).expect("a size"))
    .map(|_| {
      state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
      state.to_be_bytes()[0]
    })
    .collect();
  let large: &'static [u8] = Box::leak(large.into_boxed_slice());
  let daemon = crate::daemon::audit_daemon();
  let (record, head) = crate::daemon::observe_first(&daemon, move |state| {
    one_file_history(state, "cloned-over-ssh", large)
  });
  let (sent, outcome) = mpsc::sync_channel(1);
  crate::daemon::observe_first(&daemon, move |_state| {
    let task = slates_rt::futures::spawn(async move {
      let session = async {
        let mut services = ToolServices::new(ShardStores, ServedViews, ThisShard, room());
        let principal = owner();
        let start = Start {
          service: Service::Mercurial { hidden: false },
          repository: Some(&record),
          principal: &principal,
          git_protocol: None,
        };
        let mut command = services
          .start(&start)
          .map_err(|unserved| format!("{unserved:?}"))?;
        let mut channel = Channel::default();
        command
          .input(&clone_request(&head))
          .map_err(|refused| format!("{refused:?}"))?;
        command.input_end();
        channel.drive_awaiting(&mut command).await;
        Ok::<Channel, String>(channel)
      };
      let _ = sent.send(session.await);
    })
    .expect("the session's task");
    slates_rt::futures::detach(task).expect("detached");
  });
  let channel = outcome.recv_timeout(PATIENCE).expect("the session ended");
  daemon.stop();
  let channel = channel.expect("the session served");
  assert_eq!(
    (String::from_utf8_lossy(&channel.stderr), channel.exited),
    (String::new().into(), Some(EXIT_STATUS))
  );
  let bundle =
    slates_hg::bundle2::Bundle2::decode(&channel.stdout, &mut slates_hg::Admission::new(u64::MAX))
      .expect("a bundle2");
  let (index, _) = bundle
    .parts
    .iter()
    .enumerate()
    .find(|(_, part)| part.kind.eq_ignore_ascii_case(b"changegroup"))
    .expect("a changegroup part");
  let payload = bundle
    .payload_bytes(index, &mut slates_hg::Admission::new(u64::MAX))
    .expect("its payload");
  let found = payload
    .windows(64)
    .position(|window| window == &large[..64])
    .expect("the file's bytes");
  assert_eq!(payload.get(found..found + large.len()), Some(large));
}
