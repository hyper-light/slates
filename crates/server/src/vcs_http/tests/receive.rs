//! The HTTP site's receive-pack exchange driven on a stored daemon's shard (the receive module's doc, "Refusals"): a
//! push refused once its body was read whole is answered in git's words and its reply ended cleanly, never cut short.

use std::cell::Cell;

use slates_git::http::{Service, Version};
use slates_git::pktline::{self, Packet};

use super::{
  PUSH_ENDED_EMPTY, PUSH_REFUSED_AFTER_BODY, Receive, ReceiveServices, ShardLender, ThisShard,
};
use crate::daemon::observe_first;
use crate::git_push::tests::{git_repository, owner};
use crate::state::ShardState;
use crate::vcs_http::daemon_edge::tests::long_history;
use crate::vcs_http::git::{GitServices, Start};
use crate::vcs_http::{ClientGone, Exchange};

/// Shape: the commits of the pushed history: a few turns of the answer.
const HISTORY: usize = 30;

/// Format: the words a refusal of a shard not lent is answered in (`crate::git_push::words::NOT_SERVED`).
const NOT_SERVED: &[u8] = b"git-receive-pack is not served yet";

thread_local! {
  /// How many of [`FencedOnce`]'s next lends are refused, on the shard's thread the exchange runs on.
  static REFUSALS_DUE: Cell<u32> = const { Cell::new(0) };
  /// How many of [`FencedOnce`]'s next lends are made before [`REFUSALS_DUE`] refuses any.
  static LENDS_BEFORE_FENCE: Cell<u32> = const { Cell::new(0) };
}

/// The daemon's lender ([`ThisShard`]), but for the lends [`REFUSALS_DUE`] names, refused as a fenced shard's are.
#[derive(Clone, Copy, Debug)]
struct FencedOnce;

impl ShardLender for FencedOnce {
  fn lend<R>(&mut self, step: impl FnOnce(&mut ShardState) -> R) -> Option<R> {
    let lent_first = LENDS_BEFORE_FENCE.with(|before| {
      let left = before.get();
      before.set(left.saturating_sub(1));
      left > 0
    });
    if lent_first {
      return ThisShard.lend(step);
    }
    let refused = REFUSALS_DUE.with(|due| {
      let left = due.get();
      due.set(left.saturating_sub(1));
      left > 0
    });
    if refused {
      return None;
    }
    ThisShard.lend(step)
  }
}

/// `request` with its first command's capabilities `asked` in place of stock git's.
fn asking(request: &[u8], asked: &str) -> Vec<u8> {
  let length = request
    .get(..4)
    .and_then(|hex| core::str::from_utf8(hex).ok())
    .and_then(|hex| usize::from_str_radix(hex, 16).ok())
    .expect("a first packet");
  let line = request.get(4..length).expect("its line");
  let at = line
    .iter()
    .position(|byte| *byte == 0)
    .expect("its capabilities");
  let mut first = line.get(..=at).expect("its command").to_vec();
  first.extend_from_slice(asked.as_bytes());
  let mut asked_request = Vec::new();
  pktline::write(Packet::Data(&first), &mut asked_request).expect("the command");
  asked_request.extend_from_slice(request.get(length..).expect("the rest"));
  asked_request
}

/// Pushes `request` to a new repository named `name` through the site's receive exchange on a stored daemon's shard, its
/// shard fenced for the one lend that ends the body: whether the body's end was answered (not failed), the whole reply,
/// and how many pushes were counted refused after their body.
fn pushed_fenced_after_body(name: &'static str, request: Vec<u8>) -> (bool, Vec<u8>, u64) {
  let pushed = pushed_through(name, request, 1);
  (pushed.ended, pushed.answer, pushed.refused_after_body)
}

/// What [`pushed_through`] saw of a push.
struct Pushed {
  /// Whether the body's end was answered, not failed.
  ended: bool,
  /// The whole reply given.
  answer: Vec<u8>,
  /// Whether the reply ended cleanly (its last turn answered no more), not failed.
  completed: bool,
  /// The pushes counted refused after their body.
  refused_after_body: u64,
  /// The pushes whose reply would have ended with nothing given, counted.
  ended_empty: u64,
}

