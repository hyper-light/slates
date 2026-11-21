//! The HTTP site's receive-pack exchange driven on a stored daemon's shard (the receive module's doc, "Refusals"): a
//! push refused once its body was read whole is answered in git's words and its reply ended cleanly, never cut short.

use std::cell::Cell;

use slates_git::http::{Service, Version};
use slates_git::pktline::{self, Packet};

use super::{PUSH_REFUSED_AFTER_BODY, Receive, ReceiveServices, ShardLender, ThisShard};
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
}

/// The daemon's lender ([`ThisShard`]), but for the lends [`REFUSALS_DUE`] names, refused as a fenced shard's are.
#[derive(Clone, Copy, Debug)]
struct FencedOnce;

impl ShardLender for FencedOnce {
  fn lend<R>(&mut self, step: impl FnOnce(&mut ShardState) -> R) -> Option<R> {
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
        .ok()
        .expect("the push starts");
      exchange.body(&request).await.ok().expect("the body taken");
      REFUSALS_DUE.with(|due| due.set(1));
      let ended = exchange.body_end(ClientGone::never()).await.is_ok();
      let mut answer = Vec::new();
      while exchange
        .reply(&mut answer, ClientGone::never())
        .await
        .unwrap_or(false)
      {}
      let counted = crate::state::with_state(|state| {
        state
          .refusals
          .get(PUSH_REFUSED_AFTER_BODY)
          .copied()
          .unwrap_or(0)
      })
      .unwrap_or(0);
      let _ = sent.send((ended, answer, counted));
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
