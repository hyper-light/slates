//! Edit and submit in one request (§4.16 "Edit and submit in one request", A-225): the combined verb's semantics are
//! `edit` then `submit` exactly. Two in-process daemons serve the same generated histories, one through
//! `Client::edit_and_submit` and one through `Client::edit` then `Client::submit`, and every verdict, every conflict
//! report, every refusal and every file at every head must agree. Exactly-once across a restart is held by a retry of
//! an unanswered combined request after its daemon was replaced over the same anchor-held segment: it answers the
//! recorded verdict, and the green is one version ahead, not two.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::panic_in_result_fn,
  clippy::indexing_slicing
)]

use std::cell::RefCell;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use proptest::strategy::Strategy;
use proptest::test_runner::{Config, TestRunner};
use slates_anchor::AnchorSegment;
use slates_client::{Client, ClientError, Deadlines, ReadAt, Rebased, Submitted, VolumeId};
use slates_ipc::HeldRendezvous;
use slates_ipc::protocol::{EditBytes, ReplyBody, RequestBody};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::daemon::LIVENESS_BUDGET_NS;
use slates_server::{Daemon, DaemonConfig, SegmentSource};

/// Shape: each probe's wall budget for the test's machine profile (milliseconds).
const PROBE_MS: u64 = 5;
/// Shape: shards for each daemon: two, so a client's shard and the control shard differ.
const TEST_SHARDS: u16 = 2;
/// Shape: how long a client retries the rendezvous while its daemon starts.
const START_WAIT: Duration = Duration::from_secs(10);
/// Shape: the files of a generated green, each seeded with [`SEED`].
const FILES: usize = 3;
/// Shape: each file's seeded bytes, short so generated edits overlap and conflict often.
const SEED: &[u8] = b"abcdefgh";
/// Shape: the works a generated history keeps at once, so works race each other over one green.
const WORKS: usize = 3;

fn profile() -> MachineProfile {
  static PROFILE: OnceLock<MachineProfile> = OnceLock::new();
  PROFILE
    .get_or_init(|| {
      MachineProfile::measure_patiently(&ProfileOptions {
        budget_per_probe: Duration::from_millis(PROBE_MS),
        codecs: false,
        core_matrix: false,
      })
      .expect("the machine profile measures")
    })
    .clone()
}

fn deadlines() -> Deadlines {
  Deadlines::derive(LIVENESS_BUDGET_NS, slates_db::replay::RECOVERY_BUDGET_NS).get()
}

fn connect(instance: &str) -> Client {
  let started = Instant::now();
  loop {
    match Client::connect(instance, deadlines()) {
      Ok(client) => return client,
      Err(ClientError::Ipc(slates_ipc::IpcError::DaemonUnavailable { .. }))
        if started.elapsed() < START_WAIT =>
      {
        std::thread::yield_now();
      }
      Err(e) => panic!("{e}"),
    }
  }
}

fn start(tag: &str) -> (Daemon, Client) {
  let profile = profile();
  let instance = format!("es-{tag}-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-es-{tag}-{}", std::process::id()),
    },
  )
  .unwrap();
  daemon.bootstrap(true).unwrap();
  let client = connect(&instance);
  (daemon, client)
}

fn path_of(file: usize) -> String {
  format!("f{file}")
}

/// One step of a generated history.
#[derive(Clone, Debug)]
enum Step {
  /// Replace work slot `slot` with a new work based on the green's head.
  NewWork { slot: usize },
  /// An edit alone, through both clients alike: a work may carry several edits into its submit.
  Edit { slot: usize, splice: Splice },
  /// The step under test: one request on one daemon, `edit` then `submit` on the other.
  EditAndSubmit { slot: usize, splice: Splice },
  /// A rebase onto the head, through both clients alike.
  Rebase { slot: usize },
}

/// A splice of one file: some offsets and lengths fall past a file's end, so refusals are compared too.
#[derive(Clone, Debug)]
struct Splice {
  file: usize,
  at: u64,
  delete_len: u64,
  bytes: Vec<u8>,
}

fn splice() -> impl Strategy<Value = Splice> {
  (
    0..FILES,
    0u64..10,
    0u64..4,
    proptest::collection::vec(b'A'..=b'Z', 0..4),
  )
    .prop_map(|(file, at, delete_len, bytes)| Splice {
      file,
      at,
      delete_len,
      bytes,
    })
}