/// Pushes `request` (none when empty: the edge hands the exchange no run then) to a new repository named `name` through
/// the site's receive exchange on a stored daemon's shard, its shard fenced for the next `fenced` lends once the body
/// is taken: what the push saw.
fn pushed_through(name: &'static str, request: Vec<u8>, fenced: u32) -> Pushed {
  pushed_fenced_after(name, request, (0, fenced))
}

/// [`pushed_through`], the shard fenced for `fenced` lends once `lent` more lends after the body was taken were made.
fn pushed_fenced_after(name: &'static str, request: Vec<u8>, (lent, fenced): (u32, u32)) -> Pushed {
  let (_store, daemon) = crate::test_store::audit_daemon_stored();
  let (sent, answered) = std::sync::mpsc::channel();
  observe_first(&daemon, move |_| {
    let task = slates_rt::futures::spawn(async move {
      let record = crate::state::with_state(|state| {
        let repository = git_repository(state, name);
        state
          .db
          .partition()
          .repositories()
          .repository(repository)
          .cloned()
      })
      .flatten()
      .expect("the repository");
      let principal = owner();
      let start = Start {
        repository: &record,
        principal: &principal,
        service: Service::ReceivePack,
        version: Version::V0,
        advertise: false,
      };
      let mut exchange: Receive<FencedOnce> = ReceiveServices::new(FencedOnce, u64::MAX)
        .start(&start)
        .expect("the push starts");
      if !request.is_empty() {
        exchange.body(&request).await.expect("the body taken");
      }
      LENDS_BEFORE_FENCE.with(|before| before.set(lent));
      REFUSALS_DUE.with(|due| due.set(fenced));
      let ended = exchange.body_end(ClientGone::never()).await.is_ok();
      let mut answer = Vec::new();
      let completed = loop {
        match exchange.reply(&mut answer, ClientGone::never()).await {
          Ok(true) => {}
          Ok(false) => break true,
          Err(_) => break false,
        }
      };
      let counted = |name: &str| {
        crate::state::with_state(|state| state.refusals.get(name).copied().unwrap_or(0))
          .unwrap_or(0)
      };
      let _ = sent.send(Pushed {
        ended,
        answer,
        completed,
        refused_after_body: counted(PUSH_REFUSED_AFTER_BODY),
        ended_empty: counted(PUSH_ENDED_EMPTY),
      });
    })
    .expect("the task");
    slates_rt::futures::detach(task).expect("detached");
  });
  let outcome = answered.recv().expect("the outcome");
  daemon.stop();
  outcome
}

/// The bytes `text` takes as one pkt-line.
fn line(text: &[u8]) -> Vec<u8> {
  let mut written = Vec::new();
  pktline::write(Packet::Data(text), &mut written).expect("a line");
  written
}

/// w-interop, 2026-11-21 (main's priority, 01:31 PST): a whole-history push of git/git over the HTTP edge was refused
/// past its room in its checks, after the reply's `200` head was out, and the edge cut the connection: stock git said
/// `curl 18 transfer closed with outstanding read data remaining` and no word of why, 6 of 6. Do: push a history of
/// [`HISTORY`] commits as stock git posts it, side-band-64k asked, through the site's receive exchange on a stored
/// daemon's shard, the shard fenced for the one lend that ends the body. Expect: the body's end answered, not failed;
/// the reply git's band 3 with the refusal's words, `git-receive-pack is not served yet`, ended there; the push counted
/// refused after its body.
#[test]
fn a_push_refused_after_its_body_is_answered_on_gits_band_three_not_cut_short() {
  let (request, _) = long_history(HISTORY);
  let (ended, answer, counted) = pushed_fenced_after_body("fenced", request);
  let mut words = vec![3];
  words.extend_from_slice(NOT_SERVED);
  assert_eq!(
    (
      ended,
      String::from_utf8_lossy(&answer).into_owned(),
      counted
    ),
    (true, String::from_utf8_lossy(&line(&words)).into_owned(), 1)
  );
}

