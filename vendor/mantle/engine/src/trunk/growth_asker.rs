//! A worker whose job outgrew its grant waits for extents the shard's admitted extents cannot
//! give, holding its seat. With every seat so held and no growth out, the file's growth goes to the
//! oldest waiting asker as its reply ([`Pool::send_growth`]): it adds and writes the segment whole,
//! reports it, and takes its extents once the segment is admitted. On a real segmented file, with
//! the one worker a pool of one has, a merge whose interleaved entries pack one a page where its
//! inputs packed one or two outgrows the grant of all its inputs' extents:
//! - its output reads back exactly, the newer input's values and tombstones over the older's, and
//!   every extent granted or topped up is the output's or comes back unused, each once;
//! - a segment booked before the ask and smaller than it is grown and admitted first, and the ask,
//!   still short, is handed a second growth, each reported;
//! - a growth that fails, its segment's add, comes back as that failure, which stays the pool's
//!   first error through the close, whether the close began before the report or after;
//! - a growth that unwinds comes back failed when its asker's job ends, the unwind first;
//! - a close that begins while the asker's growth is held completes only after that growth.
//!
//! Facts decide every verdict: the gate counts growths begun and finished, the runtime task
//! reports what it saw, and each wait for a fact has a failure guard ([`WAIT`]) a passing run never
//! reaches.
#![cfg(test)]
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::arithmetic_side_effects,
  clippy::disallowed_macros,
  clippy::disallowed_methods,
  clippy::disallowed_types,
  clippy::cast_possible_truncation
)]
use super::*;
use crate::branch::filter::Keys;
use crate::branch::{Builder, Op};
use hyper_block::DiskError;
use hyper_block::block::Durable;
use hyper_block::buf::Alignment;
use hyper_block::file::CachingRequest;
use hyper_block::issuer::Issuer;
use hyper_block::segmented::SegmentedFile;
use slates_rt::{Runtime, RuntimeConfig};
use std::collections::BTreeMap;
use std::sync::{Condvar, Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

const CONFIG: Config = Config {
  page_size: 4096,
  extent_pages: 4,
  max_extents: 1 << 14,
};
/// Segments a file may have: past what a case makes.
const BOUND: usize = 1 << 10;
/// Rows each input holds.
const ROWS: u64 = 200;
/// Value sizes: an older entry fills most of a page alone; a newer one, half a page.
const BIG: usize = 3000;
const MID: usize = 1500;
/// The failure guard of every wait for a fact, the native worker fixtures' (pool_lifecycle.rs):
/// a passing run never waits for it, and elapsed time never counts as the fact.
const WAIT: Duration = Duration::from_secs(5);
/// Messages a case's runtime task takes at most: a top-up ask a doubling, a growth's report
/// each, the job's result, and the close's.
const MESSAGES: usize = 64;

/// Growths on the worker's file, each its segment's add (`BlockFile::grow`), the I/O every growth
/// makes on every target: counted as they begin and as they finish; held until the gate opens;
/// the growth numbered `fail` fails, the one numbered `unwind` unwinds.
struct Gate {
  state: Mutex<GateState>,
  changed: Condvar,
}

#[derive(Clone, Copy, Debug)]
struct GateState {
  begun: u64,
  finished: u64,
  open: bool,
  fail: Option<u64>,
  unwind: Option<u64>,
}

impl Gate {
  fn leak(fail: Option<u64>, unwind: Option<u64>) -> &'static Self {
    Box::leak(Box::new(Self {
      state: Mutex::new(GateState {
        begun: 0,
        finished: 0,
        open: false,
        fail,
        unwind,
      }),
      changed: Condvar::new(),
    }))
  }

  fn state(&self) -> MutexGuard<'_, GateState> {
    self
      .state
      .lock()
      .unwrap_or_else(std::sync::PoisonError::into_inner)
  }

  fn now(&self) -> GateState {
    *self.state()
  }

  /// A growth on the worker's file: counted, held until the gate opens, then failed, unwound or
  /// made as its number says.
  fn write<T>(&self, write: impl FnOnce() -> Result<T, DiskError>) -> Result<T, DiskError> {
    let mut s = self.state();
    let me = s.begun;
    s.begun += 1;
    self.changed.notify_all();
    while !s.open {
      s = self
        .changed
        .wait(s)
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
    let (fail, unwind) = (s.fail == Some(me), s.unwind == Some(me));
    drop(s);
    if unwind {
      panic!("a test's unwound growth write");
    }
    let result = if fail {
      Err(DiskError::Io {
        op: "a test's failed growth write",
        path: std::path::PathBuf::new(),
        source: std::io::Error::other("a test's failed growth write"),
      })
    } else {
      write()
    };
    let mut s = self.state();
    s.finished += 1;
    self.changed.notify_all();
    result
  }

  /// Waits until `fact` holds of the gate, failing the test past the guard.
  fn until(&self, what: &str, fact: impl Fn(&GateState) -> bool) {
    let deadline = Instant::now() + WAIT;
    let mut s = self.state();
    while !fact(&s) {
      let left = deadline
        .checked_duration_since(Instant::now())
        .unwrap_or_else(|| panic!("{what}: not seen within the failure guard ({:?})", *s));
      s = self
        .changed
        .wait_timeout(s, left)
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .0;
    }
  }

  fn open(&self) {
    self.state().open = true;
    self.changed.notify_all();
  }
}