fn step() -> impl Strategy<Value = Step> {
  // A slot of `WORKS` names the green itself, which every edit, submit and rebase refuses: the edit's refusal path.
  (0u8..8, 0..=WORKS, splice()).prop_map(|(draw, slot, splice)| match draw {
    0 => Step::NewWork { slot: slot % WORKS },
    1 => Step::Edit { slot, splice },
    2 => Step::Rebase { slot },
    _ => Step::EditAndSubmit { slot, splice },
  })
}

/// One daemon's side of the oracle: its client, the green of the current history, and the works in their slots.
struct Side {
  client: Client,
  green: VolumeId,
  works: Vec<VolumeId>,
}

/// What a step answered, comparable across the two daemons: volume ids are each daemon's own, so a refusal is
/// compared by its kind and the conflict windows by what they name.
#[derive(Debug, PartialEq, Eq)]
enum Answer {
  Edited,
  Accepted(u64),
  Conflict(Vec<(String, u64, u64)>),
  Rebased(u64),
  Refused(
    std::mem::Discriminant<slates_ipc::protocol::Refusal>,
    String,
  ),
  Created(u64),
}

fn refusal_of(error: ClientError) -> Answer {
  match error {
    ClientError::Refused(refusal) => {
      let name = format!("{refusal:?}");
      let kind = name
        .split([' ', '{', '('])
        .next()
        .unwrap_or_default()
        .to_owned();
      Answer::Refused(std::mem::discriminant(&refusal), kind)
    }
    other => panic!("a step failed outside the verbs' refusals: {other}"),
  }
}

fn submitted(result: Result<Submitted, ClientError>) -> Answer {
  match result {
    Ok(Submitted::Accepted(version)) => Answer::Accepted(version),
    Ok(Submitted::Conflict(windows)) => Answer::Conflict(
      windows
        .into_iter()
        .map(|window| (window.path, window.at, window.len))
        .collect(),
    ),
    Err(error) => refusal_of(error),
  }
}

impl Side {
  /// A fresh green whose version 1 holds every file as [`SEED`], and [`WORKS`] works based on it.
  fn begin(&mut self, name: &str) {
    self.green = self.client.create_green(name, false).unwrap();
    let (seed, _) = self
      .client
      .create_work(self.green, &format!("{name}-seed"))
      .unwrap();
    for file in 0..FILES {
      self.client.edit(seed, &path_of(file), 0, 0, SEED).unwrap();
    }
    assert_eq!(self.client.submit(seed).unwrap(), Submitted::Accepted(1));
    self.works = (0..WORKS)
      .map(|slot| {
        self
          .client
          .create_work(self.green, &format!("{name}-w{slot}"))
          .unwrap()
          .0
      })
      .collect();
  }

  /// The volume in `slot`: a work, or the green for slot [`WORKS`].
  fn target(&self, slot: usize) -> VolumeId {
    self.works.get(slot).copied().unwrap_or(self.green)
  }

  fn edit(&mut self, slot: usize, splice: &Splice) -> Result<(), ClientError> {
    self.client.edit(
      self.target(slot),
      &path_of(splice.file),
      splice.at,
      splice.delete_len,
      &splice.bytes,
    )
  }

  /// One step; `combined` chooses the form of [`Step::EditAndSubmit`].
  fn run(&mut self, step: &Step, combined: bool, name: &str) -> Answer {
    match step {
      Step::NewWork { slot } => {
        let (work, base) = self.client.create_work(self.green, name).unwrap();
        self.works[*slot] = work;
        Answer::Created(base)
      }
      Step::Edit { slot, splice } => match self.edit(*slot, splice) {
        Ok(()) => Answer::Edited,
        Err(error) => refusal_of(error),
      },
      Step::Rebase { slot } => match self.client.rebase(self.target(*slot)) {
        Ok(Rebased::Rebased(version)) => Answer::Rebased(version),
        Ok(Rebased::Conflict(windows)) => Answer::Conflict(
          windows
            .into_iter()
            .map(|window| (window.path, window.at, window.len))
            .collect(),
        ),
        Err(error) => refusal_of(error),
      },
      Step::EditAndSubmit { slot, splice } if combined => submitted(self.client.edit_and_submit(
        (self.target(*slot), &path_of(splice.file)),
        splice.at,
        splice.delete_len,
        &splice.bytes,
        &[],
      )),
      Step::EditAndSubmit { slot, splice } => match self.edit(*slot, splice) {
        Ok(()) => submitted(self.client.submit(self.target(*slot))),
        Err(error) => refusal_of(error),
      },
    }
  }