/// The receive module's doc, "Refusals" (main's ruling, 01:56 PST: `unpack <reason>` and `ng` per ref where side-band
/// cannot carry band 3). Do: push the same history asking `report-status` without side-band-64k, the shard fenced for
/// the one lend that ends the body. Expect: the body's end answered; the reply git's report of a refused pack,
/// `unpack <words>`, `ng refs/heads/main <words>` and a flush; the push counted refused after its body.
#[test]
fn a_push_refused_after_its_body_without_side_band_is_reported_unpack_and_ng() {
  let (request, _) = long_history(HISTORY);
  let request = asking(&request, " report-status object-format=sha1");
  let (ended, answer, counted) = pushed_fenced_after_body("fenced-report", request);
  let mut report = line(&[b"unpack ".as_slice(), NOT_SERVED, b"\n"].concat());
  report.extend(line(
    &[b"ng refs/heads/main ".as_slice(), NOT_SERVED, b"\n"].concat(),
  ));
  pktline::write(Packet::Flush, &mut report).expect("the flush");
  assert_eq!(
    (
      ended,
      String::from_utf8_lossy(&answer).into_owned(),
      counted
    ),
    (true, String::from_utf8_lossy(&report).into_owned(), 1)
  );
}

/// Main 01a20182's gate 4 (run 45363070862, `v3_takeover`'s first debug run, 2026-11-24 02:24): stock git's push to a
/// repository created a moment before was answered `200` and a chunked body that ended cleanly with nothing in it, so
/// git said "unexpected disconnect while reading sideband packet", and the edge counted the connection completed with no
/// line logged. A request whose bytes never reach the exchange is read as one cut short before its first command
/// (`Truncated` at 0), and its refusal had no words to give: no side-band asked and no command named, as far as the
/// push knew. Do: end a push's body through the site's receive exchange with no bytes of it taken. Expect: the reply
/// fails (the edge cuts the connection, logged), never ends cleanly with nothing given; counted refused after its body
/// and ended empty.
#[test]
fn a_push_whose_request_never_arrived_is_failed_never_answered_with_an_empty_body() {
  let pushed = pushed_through("never-arrived", Vec::new(), 0);
  assert_eq!(
    (
      pushed.ended,
      pushed.answer,
      pushed.completed,
      pushed.refused_after_body,
      pushed.ended_empty
    ),
    (true, Vec::new(), false, 1, 1)
  );
}

/// The receive module's doc ("A request that asks nothing"): stock git's client posts a flush alone before a large push
/// to learn whether it may (`remote-curl.c`'s `probe_rpc`), and git's receive-pack writes nothing for it. Do: push a
/// flush alone through the site's receive exchange. Expect: the reply ends cleanly with nothing given, nothing counted
/// refused or ended empty: the guard against an empty answer leaves the probe's alone.
#[test]
fn a_probe_of_a_flush_alone_is_still_answered_with_nothing_and_ends_cleanly() {
  let pushed = pushed_through("probed", b"0000".to_vec(), 0);
  assert_eq!(
    (
      pushed.ended,
      pushed.answer,
      pushed.completed,
      pushed.refused_after_body,
      pushed.ended_empty
    ),
    (true, Vec::new(), true, 0, 0)
  );
}

/// Main 01a20182's gate 5 (run 45386968890, `v3_takeover`'s first release run, 2026-11-24 04:00): a push asking
/// side-band-64k was refused `Unlanded` while it was answered, and the log said "side-band false": the session lets its
/// request go once the push is checked and taken, and the side-band asked was read from it, so the refusal had no band
/// to go on and the reply was failed empty. Do: push a history asking side-band-64k through the site's receive exchange,
/// the shard fenced for the one lend after the push was checked and taken (the third after its body). Expect: the reply git's band 3 with the
/// refusal's words, ended cleanly; counted refused after its body, never ended empty.
#[test]
fn a_push_refused_while_answered_is_still_answered_on_band_three() {
  let (request, _) = long_history(HISTORY);
  let pushed = pushed_fenced_after("answered-fenced", request, (3, 1));
  let mut words = vec![3];
  words.extend_from_slice(NOT_SERVED);
  assert_eq!(
    (
      String::from_utf8_lossy(&pushed.answer).into_owned(),
      pushed.completed,
      pushed.refused_after_body,
      pushed.ended_empty
    ),
    (
      String::from_utf8_lossy(&line(&words)).into_owned(),
      true,
      1,
      0
    )
  );
}