/// Opens the gate when dropped: a failed assertion leaves no worker held in a growth.
struct OpenOnDrop(&'static Gate);

impl Drop for OpenOnDrop {
  fn drop(&mut self) {
    self.0.open();
  }
}

/// The store's file grown by segments; the worker's growths pass its gate.
struct Grown {
  file: SegmentedFile,
  gate: Option<&'static Gate>,
}

impl BlockFile for Grown {
  fn alignment(&self) -> Alignment {
    self.file.alignment()
  }
  fn layout_block(&self) -> Alignment {
    self.file.layout_block()
  }
  fn len(&self) -> Result<u64, DiskError> {
    self.file.len()
  }
  fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> Result<(), DiskError> {
    self.file.read_exact_at(buf, offset)
  }
  fn read_resident_at(&mut self, buf: &mut [u8], offset: u64) -> Result<bool, DiskError> {
    self.file.read_resident_at(buf, offset)
  }
  fn reads_resident(&self) -> bool {
    self.file.reads_resident()
  }
  fn residency(&self) -> hyper_block::Residency {
    self.file.residency()
  }
  fn set_residency_shadow(&mut self, on: bool) {
    self.file.set_residency_shadow(on);
  }
  fn write_all_at(&self, buf: &[u8], offset: u64) -> Result<(), DiskError> {
    self.file.write_all_at(buf, offset)
  }
  fn sync_data(&self) -> Result<(), DiskError> {
    self.file.sync_data()
  }
  fn fills_new_space(&self) -> bool {
    self.file.fills_new_space()
  }
  fn grows_by_segments(&self) -> bool {
    self.file.grows_by_segments()
  }
  fn grow(&self, len: u64) -> Result<Option<u64>, DiskError> {
    match self.gate {
      Some(gate) => gate.write(|| self.file.grow(len)),
      None => self.file.grow(len),
    }
  }
  fn write_whole(&self, offset: u64, copies: u64, fill: &[u8]) -> Result<(), DiskError> {
    self.file.write_whole(offset, copies, fill)
  }
  fn segment_end(&self, offset: u64) -> Result<Option<u64>, DiskError> {
    self.file.segment_end(offset)
  }
  fn write_durable_at(&self, buf: &[u8], offset: u64) -> Result<Durable, DiskError> {
    self.file.write_durable_at(buf, offset)
  }
  fn try_clone(&self) -> Result<Self, DiskError> {
    Ok(Self {
      file: self.file.try_clone()?,
      gate: self.gate,
    })
  }
}

fn align() -> Alignment {
  Alignment::new(CONFIG.page_size).unwrap()
}

fn key(i: u64) -> Vec<u8> {
  format!("key-{i:06}").into_bytes()
}

fn value(i: u64, len: usize) -> Vec<u8> {
  (0..len).map(|b| (i as u8).wrapping_add(b as u8)).collect()
}

/// The newest entry each key reads as.
type Image = BTreeMap<Vec<u8>, (Op, Vec<u8>)>;

/// Two inputs on `store`: the older holds the even keys with values of [`BIG`] bytes; the newer
/// the odd keys with values of [`MID`], newer values of [`MID`] over every twentieth even key,
/// and tombstones over every thirtieth, offset. Their merge interleaves a big entry with a mid
/// one, which share no page, where the older packed one a page and the newer two. Returns the
/// newer, the older, and the image the merge must make: the newer over the older, its tombstones
/// kept.
fn inputs(store: &mut Store<Grown>) -> (Branch, Branch, Image) {
  let mut older = Image::new();
  let mut newer = Image::new();
  for i in 0..ROWS {
    let even = 2 * i;
    older.insert(key(even), (Op::Put, value(even, BIG)));
    newer.insert(key(even + 1), (Op::Put, value(even + 1, MID)));
    if i % 30 == 15 {
      newer.insert(key(even), (Op::Delete, Vec::new()));
    } else if i % 20 == 0 {
      newer.insert(key(even), (Op::Put, value(even + 7, MID)));
    }
  }
  let build = |store: &mut Store<Grown>, rows: &Image| {
    let mut b = Builder::new(store, Keys::Exactly(rows.len() as u64)).unwrap();
    for (k, (op, v)) in rows {
      b.add(store, k, *op, v).unwrap();
    }
    b.finish(store).unwrap()
  };
  let a = build(store, &older);
  let b = build(store, &newer);
  let mut image = older;
  image.extend(newer);
  (b, a, image)
}

/// A case's prepared parts: the shard's store on a segmented file whose admitted extents are all
/// held (the inputs' and a holder's), so the job's first ask for more waits for growth; a pool of
/// one worker over the file opened again, whose growths pass the case's gate; and the job
/// sent to it, a merge granted all its inputs' extents and one more a part, as the trunk granted
/// one.
struct Case {
  runtime: Runtime,
  // slates: the retirement owners live as long as the runtime that held them upstream (local change 1).
  _reaper: slates_rt::retirement::Reaper,
  issuer: Issuer,
  ticket: Ticket,
  grant: Vec<u64>,
  holder: Vec<u64>,
  image: Image,
  _dir: tempfile::TempDir,
}

fn case(gate: &'static Gate) -> (Case, Store<Grown>, Pool) {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("store");
  let file = Grown {
    file: SegmentedFile::create(&path, CachingRequest::Buffered, align(), BOUND).unwrap(),
    gate: None,
  };
  let mut store = Store::create(file, CONFIG).unwrap();
  let (newer, older, image) = inputs(&mut store);
  let extents = newer.extents.len() + older.extents.len();
  let needed = u64::try_from(extents + 2).unwrap();
  if store.capacity() < needed {
    store.grow_now(needed).unwrap();
  }
  let grant = store.grant(extents + 2).unwrap();
  let holder = store
    .grant(usize::try_from(store.capacity()).unwrap())
    .unwrap();
  assert_eq!(store.capacity(), 0, "every admitted extent is held");
  let issuer = Issuer::start_for(&path, 1, 1).unwrap();
  let worker_path = path.clone();
  let spawn: Spawn = Box::new(move |seat| {
    let file = Grown {
      file: SegmentedFile::open(&worker_path, CachingRequest::Buffered, align(), BOUND).unwrap(),
      gate: Some(gate),
    };
    std::thread::Builder::new()
      .name("growth-asker-worker".into())
      .spawn(move || serve(file, CONFIG, seat))
      .map_err(|error| Error::Io {
        op: "start a growth-asker worker",
        detail: error.to_string(),
      })
  });
  let mut pool = Pool::new(spawn, 1).unwrap();
  pool.set_attach(issuer.attacher(), 1);
  pool.prepare().unwrap();
  let mut runtime = runtime();
  // slates: the leases come from a `Reaper` (`slates_rt::retirement`), not the runtime (local change 1).
  let mut reaper = slates_rt::retirement::Reaper::new(1).unwrap();
  let mut leases = reaper.prepare_retirement(&[1]).unwrap();
  pool.adopt_retirement(leases.pop().unwrap()).unwrap();
  let inputs = vec![newer.for_merge(), older.for_merge()];
  let pages = inputs
    .iter()
    .map(|b| b.pages_between(&[], None))
    .sum::<usize>();
  let job = Box::new(Job {
    work: Work::Compact(Task {
      inputs,
      from: Vec::new(),
      end: None,
      drop_tombstones: false,
      per: u64::MAX,
      pages,
    }),
    grant: grant.clone(),
    file_end: store.end(),
    generation: store.generation(),
    keep: None,
  });
  let ticket = pool.send(job, Owner::Trunk, true).unwrap().unwrap();
  let case = Case {
    runtime,
    _reaper: reaper,
    issuer,
    ticket,
    grant,
    holder,
    image,
    _dir: dir,
  };
  (case, store, pool)
}

fn runtime() -> Runtime {
  let roles = 1;
  Runtime::start(&RuntimeConfig {
    shards: 1,
    tasks_per_shard: roles,
    timers_per_shard: roles,
    ring_entries: roles.max(2), // slates: two slots at least (slates_mem rings, AUD-29-33)
    step_budget_ns: 1_000_000,
    timer_tick_ns: 100_000,
    batch: roles,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
    wake_tracking: None,
  })
  .unwrap()
}

/// How the shard plays a case.
#[derive(Clone, Copy)]
struct Plan {
  /// Extents of a segment booked, and left booked, before the job asks: reused as it stands
  /// by the first growth the ask waits for.
  undersized: Option<u64>,
  /// The pool begins closing as soon as the first growth is handed.
  close_early: bool,
}

/// What the runtime task saw.
#[derive(Debug)]
enum Fact {
  /// The job's ask waited for growth, and the growth went to the asker under this ticket.
  Handed(Ticket),
  /// The pool began closing with the growth handed out.
  Closing,
  /// The case's end.
  End(Box<End>),
}

/// A case's end as the runtime task saw it: each growth's ticket and result, in order; the job's
/// result as it came back; when the job completed, a later job's ticket as sent and as its result
/// named it; the close's outcome; and the gate as the close completed.
#[derive(Debug)]
struct End {
  growths: Vec<(Ticket, Result<(), Error>)>,
  job: Option<Back>,
  later: Option<(Ticket, Ticket, Result<(), Error>)>,
  closed: Result<(), Error>,
  gate: GateState,
}

/// The shard's part, as its grow() and the trunk's take play it, in the runtime: take messages,
/// and while an ask waits for growth with none out, book a segment of `segment` extents and hand
/// it out ([`Pool::send_growth`]); admit each growth once reported and answer the asks, until the
/// job's result comes back. A failed growth, or `close_early` once the first is handed, closes the
/// pool instead. Then the close completes and the end is reported.
async fn shard(
  mut store: Store<Grown>,
  mut pool: Pool,
  gate: &'static Gate,
  segment: u64,
  plan: Plan,
  facts: mpsc::SyncSender<Fact>,
) -> Store<Grown> {
  let s = &mut store;
  if let Some(small) = plan.undersized {
    s.begin_growth(small).unwrap().unwrap();
    s.growth_abandoned();
  }
  let mut out: Option<(Ticket, u64, u64)> = None;
  let mut growths = Vec::new();
  let mut job = None;
  for _ in 0..MESSAGES {
    if job.is_none() {
      job = pool.take(s, Owner::Trunk, false).unwrap();
    }
    if out.is_none() && pool.needs_wanted() > 0 {
      let p = s.begin_growth(segment).unwrap().unwrap();
      let growth = Box::new(Job {
        work: Work::Grow(Growth {
          start: p.start,
          extents: p.extents,
          add: !p.added,
        }),
        grant: Vec::new(),
        file_end: s.end(),
        generation: s.generation(),
        keep: None,
      });
      let ticket = match pool.send_growth(s, growth) {
        Ok(Ok(ticket)) => ticket,
        Ok(Err(_)) => panic!("no worker took the growth while the only one waited for it"),
        Err((error, _)) => panic!("the growth was refused: {error:?}"),
      };
      if growths.is_empty() {
        facts.send(Fact::Handed(ticket)).unwrap();
      }
      out = Some((ticket, p.start, p.extents));
      if plan.close_early {
        pool.begin_close();
        facts.send(Fact::Closing).unwrap();
        break;
      }
    }
    if let Some((ticket, start, extents)) = out
      && let Some(back) = pool.take(s, Owner::Grow, false).unwrap()
    {
      assert_eq!(back.ticket, ticket, "the growth's result names its ticket");
      out = None;
      let result = back.result.map(drop);
      let admitted = result.is_ok();
      if admitted {
        s.growth_added();
        s.admit(start, extents).unwrap();
        pool.answer_needs(s).unwrap();
      }
      growths.push((ticket, result));
      if !admitted {
        break;
      }
      // As the shard's grow() does in the same call: an ask still short after the
      // admission is handed the next growth before anything is awaited.
      continue;
    }
    if job.is_some() {
      break;
    }
    match pool.receive().await.unwrap() {
      Some(message) => {
        pool.accept(s, message).unwrap();
      }
      None => panic!("the workers' channel closed with the job out"),
    }
  }
  // Once the job completed, the next job on the same worker: its ticket as sent, and as its
  // result names it.
  let mut later = None;
  if job.as_ref().is_some_and(|back| back.result.is_ok()) {
    let index = Box::new(Job {
      work: Work::Index(Box::default()),
      grant: Vec::new(),
      file_end: s.end(),
      generation: s.generation(),
      keep: None,
    });
    let sent = pool.send(index, Owner::Index, true).unwrap().unwrap();
    for _ in 0..MESSAGES {
      if let Some(back) = pool.take(s, Owner::Index, false).unwrap() {
        later = Some((sent, back.ticket, back.result.map(drop)));
        break;
      }
      match pool.receive().await.unwrap() {
        Some(message) => {
          pool.accept(s, message).unwrap();
        }
        None => panic!("the workers' channel closed with the later job out"),
      }
    }
  }
  // The close: refuse what waits, take every terminal result, wait for the workers to retire.
  let mut closed = Err(Error::InvalidArgument {
    what: "the close did not complete within its messages",
  });
  for _ in 0..MESSAGES {
    match pool.close_paced(s) {
      Ok(true) => {
        closed = Ok(());
        break;
      }
      Err(error) => {
        closed = Err(error);
        break;
      }
      Ok(false) => match pool.receive().await {
        Ok(Some(message)) => {
          if let Err(error) = pool.accept(s, message) {
            pool.note_close_error(error);
          }
        }
        Ok(None) => {}
        Err(error) => {
          closed = Err(error);
          break;
        }
      },
    }
    if job.is_none() {
      job = pool.take(s, Owner::Trunk, false).ok().flatten();
    }
    if let Some((ticket, _, _)) = out
      && let Ok(Some(back)) = pool.take(s, Owner::Grow, false)
    {
      assert_eq!(back.ticket, ticket, "the growth's result names its ticket");
      out = None;
      growths.push((ticket, back.result.map(drop)));
    }
  }
  let at_close = gate.now();
  drop(pool);
  facts
    .send(Fact::End(Box::new(End {
      growths,
      job,
      later,
      closed,
      gate: at_close,
    })))
    .unwrap();
  store
}

/// Runs a case: the shard's part on the runtime, the controller here, which lets the growth
/// writes go once the first is seen held (and, with `close_early`, once the close has begun).
/// Returns the end the runtime task saw, the case, and its store back.
fn run(gate: &'static Gate, plan: Plan) -> (Fact, Case, Store<Grown>) {
  let (case, store, pool) = case(gate);
  // Declared after the case, so a failing exit opens the gate before the case's runtime and
  // worker are torn down: no teardown waits on a held growth.
  let _open = OpenOnDrop(gate);
  let (facts_tx, facts) = mpsc::sync_channel(4);
  let (done_tx, done) = mpsc::sync_channel(1);
  let segment = u64::try_from(4 * (case.grant.len() + case.holder.len())).unwrap();
  case
    .runtime
    .spawn_on(case.runtime.shard_ids()[0], async move {
      let store = shard(store, pool, gate, segment, plan, facts_tx).await;
      done_tx.send(store).unwrap();
    })
    .unwrap();
  let handed = facts
    .recv_timeout(WAIT)
    .expect("the job's ask never waited for growth");
  let Fact::Handed(ticket) = handed else {
    panic!("{handed:?} before any growth was handed");
  };
  assert_eq!(
    ticket.worker, case.ticket.worker,
    "the growth goes to the one worker, whose job waits for extents"
  );
  gate.until("the asker's growth held", |s| s.begun == 1);
  if plan.close_early {
    let closing = facts.recv_timeout(WAIT).expect("the close never began");
    assert!(matches!(closing, Fact::Closing), "{closing:?}");
  }
  assert_eq!(gate.now().finished, 0, "the growth is held, not done");
  gate.open();
  let end = facts.recv_timeout(WAIT).expect("the case never ended");
  let store = done
    .recv_timeout(WAIT)
    .expect("the runtime task kept the store");
  (end, case, store)
}

/// Every entry of `branch`, in order.
fn entries(store: &mut Store<Grown>, branch: &Branch) -> Image {
  let mut out = Image::new();
  let mut c = branch.seek(store, b"").unwrap();
  for _ in 0..=branch.count {
    if !c.valid() {
      break;
    }
    out.insert(c.key().to_vec(), (c.op(), c.value().to_vec()));
    c.next(branch, store).unwrap();
  }
  assert!(!c.valid(), "a branch walked past its count");
  c.give_back(store);
  out
}

/// The case's end, from the runtime task's last fact.
fn end(fact: Fact) -> End {
  match fact {
    Fact::End(end) => *end,
    other => panic!("{other:?} where the case's end was due"),
  }
}

/// A completed case's checks: every growth went to the asker and came back whole; the merge's
/// output reads back as the newer input over the older, tombstones kept; and every extent
/// granted or topped up is the output's or comes back unused, each once.
fn completed(fact: Fact, case: &Case, store: &mut Store<Grown>, growths_at_least: usize) {
  let End {
    growths,
    job,
    later,
    closed,
    gate: at_close,
  } = end(fact);
  closed.unwrap();
  // Every ticket the pool issued names one job: the merge, each growth, and the job after them
  // on the same worker, whose result comes back under the ticket it was sent under.
  let (sent, named, done) = later.expect("the later job's result came back");
  assert_eq!(named, sent, "the later job's result names its own ticket");
  done.expect("the later job succeeded");
  let mut numbers: Vec<u64> = std::iter::once(case.ticket.job)
    .chain(growths.iter().map(|(t, _)| t.job))
    .chain(std::iter::once(sent.job))
    .collect();
  let issued = numbers.len();
  numbers.sort_unstable();
  numbers.dedup();
  assert_eq!(
    numbers.len(),
    issued,
    "two tickets share a job number: the merge {:?}, the growths {:?}, the later job {sent:?}",
    case.ticket,
    growths.iter().map(|(t, _)| *t).collect::<Vec<_>>()
  );
  assert!(growths.len() >= growths_at_least, "{growths:?}");
  for (ticket, result) in &growths {
    assert_eq!(
      ticket.worker, case.ticket.worker,
      "a growth went to the asker"
    );
    assert!(result.is_ok(), "{result:?}");
  }
  assert_eq!(
    (at_close.begun, at_close.finished),
    (growths.len() as u64, growths.len() as u64),
    "each growth one write, done"
  );
  let back = job.expect("the job's result came back");
  assert_eq!(back.ticket, case.ticket);
  assert!(
    !back.topped.is_empty(),
    "the job asked for more than its grant"
  );
  let out = back.result.unwrap();
  store.extend_end(out.end);
  let mut image = Image::new();
  let mut written: Vec<u64> = out.unused.clone();
  for (_, part) in &out.parts {
    image.extend(entries(store, part));
    written.extend(&part.extents);
  }
  assert_eq!(
    image, case.image,
    "the merge's output, newest first, tombstones kept"
  );
  let mut handed: Vec<u64> = case.grant.clone();
  handed.extend(&back.topped);
  handed.sort_unstable();
  written.sort_unstable();
  assert_eq!(
    written, handed,
    "every extent granted or topped up is the output's or unused, each once"
  );
  store.grant_back(&out.unused).unwrap();
  store.grant_back(&case.holder).unwrap();
}

/// The error a failed growth carries.
fn the_failed_write(error: &Error) -> bool {
  matches!(error, Error::Io { detail, .. } if detail.contains("a test's failed growth write"))
}

/// Do: a merge on the pool's one worker outgrows its grant while every admitted extent is held;
/// the shard books a segment and, no worker being free, hands it to the asker; its write is held
/// until seen, then let go; the shard admits it and answers. Expect: the growth went to the asker
/// under its own ticket and came back under it; the output is exact; every extent is accounted.
#[test]
fn every_seat_waiting_for_extents_grows_the_file_through_the_oldest_asker() {
  let gate = Gate::leak(None, None);
  let plan = Plan {
    undersized: None,
    close_early: false,
  };
  let (fact, case, mut store) = run(gate, plan);
  completed(fact, &case, &mut store, 1);
  drop(case.issuer);
}

/// Do: as above, but a segment of one extent was booked and left booked before the job asked,
/// so the first growth grows it as it stands and the ask is still short once it is admitted.
/// Expect: the asker is handed a second growth for the same ask; both are reported and admitted;
/// the output is exact; every extent is accounted.
#[test]
fn an_ask_past_an_undersized_booked_segment_is_handed_a_second_growth() {
  let gate = Gate::leak(None, None);
  let plan = Plan {
    undersized: Some(1),
    close_early: false,
  };
  let (fact, case, mut store) = run(gate, plan);
  completed(fact, &case, &mut store, 2);
  drop(case.issuer);
}

/// Do: as the first case, but the asker's growth, its segment's add, fails once let go. Expect:
/// the growth comes back under its ticket as that failure; the asker's job ends refused once the
/// pool closes; the close reports the growth's failure first, not the refusal.
#[test]
fn a_growth_whose_write_fails_stays_the_first_error_through_the_close() {
  let gate = Gate::leak(Some(0), None);
  let plan = Plan {
    undersized: None,
    close_early: false,
  };
  let (fact, case, _store) = run(gate, plan);
  let End {
    growths,
    job,
    closed,
    gate: at_close,
    ..
  } = end(fact);
  assert_eq!(growths.len(), 1, "{growths:?}");
  assert!(
    matches!(&growths[0].1, Err(e) if the_failed_write(e)),
    "{growths:?}"
  );
  assert_eq!((at_close.begun, at_close.finished), (1, 1));
  let back = job.expect("the refused job's result came back");
  assert_eq!(back.ticket, case.ticket);
  assert!(back.result.is_err(), "the asker's job was refused");
  assert!(
    matches!(&closed, Err(e) if the_failed_write(e)),
    "the close's first error: {closed:?}"
  );
  drop(case.issuer);
}

/// Do: the pool begins closing as soon as the growth is handed, while the asker's write is held;
/// the write then fails once let go. Expect: the close completes only after the write; the growth
/// comes back as the write's failure; the ask is refused after it; the close reports the write's
/// failure first, not the refusal.
#[test]
fn a_growth_that_fails_after_the_close_began_stays_the_first_error() {
  let gate = Gate::leak(Some(0), None);
  let plan = Plan {
    undersized: None,
    close_early: true,
  };
  let (fact, case, _store) = run(gate, plan);
  let End {
    growths,
    job,
    closed,
    gate: at_close,
    ..
  } = end(fact);
  assert_eq!(
    (at_close.begun, at_close.finished),
    (1, 1),
    "the close completed before the held growth finished"
  );
  assert_eq!(growths.len(), 1, "{growths:?}");
  assert!(
    matches!(&growths[0].1, Err(e) if the_failed_write(e)),
    "{growths:?}"
  );
  let back = job.expect("the refused job's result came back");
  assert!(back.result.is_err(), "the ask was refused after the growth");
  assert!(
    matches!(&closed, Err(e) if the_failed_write(e)),
    "the close's first error: {closed:?}"
  );
  drop(case.issuer);
}

/// Do: as the first case, but the asker's growth unwinds once let go. Expect: the asker's
/// job ends unwound, its growth unreported, and the growth comes back under its ticket failed with
/// the unwind as its cause; the worker retires and the close completes, its first error the same.
#[test]
fn a_growth_that_unwinds_comes_back_failed_when_its_askers_job_ends() {
  let gate = Gate::leak(None, Some(0));
  let plan = Plan {
    undersized: None,
    close_early: false,
  };
  let (fact, case, _store) = run(gate, plan);
  let End {
    growths,
    job,
    closed,
    gate: at_close,
    ..
  } = end(fact);
  assert_eq!(
    (at_close.begun, at_close.finished),
    (1, 0),
    "the growth began and unwound"
  );
  assert_eq!(growths.len(), 1, "{growths:?}");
  assert!(
    matches!(&growths[0].1, Err(Error::Io { detail, .. }) if detail.contains("unwound")),
    "the growth's receipt carries the unwind, as the close does: {growths:?}"
  );
  let back = job.expect("the unwound job's result came back");
  assert_eq!(back.ticket, case.ticket);
  assert!(back.result.is_err(), "the asker's job unwound");
  assert!(
    matches!(&closed, Err(Error::Io { detail, .. }) if detail.contains("unwound")),
    "the close's first error is the unwind's: {closed:?}"
  );
  drop(case.issuer);
}

/// Do: the pool begins closing as soon as the growth is handed, while the asker's write is held;
/// then the write is let go. Expect: the close completes only after the write has finished; the
/// growth comes back whole; the ask is refused after it, so the job ends refused; the close
/// completes without error.
#[test]
fn a_close_while_an_askers_growth_is_held_waits_for_its_write() {
  let gate = Gate::leak(None, None);
  let plan = Plan {
    undersized: None,
    close_early: true,
  };
  let (fact, case, _store) = run(gate, plan);
  let End {
    growths,
    job,
    closed,
    gate: at_close,
    ..
  } = end(fact);
  assert_eq!(
    (at_close.begun, at_close.finished),
    (1, 1),
    "the close completed before the held growth finished"
  );
  assert_eq!(growths.len(), 1, "{growths:?}");
  assert!(growths[0].1.is_ok(), "{growths:?}");
  let back = job.expect("the refused job's result came back");
  assert_eq!(back.ticket, case.ticket);
  assert!(back.result.is_err(), "the ask was refused after the growth");
  closed.unwrap();
  drop(case.issuer);
}

/// Do: a pool never prepared, its one worker's job outgrowing its grant while no extent is free:
/// once a relay between the worker and the pool has seen its ask for extents go by, the pool and
/// store move into a runtime task, which hands the pool a growth, then takes the workers' messages.
/// Expect: the hand-off is refused as a send is before preparation, the growth's job back
/// unchanged, with no ask taken and no growth out; the ask is still there for the take after it,
/// which defers it for growth. Back out of the runtime, the pool closes cold, refusing the ask, and
/// its worker ends.
#[test]
fn a_growth_handed_by_an_unprepared_pool_in_a_runtime_is_refused_untouched() {
  let gate = Gate::leak(None, None);
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("store");
  let file = Grown {
    file: SegmentedFile::create(&path, CachingRequest::Buffered, align(), BOUND).unwrap(),
    gate: None,
  };
  let mut store = Store::create(file, CONFIG).unwrap();
  let (newer, older, _) = inputs(&mut store);
  if store.capacity() == 0 {
    store.grow_now(1).unwrap();
  }
  let grant = store.grant(1).unwrap();
  let holder = store
    .grant(usize::try_from(store.capacity()).unwrap())
    .unwrap();
  // The worker's messages pass a relay, which reports each ask for extents it forwards.
  let (asked_tx, asked) = mpsc::channel();
  let (relays_tx, relays) = mpsc::channel();
  let worker_path = path.clone();
  let spawn: Spawn = Box::new(move |mut seat| {
    let (relay, mut relayed) = slates_rt::sync::channel(MESSAGES_A_WORKER).unwrap();
    let back = std::mem::replace(&mut seat.back, relay);
    let asked_tx = asked_tx.clone();
    let relaying = std::thread::Builder::new()
      .name("growth-asker-relay".into())
      .spawn(move || {
        while let Ok(message) = relayed.blocking_recv() {
          let need = matches!(message, Message::Need { .. });
          if back.blocking_send(message).is_err() {
            return;
          }
          if need {
            asked_tx.send(()).unwrap();
          }
        }
      })
      .unwrap();
    relays_tx.send(relaying).unwrap();
    let file = Grown {
      file: SegmentedFile::open(&worker_path, CachingRequest::Buffered, align(), BOUND).unwrap(),
      gate: Some(gate),
    };
    std::thread::Builder::new()
      .name("growth-asker-cold-worker".into())
      .spawn(move || serve(file, CONFIG, seat))
      .map_err(|error| Error::Io {
        op: "start a growth-asker worker",
        detail: error.to_string(),
      })
  });
  let mut pool = Pool::new(spawn, 1).unwrap();
  let job = Box::new(Job {
    work: Work::Compact(Task {
      inputs: vec![newer.for_merge(), older.for_merge()],
      from: Vec::new(),
      end: None,
      drop_tombstones: false,
      per: u64::MAX,
      pages: 0,
    }),
    grant,
    file_end: store.end(),
    generation: store.generation(),
    keep: None,
  });
  pool.send(job, Owner::Trunk, true).unwrap().unwrap();
  // A growth accepted by a broken refusal would write through the gate: open, and opened again
  // before anything below is torn down, so no failing exit waits on a held write.
  gate.open();
  asked
    .recv_timeout(WAIT)
    .expect("the worker's ask for extents never went by");
  let p = store.begin_growth(8).unwrap().unwrap();
  let growth = Box::new(Job {
    work: Work::Grow(Growth {
      start: p.start,
      extents: p.extents,
      add: !p.added,
    }),
    grant: Vec::new(),
    file_end: store.end(),
    generation: store.generation(),
    keep: None,
  });
  let runtime = runtime();
  let _open = OpenOnDrop(gate);
  let (back_tx, back) = mpsc::sync_channel(1);
  runtime
    .spawn_on(runtime.shard_ids()[0], async move {
      let on = (slates_rt::registry::current_shard(), pool.prepared());
      let refused = pool.send_growth(&mut store, growth);
      let before = (pool.needs_wanted(), pool.out_for(Owner::Grow));
      // The ask the refused hand-off left: taken now, and deferred for growth.
      let taken = pool.can_take(&mut store, true).map(|_| pool.needs_wanted());
      back_tx
        .send((on, refused, before, taken, pool, store))
        .unwrap();
    })
    .unwrap();
  let (on, refused, before, taken, mut pool, mut store) = back.recv_timeout(WAIT).unwrap();
  drop(runtime);
  assert!(
    on.0.is_some() && !on.1,
    "the hand-off ran on a runtime shard with the pool unprepared: {on:?}"
  );
  match refused {
    Err((Error::InvalidArgument { what }, job)) => {
      assert_eq!(what, "runtime worker admission before preparation");
      assert!(
        matches!(&job.work, Work::Grow(g) if (g.start, g.extents, g.add) == (p.start, p.extents, !p.added)),
        "the growth's job comes back unchanged"
      );
    }
    Ok(Ok(ticket)) => panic!("an unprepared pool in a runtime handed out a growth: {ticket:?}"),
    Ok(Err(_)) => {
      panic!("an unprepared pool in a runtime took messages, then gave the job back")
    }
    Err((other, _)) => panic!("refused as {other:?}"),
  }
  assert_eq!(before, (0, false), "no ask taken, no growth out");
  assert!(
    matches!(taken, Ok(n) if n > 0),
    "the ask was still there after the refusal: {taken:?}"
  );
  store.growth_abandoned();
  // Cold again: the pool closes, refusing the deferred ask, which fails its job's result, not
  // the close; its worker and its relay end.
  pool.close_blocking(&mut store).unwrap();
  drop(pool);
  for relaying in relays.try_iter() {
    relaying.join().unwrap();
  }
  store.grant_back(&holder).unwrap();
}