  /// The green's head and every file at it.
  fn head(&mut self) -> (u64, Vec<Option<Vec<u8>>>) {
    let head = self.client.versions(self.green).unwrap();
    let files = (0..FILES)
      .map(|file| {
        self
          .client
          .read(self.green, &path_of(file), ReadAt::Head)
          .ok()
      })
      .collect();
    (head, files)
  }
}

/// A-225 (§4.16 "Edit and submit in one request"), the oracle: do run generated histories of works over one green
/// (new works, edits alone, rebases, and edit-and-submit steps) on two daemons, the combined verb on one and `edit`
/// then `submit` on the other; expect every step's answer to agree (verdicts, conflict windows, refusals), and the
/// greens' heads and files to agree after every history. Some steps aim at the green itself, which refuses the edit.
/// The combined form must also have answered at least one accept, one conflict and one refusal over the run, so the
/// comparison is not vacuous.
#[test]
fn edit_and_submit_answers_what_edit_then_submit_does_over_generated_histories() {
  let (combined_daemon, combined_client) = start("one");
  let (separate_daemon, separate_client) = start("two");
  let sides = RefCell::new((
    Side {
      client: combined_client,
      green: VolumeId::default(),
      works: Vec::new(),
    },
    Side {
      client: separate_client,
      green: VolumeId::default(),
      works: Vec::new(),
    },
  ));
  let histories = RefCell::new(0u64);
  let seen = RefCell::new((0u64, 0u64, 0u64));
  let mut runner = TestRunner::new(slates_test_seeds::unseeded(Config {
    cases: slates_test_seeds::cases(24),
    ..Config::default()
  }));
  let outcome = runner.run(&proptest::collection::vec(step(), 1..24), |steps| {
    let mut sides = sides.borrow_mut();
    let (combined, separate) = &mut *sides;
    let history = {
      let mut histories = histories.borrow_mut();
      *histories += 1;
      *histories
    };
    combined.begin(&format!("g{history}"));
    separate.begin(&format!("g{history}"));
    for (at, step) in steps.iter().enumerate() {
      let name = format!("h{history}s{at}");
      let one = combined.run(step, true, &name);
      let two = separate.run(step, false, &name);
      proptest::prop_assert_eq!(&one, &two, "step {} {:?}", at, step);
      if matches!(step, Step::EditAndSubmit { .. }) {
        let mut seen = seen.borrow_mut();
        match one {
          Answer::Accepted(_) => seen.0 += 1,
          Answer::Conflict(_) => seen.1 += 1,
          Answer::Refused(..) => seen.2 += 1,
          _ => {}
        }
      }
    }
    proptest::prop_assert_eq!(
      combined.head(),
      separate.head(),
      "the greens after the history"
    );
    Ok(())
  });
  if let Err(failure) = outcome {
    panic!("{failure}");
  }
  let (accepted, conflicted, refused) = *seen.borrow();
  assert!(
    accepted > 0 && conflicted > 0 && refused > 0,
    "the histories reached both verdicts and a refusal through the combined verb: {accepted} accepted, {conflicted} \
     conflicts, {refused} refused"
  );
  combined_daemon.stop();
  separate_daemon.stop();
}

