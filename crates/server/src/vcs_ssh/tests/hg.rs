//! Tests of Mercurial's `hg -R <path> serve --stdio` behind the SSH edge (A-679), a child of the edge's tests so that a
//! session runs over the real edge with their client. The golden is the probe stock `hg serve --stdio` 7.2 answered
//! (`crates/hg/fixtures/serve`, made by `make-serve-fixtures.sh`), sent after the handshake 7.2's client opens every
//! session with, over the repository the probe asked rebuilt from the fixture's listing: the command answers it byte for
//! byte as 7.2 did at every cut, and so does the edge. Then what the command adds to the session (7.2's warning to a
//! client asking for hidden changesets, the store going away, the window's bookkeeping), the services starting it, and
//! the site refusing a repository argument as 7.2's dispatch refuses it before anything is opened.

use std::borrow::Cow;
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Waker};

use slates_db::catalog::{
  ObjectFormat, Principal, RepositoryId, RepositoryPolicy, RepositoryRecord, VcsTool,
};
use slates_git::Admission;
use slates_git::upload::{PackPlan, PackRefused, PackSource, Served};
use slates_hg::obsmarkers::Marker;
use slates_hg::phases::Phase;
use slates_hg::push::Outcome;
use slates_hg::serve::getbundle::{Logged, Store};
use slates_hg::serve::stdio::{EXIT_STATUS, Progress, Stdio};
use slates_hg::serve::{Repository, Rev};
use slates_hg::{NULL_NODE, Node, pushkey};
use slates_rt::tcp::SocketAddrV4;
use slates_ssh::command::Command as Asked;

use super::super::hg::HgCommand;
use super::super::site::{
  Command, Dispatched, HG_ABORT_STATUS, Next, Refused, Service, Services, SshSite, Start, Stepped,
  ToolCommand, ToolServices, ToolSite, Unserved,
};
use super::{
  HELD_SEED, HOLDER, Keys, ONE_SEAT, Peer, ROOM_BYTES, TICK_NS, Transcript, exec, query, send_data,
  served_by, sign, transcript,
};
use crate::repository::hg_push::Pushing;
use crate::vcs_http::Bounds;
use crate::vcs_http::git::{Located, Repositories, StoreLender, ThisShard};
use crate::vcs_http::hg::HgStores;

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

  /// No push lands: the SSH session's `unbundle` is not served (its pushes are A-680 part 2b's driver's, over HTTP).
  fn push(
    &mut self,
    _id: RepositoryId,
    _pushing: Pushing<'_>,
    _room: &mut slates_hg::Admission,
  ) -> Option<Outcome> {
    None
  }
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
    while self.exited.is_none() {
      self.take(command);
      match ready(command.step()) {
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
  let mut command = HgCommand::new(lends, HG, room(), hidden);
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
  let started = HgCommand::new(lends, HG, room(), Some(&holder));
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
  let mut command = HgCommand::new(lends, HG, room(), None);
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
  let mut command = HgCommand::new(Lends::probed(), HG, SMALL_ROOM, None);
  let past = command.input(&[b'x'; PAST_SMALL_ROOM]);
  command.input(TOO_LONG_REQUEST).expect("within the window");
  let refused_whole = ready(command.step());
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