/// A-225, exactly once across a restart: do send an edit-and-submit without reading its reply, see from a second
/// client that the green moved to version 2, replace the daemon over the same anchor-held segment, and retry the
/// request under its id; expect the retry to answer the recorded accept at version 2, the green still at version 2,
/// and the file to hold the edit once. Then do two works based on the new daemon's head edit-and-submit the same
/// bytes; expect the first accepted at version 3 and the second a conflict on them.
#[test]
fn an_edit_and_submit_retried_across_a_restart_answers_its_record_once() {
  let profile = profile();
  let instance = format!("es-restart-{}", std::process::id());
  let config = DaemonConfig::derive(&profile, &instance, Some(TEST_SHARDS));
  let segment = AnchorSegment::create(
    &format!("slates-seg-es-restart-{}", std::process::id()),
    &profile.facts.identity,
    config.geometry,
  )
  .unwrap()
  .with_content(
    &format!("slates-con-es-restart-{}", std::process::id()),
    config.content_bytes(),
  )
  .unwrap();
  let rendezvous = HeldRendezvous::hold(&instance).unwrap();
  let source = || {
    let (handoff, len) = segment.handoff().unwrap();
    let content = segment.content_handoff().unwrap();
    SegmentSource::Handoff {
      handoff,
      len,
      content,
      rendezvous: rendezvous.adoption(),
    }
  };
  let first = Daemon::start(&profile, config.clone(), source()).unwrap();
  first.bootstrap(true).unwrap();
  let mut client = connect(&instance);
  let green = client.create_green("g", false).unwrap();
  let (seed, _) = client.create_work(green, "seed").unwrap();
  client.edit(seed, "f", 0, 0, SEED).unwrap();
  assert_eq!(client.submit(seed).unwrap(), Submitted::Accepted(1));
  let (work, _) = client.create_work(green, "work").unwrap();
  client.acknowledge_all().unwrap();
  let request = RequestBody::EditSubmit {
    work,
    path: "f".to_owned(),
    at: 0,
    delete_len: 2,
    insert: EditBytes::Inline {
      bytes: b"XY".to_vec(),
    },
    evidence: Vec::new(),
  };
  let id = client.begin(&request).unwrap();
  committed_unread(&mut client, &instance, (green, 2), id);
  first.stop();
  let second = Daemon::start(&profile, config, source()).unwrap();
  second.bootstrap(true).unwrap();
  let retried = client.retry(id, &request).unwrap();
  assert!(
    matches!(
      retried,
      ReplyBody::Submitted {
        version: Some(2),
        ..
      }
    ),
    "the retry answers the recorded accept: {retried:?}"
  );
  assert_eq!(client.reconnects(), 1, "the retry reached the new daemon");
  assert_eq!(
    client.versions(green).unwrap(),
    2,
    "one version ahead, not two"
  );
  assert_eq!(
    client.read(green, "f", ReadAt::Head).unwrap(),
    b"XYcdefgh".to_vec(),
    "the edit is in the head once"
  );
  two_works_race_on_the_new_head(&mut client, green);
  second.stop();
  drop(segment);
}

/// Waits until a second client sees `green` at `version`, then takes request `id`'s reply off the ring unread, so a
/// retry can only be answered by a completion record.
fn committed_unread(
  client: &mut Client,
  instance: &str,
  (green, version): (VolumeId, u64),
  id: slates_client::RequestId,
) {
  let mut observer = connect(instance);
  let started = Instant::now();
  while observer.versions(green).unwrap() < version {
    assert!(
      started.elapsed() < START_WAIT,
      "the edit and submit commits"
    );
    std::thread::yield_now();
  }
  while client.poll_reply(id).unwrap().is_none() {
    assert!(
      started.elapsed() < START_WAIT,
      "the committed request replies"
    );
    std::thread::yield_now();
  }
}

/// Two works based on the head at version 2 edit-and-submit the same bytes: the first is accepted at 3, the second
/// is a conflict on them. A work's declared edits do not survive a restart, and each work is rebased onto its green's
/// head (`rebuild_work`), so the race is made between works made after it.
fn two_works_race_on_the_new_head(client: &mut Client, green: VolumeId) {
  let (leader, base) = client.create_work(green, "leader").unwrap();
  assert_eq!(base, 2);
  let (late, base) = client.create_work(green, "late").unwrap();
  assert_eq!(base, 2);
  assert_eq!(
    client
      .edit_and_submit((leader, "f"), 0, 2, b"QQ", &[])
      .unwrap(),
    Submitted::Accepted(3)
  );
  match client
    .edit_and_submit((late, "f"), 0, 2, b"ZZ", &[])
    .unwrap()
  {
    Submitted::Conflict(windows) => assert!(
      windows
        .iter()
        .any(|window| window.path == "f" && window.at < 2),
      "the conflict names the edited range: {windows:?}"
    ),
    other => panic!("a racing work's edit over the changed bytes answered {other:?}"),
  }
}
