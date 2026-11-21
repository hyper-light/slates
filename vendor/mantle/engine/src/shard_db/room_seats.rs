//! Who gets a seat when a growth due ahead of need and a packing held for a seat both want one
//! ([`ShardDb::grow`]), on an engine prepared as a range's is, whose owner steps a test's task
//! takes one at a time, on a store grown by segments, its pool's two seats on a test's relays
//! ([`relay`]). Each step is answered with what the owner left: whether a packing is held for a
//! seat, a growth is out or due, what waits for room, and the seats free.
//!
//! The independent holder is the test's own job, an index of nothing sent to the pool and held
//! on its seat: no owner of the engine waits on it. Facts decide every verdict: the relays name
//! each job and result as it passes, and each step's answer what the owner left. Each wait for a
//! fact has a failure guard ([`WAIT`]) a passing run never reaches.
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
use crate::branch::{Branch, Builder, Op};
use crate::ranges::Ranges;
use crate::trunk::pool::Task;
use crate::trunk::pool::relay::{self, Event, Kind, OpenOnDrop, Relays, Seen, WAIT};
use hyper_block::buf::Alignment;
use hyper_block::file::CachingRequest;
use hyper_block::issuer::Issuer;
use hyper_block::segmented::SegmentedFile;
use slates_rt::sync::{ChannelReceiver, Sender};
use slates_rt::{Runtime, RuntimeConfig};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

const STORE: Config = Config {
    page_size: 4096,
    extent_pages: 8,
    max_extents: 1 << 16,
};
const TRUNK: TrunkConfig = TrunkConfig {
    fanout: 8,
    leaf_entries: 96,
};
/// A memtable of four pages: packings come every few dozen operations.
const MEM: usize = 4 * 4096;
const OPS: u64 = 2048;
/// Operations before the case begins, each until done: the shard has measured a fill and a
/// packing by then, so a memtable may stay frozen while the next fills.
const WARM: u64 = 256;
/// Segments a file may have: past what the workload makes.
const BOUND: usize = 1 << 12;
const SLICE_NS: u64 = 50_000;
/// The work units an owner's idle slice is given.
const KEYS: u64 = 64;
/// Owner turns a step may take before it is refused: past any a passing run takes.
const TURNS: usize = 1 << 16;
/// The asker's inputs (growth_asker.rs's): rows each, the older's values of [`BIG`] bytes on the
/// even keys, the newer's of [`MID`] on the odd. Their merge puts a big entry and a mid one on no
/// shared page, where the older packed one a page and the newer two, so it outgrows its grant.
const ROWS: u64 = 200;
const BIG: usize = 3000;
const MID: usize = 1500;
/// Extents granted cold to no job, held for the steps that set the room ([`Step::ReserveTo`]):
/// past any room a case raises it to, the segment and lead a growth is due at.
const BANK: usize = 256;
/// The room the last-free-seat case gives back from the bank before the writer goes on: past
/// what the packings and compactions it makes ask.
const ROOM: u64 = 64;

fn align() -> Alignment {
    Alignment::new(STORE.page_size).unwrap()
}

fn key(i: u64) -> Vec<u8> {
    format!("bucket-{}/object-{:05}", i % 5, i % 1500).into_bytes()
}

/// The `i`th operation: a put of a value of one of several lengths, or every seventh a delete.
fn operation(i: u64) -> (Vec<u8>, Option<Vec<u8>>) {
    let value = if i.is_multiple_of(7) {
        None
    } else {
        let len = [1, 33, 100, 700][i as usize % 4];
        Some(vec![(i % 251) as u8; len])
    };
    (key(i), value)
}

fn open(path: &Path) -> Result<SegmentedFile, Error> {
    SegmentedFile::open(path, CachingRequest::Buffered, align(), BOUND).map_err(|e| Error::Io {
        op: "open a worker's file",
        detail: e.to_string(),
    })
}

fn runtime() -> Runtime {
    Runtime::start(&RuntimeConfig {
        shards: 1,
        tasks_per_shard: 4,
        timers_per_shard: 64,
        ring_entries: 64,
        step_budget_ns: SLICE_NS,
        timer_tick_ns: 100_000,
        batch: 64,
        pin: false,
        cores: Vec::new(),
        page_bytes: STORE.page_size,
        spin_ns: 0,
        wake_tracking: None,
    })
    .unwrap()
}

/// A step of the owner's, each answered with what it left.
#[derive(Debug)]
enum Step {
    /// The writer's operation `i`, tried once: done, or waiting.
    Once(u64),
    /// The writer's operation `i` until done, the owner taking a range's turns between tries.
    Until(u64),
    /// One idle slice of maintenance, as a range's idle turn takes.
    Idle,
    /// One of a range's turns ([`turn`]): results taken, an idle slice, and a wait where one is
    /// owed.
    Turn,
    /// The test's own job: an index of nothing, sent to the pool for a seat.
    Inject,
    /// The test's own merge: its inputs, written cold, merged under a grant of their extents and
    /// two more, as the trunk grants one, sent to the pool for a seat; it outgrows the grant.
    InjectAsker,
    /// The test's own job's result, taken back by its ticket once it came (the engine's own
    /// `Owner::Index` results left for it), and every extent the job wrote or read given back.
    TakeInjected,
    /// Extents granted to no job, or given back from those (the cold bank first among them), so
    /// the room admitted is exactly this many.
    ReserveTo(u64),
    /// Nothing done: what the owner holds now.
    Look,
    /// Every message the workers have sent taken, as the pool takes them (an ask answered from
    /// the room admitted, or kept waiting for growth; a result kept for its owner), no result
    /// taken for an owner, and nothing else done ([`relay::drain`]).
    Drain,
    /// One look of the owner's growth ([`ShardDb::grow`]) and nothing else.
    Grow,
    /// Extents granted to no job, so the room admitted is what the held packing's hand-off
    /// asks: it fits, and a growth is due. Given back by [`Step::Unreserve`].
    Reserve,
    Unreserve,
    /// Flush, checkpoint at the operations applied, land and finish the engine.
    Finish(u64),
}

/// What the owner left after a step.
#[derive(Clone, Copy, Debug)]
struct Witness {
    done: bool,
    /// A packing held for a seat: the writer's next rotation waits on it.
    packing_held: bool,
    growth_out: bool,
    growth_due: bool,
    /// What waits for room: the packing's ask and the workers' asks.
    packing_wanted: u64,
    needs: u64,
    /// The room admitted, and what the held packing's hand-off asks of it.
    capacity: u64,
    asks: u64,
    /// The extents of the growth out.
    growth_extents: u64,
    /// Nothing pending or in progress in the trunk.
    trunk_idle: bool,
    frozen_most: usize,
    /// The seats with a job out, as the pool counts them ([`relay::seats_out`]).
    seats_out: usize,
    /// Whether the trunk's counts of nodes with in-flight bundles and wanting a change match a
    /// look at every node ([`crate::trunk::Trunk::check_idle_counts`]).
    counts_whole: bool,
}

/// The extents a hand-off of the held packing asks, as `ShardDb::sort_off` counts them: its
/// memtable's runs, one more, and one for each of the root's pivot keys it is cut at.
fn hand_off_asks(db: &mut ShardDb<SegmentedFile>) -> u64 {
    let Some(bytes) = db.packing.as_ref().map(|p| p.mem.bytes()) else {
        return 0;
    };
    let mut cuts = Vec::new();
    db.trunk.root_cuts(&mut cuts);
    let run = db.store.run_bytes().max(1);
    let extents = crate::util::div_ceil_usize(bytes, run)
        .unwrap()
        .saturating_add(1)
        .saturating_add(cuts.len());
    u64::try_from(extents).unwrap()
}

fn witness(db: &mut ShardDb<SegmentedFile>, done: bool) -> Witness {
    let needs = db.trunk.pool_mut().map_or(0, |p| p.needs_wanted());
    Witness {
        done,
        packing_held: db.packing.as_ref().is_some_and(|p| p.sort.is_some()),
        growth_out: db.growth.is_some(),
        growth_due: db.growth_due(),
        packing_wanted: db.packing_wanted,
        needs,
        capacity: db.store.capacity(),
        asks: hand_off_asks(db),
        growth_extents: db.growth.map_or(0, |(_, _, extents)| extents),
        trunk_idle: db.trunk.is_idle(),
        frozen_most: db.frozen_most(),
        seats_out: db.trunk.pool_mut().map_or(0, |p| relay::seats_out(p)),
        counts_whole: db.trunk.check_idle_counts().is_ok(),
    }
}

/// The writer's operation `i`, tried once.
fn once(db: &mut ShardDb<SegmentedFile>, i: u64) -> Result<bool, Error> {
    let (key, value) = operation(i);
    match &value {
        Some(v) => db.put_paced(&key, v, KEYS),
        None => db.delete_paced(&key, KEYS),
    }
}

/// One of a range's turns between tries, in a range's order (`ranges.rs`'s task): what the
/// workers sent back taken, an idle slice, and then a wait for a worker's message or the device
/// where the slice left that owed, else a yield. A slice is what declares the wait (a hand-off
/// refused a seat, an ask waiting for growth), and the next try or take may clear it, so a turn
/// that looked for it first spun: a packing held for room while a growth was out took all of
/// [`TURNS`] in 211 ms without one wait, every growth slowed by 400 ms, as a zero-filled
/// segment is on a slow disk (CI windows-x86_64, run 39079182870).
async fn turn(db: &mut ShardDb<SegmentedFile>) -> Result<(), Error> {
    db.take_results()?;
    db.idle_paced_step(KEYS)?;
    if db.waiting_for_io() {
        db.wait_completion().await?;
    } else {
        slates_rt::futures::yield_now().await;
    }
    Ok(())
}

/// Tries `step` until it is done, a range's turn between tries, within [`TURNS`].
async fn until(
    db: &mut ShardDb<SegmentedFile>,
    mut step: impl FnMut(&mut ShardDb<SegmentedFile>) -> Result<bool, Error>,
) -> Result<bool, Error> {
    for _ in 0..TURNS {
        if step(db)? {
            return Ok(true);
        }
        turn(db).await?;
    }
    Err(Error::LimitExceeded {
        what: "a fixture step's turns",
        limit: u64::try_from(TURNS).unwrap_or(u64::MAX),
    })
}

/// The test's own merge, written and granted cold: its inputs, the newer then the older, and a
/// grant of their extents and two more, as the trunk grants one.
struct Asker {
    newer: Branch,
    older: Branch,
    grant: Vec<u64>,
}

/// The asker on `store`, cold: its inputs written, the file grown in place if it has not room
/// for the grant, and the grant made.
fn asker(store: &mut Store<SegmentedFile>) -> Result<Asker, Error> {
    let (newer, older) = asker_inputs(store)?;
    let extents = newer.extents.len() + older.extents.len() + 2;
    let wanted = u64::try_from(extents).unwrap();
    if store.capacity() < wanted {
        store.grow_now(wanted)?;
    }
    let grant = store.grant(extents)?;
    Ok(Asker {
        newer,
        older,
        grant,
    })
}

/// The asker's inputs on `store`, written cold: the newer, then the older.
fn asker_inputs(store: &mut Store<SegmentedFile>) -> Result<(Branch, Branch), Error> {
    let value = |i: u64, len: usize| -> Vec<u8> {
        (0..len).map(|b| (i as u8).wrapping_add(b as u8)).collect()
    };
    let key = |i: u64| format!("asker-{i:06}").into_bytes();
    let mut older = BTreeMap::new();
    let mut newer = BTreeMap::new();
    for i in 0..ROWS {
        older.insert(key(2 * i), (Op::Put, value(2 * i, BIG)));
        newer.insert(key(2 * i + 1), (Op::Put, value(2 * i + 1, MID)));
    }
    let mut build = |rows: &BTreeMap<Vec<u8>, (Op, Vec<u8>)>| -> Result<Branch, Error> {
        let mut b = Builder::new(store, Keys::Exactly(rows.len() as u64))?;
        for (k, (op, v)) in rows {
            b.add(store, k, *op, v)?;
        }
        b.finish(store)
    };
    let older = build(&older)?;
    let newer = build(&newer)?;
    Ok((newer, older))
}

/// The test's own job sent to the pool as `Owner::Index`: its ticket kept in `injected`, done
/// once a seat took it.
fn inject(
    db: &mut ShardDb<SegmentedFile>,
    injected: &mut Option<Ticket>,
    job: Box<Job>,
) -> Result<bool, Error> {
    let pool = db.trunk.pool_mut().ok_or(Error::InvalidArgument {
        what: "a fixture with no pool",
    })?;
    match pool.send(job, Owner::Index, true) {
        Ok(Ok(ticket)) => {
            *injected = Some(ticket);
            Ok(true)
        }
        Ok(Err(_)) => Ok(false),
        Err((error, _)) => Err(error),
    }
}

async fn run(
    db: &mut ShardDb<SegmentedFile>,
    reserved: &mut Vec<u64>,
    asker: &mut Option<Asker>,
    injected: &mut Option<Ticket>,
    step: Step,
) -> Result<bool, Error> {
    match step {
        Step::Look => Ok(true),
        Step::Drain => {
            let ShardDb { trunk, store, .. } = &mut *db;
            let pool = trunk.pool_mut().ok_or(Error::InvalidArgument {
                what: "a fixture with no pool",
            })?;
            relay::drain(pool, store)?;
            Ok(true)
        }
        Step::Grow => db.grow(false),
        Step::ReserveTo(target) => {
            let capacity = db.store.capacity();
            if capacity > target {
                let more = usize::try_from(capacity - target).unwrap();
                reserved.extend(db.store.grant(more)?);
            } else {
                let back = usize::try_from(target - capacity).unwrap();
                let from = reserved
                    .len()
                    .checked_sub(back)
                    .ok_or(Error::InvalidArgument {
                        what: "a reserve given back past what was reserved",
                    })?;
                let given: Vec<u64> = reserved.drain(from..).collect();
                db.store.grant_back(&given)?;
            }
            Ok(db.store.capacity() == target)
        }
        Step::InjectAsker => {
            let a = asker.as_mut().ok_or(Error::InvalidArgument {
                what: "a fixture with no asker",
            })?;
            let inputs = vec![a.newer.for_merge(), a.older.for_merge()];
            let pages = inputs.iter().map(|b| b.pages_between(&[], None)).sum();
            let job = Box::new(Job {
                work: Work::Compact(Task {
                    inputs,
                    from: Vec::new(),
                    end: None,
                    drop_tombstones: false,
                    per: u64::MAX,
                    pages,
                }),
                grant: std::mem::take(&mut a.grant),
                file_end: db.store.end(),
                generation: db.store.generation(),
                keep: None,
            });
            inject(db, injected, job)
        }
        Step::Reserve => {
            let asks = hand_off_asks(db);
            let spare = db.store.capacity().saturating_sub(asks);
            reserved.extend(db.store.grant(usize::try_from(spare).unwrap())?);
            // A growth is due at this room. The lead is learned from how the growths so far
            // landed (`note_lead`): where none was out while an allocation went on, it taught
            // none, and a segment of a packing's grant is not past this room. The lead is then
            // the least at which one is due, as a growth out while that many extents were
            // allocated teaches it.
            if db.segment_extents.saturating_add(db.growth_lead) <= asks {
                db.growth_lead = asks + 1 - db.segment_extents;
            }
            Ok(true)
        }
        Step::Unreserve => {
            db.store.grant_back(reserved)?;
            reserved.clear();
            Ok(true)
        }
        Step::Once(i) => once(db, i),
        Step::Until(i) => until(db, |db| once(db, i)).await,
        Step::Idle => db.idle_paced_step(KEYS).map(|_| true),
        Step::Turn => turn(db).await.map(|()| true),
        Step::Inject => {
            let job = Box::new(Job {
                work: Work::Index(Box::default()),
                grant: Vec::new(),
                file_end: db.store.end(),
                generation: db.store.generation(),
                keep: None,
            });
            inject(db, injected, job)
        }
        Step::TakeInjected => {
            let ticket = injected.take().ok_or(Error::InvalidArgument {
                what: "no job of the test's out to take",
            })?;
            for _ in 0..TURNS {
                let ShardDb { trunk, store, .. } = &mut *db;
                let pool = trunk.pool_mut().ok_or(Error::InvalidArgument {
                    what: "a fixture with no pool",
                })?;
                if let Some(back) = relay::take_ticket(pool, ticket) {
                    // What the job made and was granted is no one's: every extent back.
                    let out = back.result?;
                    for (_, branch) in &out.parts {
                        for &e in &branch.extents {
                            store.release(e)?;
                        }
                    }
                    store.grant_back(&out.unused)?;
                    if let Some(a) = asker.take() {
                        for &e in a.newer.extents.iter().chain(&a.older.extents) {
                            store.release(e)?;
                        }
                    }
                    return Ok(true);
                }
                match pool.receive().await? {
                    Some(message) => {
                        pool.accept(store, message)?;
                    }
                    None => {
                        return Err(Error::InvalidArgument {
                            what: "the workers' channel closed with the test's job out",
                        });
                    }
                }
            }
            Ok(false)
        }
        Step::Finish(applied) => {
            until(db, |db| db.flush_paced(KEYS)).await?;
            until(db, |db| db.checkpoint_paced(applied, KEYS)).await?;
            db.land()?;
            Ok(true)
        }
    }
}

/// The owner's task: each step run and answered, until the finish, which is answered once the
/// engine's workers and file have retired, or until a step fails.
async fn owner(
    mut db: ShardDb<SegmentedFile>,
    mut asker: Option<Asker>,
    mut reserved: Vec<u64>,
    mut steps: ChannelReceiver<Step>,
    answers: mpsc::SyncSender<Result<Witness, Error>>,
) {
    let mut injected = None;
    while let Ok(step) = steps.recv().await {
        let finish = matches!(step, Step::Finish(_));
        let answer = run(&mut db, &mut reserved, &mut asker, &mut injected, step)
            .await
            .map(|done| witness(&mut db, done));
        let failed = answer.is_err();
        if finish && !failed {
            let finished = db.finish_async().await;
            let retired = db.actor_finished();
            drop(db);
            let answer = match finished {
                Err(error) => Err(error),
                Ok(()) if retired => answer,
                Ok(()) => Err(Error::InvalidArgument {
                    what: "a finished fixture whose workers and file did not retire",
                }),
            };
            drop(answers.send(answer));
            return;
        }
        if answers.send(answer).is_err() || failed {
            break;
        }
    }
    drop(db.finish_async().await);
}

/// An engine on a new store grown by segments at `path`, its pool's two seats on `relays`, its
/// owner's steps taken by [`owner`] on the runtime.
struct Fixture {
    steps: Sender<Step>,
    answers: mpsc::Receiver<Result<Witness, Error>>,
    oracle: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    path: PathBuf,
    // Declared last, dropped last: the runtime outlives the steps' sender and the answers.
    _issuer: Issuer,
    runtime: Option<Runtime>,
}

impl Fixture {
    /// With `asker`, the asker's inputs written cold first ([`Step::InjectAsker`]).
    fn start(dir: &Path, relays: &'static Relays, asker: bool) -> Self {
        let path = dir.join("store");
        let issuer = Issuer::start_for(dir, 2, 2).unwrap();
        let mut runtime = runtime();
        let created =
            SegmentedFile::create(&path, CachingRequest::Buffered, align(), BOUND).unwrap();
        let mut db = ShardDb::create(created, STORE, MEM, TRUNK).unwrap();
        let asker = asker.then(|| self::asker(&mut db.store).unwrap());
        // The bank, cold: the file grown in place if it has not room for it.
        let banked = u64::try_from(BANK).unwrap();
        if db.store.capacity() < banked {
            db.store.grow_now(banked).unwrap();
        }
        let bank = db.store.grant(BANK).unwrap();
        // The owner's write roles and two workers' (ranges_growth_asker.rs's recipe gives one).
        let run = STORE.page_size * usize::try_from(STORE.extent_pages).unwrap();
        db.set_memory(3 * (issuer.depth() + 2) * run).unwrap();
        db.attach(&issuer, issuer.depth()).unwrap();
        let worker = path.clone();
        db.set_workers(move || open(&worker)).unwrap();
        // The same pool with its two seats on the relays: no worker has started yet.
        let relayed = path.clone();
        let mut pool = Pool::new(relay::spawn(move || open(&relayed), STORE, relays), 2).unwrap();
        if let Some((attacher, batches)) = &db.worker_attach {
            pool.set_attach(attacher.clone(), *batches);
        }
        db.trunk.set_pool(pool);
        let (steps, received) = slates_rt::sync::channel::<Step>(1).unwrap();
        let (answer, answers) = mpsc::sync_channel(1);
        Ranges::start_fixture(&mut runtime, db, move |db| {
            owner(db, asker, bank, received, answer)
        })
        .unwrap();
        Self {
            steps,
            answers,
            oracle: BTreeMap::new(),
            path,
            _issuer: issuer,
            runtime: Some(runtime),
        }
    }

    /// Runs `step` on the owner and returns what it left; a failed step fails the test.
    fn step(&mut self, step: Step) -> Witness {
        let what = format!("{step:?}");
        let op = match step {
            Step::Once(i) | Step::Until(i) => Some(i),
            _ => None,
        };
        self.steps.blocking_send(step).unwrap();
        let w = self
            .answers
            .recv_timeout(WAIT)
            .unwrap_or_else(|e| panic!("the owner's answer to {what}: not received ({e})"))
            .unwrap_or_else(|e| panic!("the owner's {what} failed: {e:?}"));
        assert!(
            w.counts_whole,
            "the trunk's idle counts differ from its nodes after {what}: {w:?}"
        );
        if let Some(i) = op
            && w.done
        {
            let (key, value) = operation(i);
            self.oracle.insert(key, value);
        }
        w
    }

    /// A range's turns until no growth is out, the trunk owes nothing and no seat has a job out,
    /// as the pool counts them (a result it has not taken keeps a seat out): from then only the
    /// steps the test takes move the engine. Each turn waits for what it is owed, so the turns
    /// count what came, not how long it took.
    fn quiet(&mut self) -> Witness {
        let mut last = None;
        for _ in 0..TURNS {
            let w = self.step(Step::Turn);
            if !w.growth_out && w.trunk_idle && w.seats_out == 0 {
                return w;
            }
            last = Some(w);
        }
        panic!(
            "no growth out, the trunk idle and no seat out: not seen within the turns, the last \
             step leaving {last:?}"
        );
    }

    /// The writer's operations `from..OPS`, flushed, checkpointed and finished; the runtime
    /// stopped; then the store reopened cold, every value and tombstone read back exactly, and
    /// its references whole.
    fn finish(mut self, from: u64) {
        for i in from..OPS {
            assert!(self.step(Step::Until(i)).done);
        }
        assert!(self.step(Step::Finish(OPS)).done);
        drop(self.runtime.take());
        let (mut db, applied) =
            ShardDb::open(open(&self.path).unwrap(), STORE, MEM, TRUNK).unwrap();
        assert_eq!(applied, OPS);
        let mut rows = Rows::new();
        let mut next = Vec::new();
        assert!(
            !db.scan(b"", None, usize::MAX, &mut rows, &mut next)
                .unwrap()
        );
        let got: Vec<(Vec<u8>, Vec<u8>)> =
            rows.iter().map(|(k, v)| (k.to_vec(), v.to_vec())).collect();
        let want: Vec<(Vec<u8>, Vec<u8>)> = self
            .oracle
            .iter()
            .filter_map(|(k, v)| v.as_ref().map(|v| (k.clone(), v.clone())))
            .collect();
        assert_eq!(got, want);
        db.check_references().unwrap();
    }
}

/// The jobs handed `seat` after the `from`th event, in order.
fn jobs_after(seen: &Seen, from: usize, seat: usize) -> Vec<Kind> {
    seen.events
        .iter()
        .skip(from)
        .filter_map(|e| match *e {
            Event::Job { seat: s, kind } if s == seat => Some(kind),
            _ => None,
        })
        .collect()
}

/// Do: two seats, from a quiet engine, one held by the test's own job; the other's next result
/// held; the writer's
/// operations tried once each until a packing is held for a seat, with a growth due ahead of
/// need and nothing waiting for room. Let the held result reach the shard, then take one idle
/// slice, with growths held as a seat takes them.
/// Expect: the freed seat's next job is the packing, and no growth is out; the writer's waiting
/// operation then completes, the test's job still held; let everything go, and every value and
/// tombstone reads back exactly after a checkpoint and a cold reopen.
#[test]
fn the_last_free_seat_goes_to_the_packing_held_for_one_not_a_growth_due_ahead_of_need() {
    let dir = tempfile::tempdir().unwrap();
    let relays = Relays::leak();
    let mut fx = Fixture::start(dir.path(), relays, false);
    // Declared after the fixture, it drops first: a failed assertion lets everything held go
    // before the runtime's teardown joins the workers.
    let _open = OpenOnDrop(relays);
    let mut i = 0;
    while i < WARM {
        assert!(fx.step(Step::Until(i)).done);
        i += 1;
    }
    // A quiet engine: the seat the test's job leaves is free, not held by a result the pool
    // has yet to take.
    let w = fx.quiet();
    assert!(
        w.frozen_most >= 1,
        "no frozen memtable allowed after warming: {w:?}"
    );
    // The independent holder: the test's own job, held on its seat.
    relays.hold_job(Kind::Index);
    assert!(fx.step(Step::Inject).done, "the test's job got no seat");
    let holder = relays
        .until("the test's job held on its seat", |s| {
            s.held_jobs.len() == 1
        })
        .held_jobs[0];
    let free = 1 - holder;
    // Room enough that no hand-off from here waits for it: the packing that comes to be held
    // waits for a seat alone.
    let w = fx.step(Step::Look);
    assert!(fx.step(Step::ReserveTo(w.capacity + ROOM)).done);
    // The free seat's next result is held: from then both seats are held.
    relays.hold_done(&[Kind::Sort, Kind::Compact, Kind::Grow]);
    // The writer's operations, once each, until a packing is held for a seat: the free seat is
    // taken by a job whose result is held once it is done.
    let mut held = None;
    for _ in 0..OPS {
        let w = fx.step(Step::Once(i));
        if w.done {
            i += 1;
        }
        if w.packing_held {
            held = Some(w);
            break;
        }
        assert!(
            w.done,
            "the writer waits before a packing was held for a seat: {w:?}; {:?}",
            relays.seen()
        );
    }
    let held = held.expect("no packing held for a seat within the operations");
    relays.until(
        &format!("the free seat's result held, the packing held with {held:?}"),
        |s| s.held_dones == [free],
    );
    // The room admitted is what the held packing's hand-off asks: it fits, a growth is due.
    let w = fx.step(Step::Reserve);
    assert!(
        w.packing_held
            && w.capacity == w.asks
            && w.growth_due
            && !w.growth_out
            && w.packing_wanted == 0
            && w.needs == 0,
        "the setup: a packing held for a seat that fits the room admitted, a growth due ahead \
         of need, nothing waiting for room: {w:?}"
    );
    // Growths are held as a seat takes them; the held result reaches the shard; one idle slice.
    relays.hold_job(Kind::Grow);
    let from = relays.seen().events.len();
    relays.release_done(free);
    relays.until("the held result delivered", |s| {
        s.events
            .iter()
            .skip(from)
            .any(|e| *e == Event::Delivered { seat: free })
    });
    let after = fx.step(Step::Idle);
    let seen = relays.until("the freed seat's next job", |s| {
        !jobs_after(s, from, free).is_empty()
    });
    assert_eq!(
        jobs_after(&seen, from, free)[0],
        Kind::Sort,
        "the freed seat went to a growth before the packing the writer waits on; {after:?}"
    );
    assert!(
        !after.growth_out && !after.packing_held,
        "after the slice: {after:?}"
    );
    // The writer's waiting operation completes, the test's job still held.
    assert!(fx.step(Step::Until(i)).done);
    i += 1;
    assert!(relays.seen().held_jobs.contains(&holder));
    // Everything goes: the room reserved, the test's job, then every hold; the writer finishes.
    assert!(fx.step(Step::Unreserve).done);
    relays.release_job(holder);
    assert!(fx.step(Step::TakeInjected).done);
    relays.open();
    fx.finish(i);
}

/// The first ask for more after the `from`th event, once it is on the shard's channel: its seat
/// and what it asks. A seat's relay hands its worker's messages on one at a time, so the seat's
/// first delivery after the ask is the ask's.
fn ask_delivered(seen: &Seen, from: usize) -> Option<(usize, u64)> {
    let mut after = seen.events.iter().skip(from);
    let (seat, n) = after.by_ref().find_map(|e| match *e {
        Event::Need { seat, n } => Some((seat, n)),
        _ => None,
    })?;
    after
        .any(|e| *e == Event::Delivered { seat })
        .then(|| (seat, u64::try_from(n).unwrap()))
}

/// Do: two seats, from a quiet engine (no growth out, the trunk idle, both seats free). The
/// test's own merge outgrows its grant on one and asks for n more, with the room one short of n.
/// The owner takes the ask, which waits, and one look of its growth books a growth sized to it on
/// the other seat, its result held; the merge's result is held too. The writer's operations go
/// once each until a packing is held for a seat. The room is set so that, once the growth's
/// segment is admitted and the ask answered, what is left is that packing's ask, smaller than n.
/// Growths are held as a seat takes them, and the growth's result reaches the shard.
/// Expect: one look of the owner's growth lands it, answers the ask and books no growth; the
/// next idle slice hands the freed seat to the packing, not a growth. The writer's waiting
/// operation then completes; let everything go, and every value and tombstone reads back exactly
/// after a checkpoint and a cold reopen, the references whole.
#[test]
fn an_ask_answered_as_its_growth_lands_leaves_the_freed_seat_to_the_packing_that_fits() {
    let dir = tempfile::tempdir().unwrap();
    let relays = Relays::leak();
    let mut fx = Fixture::start(dir.path(), relays, true);
    // Declared after the fixture, it drops first: a failed assertion lets everything held go
    // before the runtime's teardown joins the workers.
    let _open = OpenOnDrop(relays);
    let mut i = 0;
    while i < WARM {
        assert!(fx.step(Step::Until(i)).done);
        i += 1;
    }
    // A quiet engine: from here only the steps below move it.
    fx.quiet();
    // The asker on a seat; its ask for more is on the shard's channel before the owner takes it.
    let from = relays.seen().events.len();
    assert!(fx.step(Step::InjectAsker).done, "the asker got no seat");
    let seen = relays.until("the asker's ask on the shard's channel", |s| {
        ask_delivered(s, from).is_some()
    });
    let (asker, n) = ask_delivered(&seen, from).unwrap();
    let free = 1 - asker;
    // The room one short of the ask: the ask waits for growth, and a packing the writer hands
    // off before the growth lands fits it.
    assert!(fx.step(Step::ReserveTo(n - 1)).done);
    // The owner takes the ask, which waits; one look books the growth for it, sized to it, and
    // hands it to the other seat, where its result is held.
    relays.hold_done(&[Kind::Grow]);
    let w = fx.step(Step::Drain);
    assert!(
        w.needs == n && !w.growth_out,
        "the ask taken and waiting: {w:?}, ask {n}"
    );
    let w = fx.step(Step::Grow);
    assert!(
        w.growth_out && w.growth_extents >= n,
        "a growth booked for the ask: {w:?}, ask {n}"
    );
    relays.until("the growth's result held on the other seat", |s| {
        s.held_dones == [free]
    });
    // From here the merge's result is held too, so its seat stays its own.
    relays.hold_done(&[Kind::Compact]);
    // The writer's operations, once each, until a packing is held for a seat.
    let mut held = None;
    for _ in 0..OPS {
        let w = fx.step(Step::Once(i));
        if w.done {
            i += 1;
        }
        if w.packing_held {
            held = Some(w);
            break;
        }
        assert!(
            w.done,
            "the writer waits before a packing was held for a seat: {w:?}"
        );
    }
    let w = held.expect("no packing held for a seat within the operations");
    // Once the growth's segment is admitted and the ask answered, what is left is the packing's
    // ask; the ask answered is larger than it.
    assert!(
        w.growth_out && w.needs == n && w.packing_wanted == 0,
        "the setup: {w:?}, ask {n}"
    );
    assert!(
        n > w.asks && w.growth_extents <= w.asks + n,
        "the setup: an ask larger than the packing's, a segment within both: {w:?}, ask {n}"
    );
    let target = w.asks + n - w.growth_extents;
    let w = fx.step(Step::ReserveTo(target));
    assert!(
        w.done && w.packing_held && w.packing_wanted == 0,
        "the setup: {w:?}"
    );
    // Growths are held as a seat takes them; the growth's result reaches the shard.
    relays.hold_job(Kind::Grow);
    let from = relays.seen().events.len();
    relays.release_done(free);
    relays.until("the growth's result delivered", |s| {
        s.events
            .iter()
            .skip(from)
            .any(|e| *e == Event::Delivered { seat: free })
    });
    // One look of the owner's growth lands it and answers the ask, and books no growth.
    let first = fx.step(Step::Grow);
    assert!(
        !first.growth_out && first.needs == 0 && first.capacity == first.asks,
        "the growth landed, the ask answered, the room left the packing's ask: {first:?}"
    );
    // The next slice hands the freed seat to the packing.
    let second = fx.step(Step::Idle);
    let seen = relays.until("the freed seat's next job", |s| {
        !jobs_after(s, from, free).is_empty()
    });
    assert_eq!(
        jobs_after(&seen, from, free)[0],
        Kind::Sort,
        "the freed seat went to a growth before the packing that fits; {first:?}; {second:?}"
    );
    assert!(
        !second.growth_out && !second.packing_held,
        "after the slices: {second:?}"
    );
    // The writer's waiting operation completes.
    assert!(fx.step(Step::Until(i)).done);
    i += 1;
    // Everything goes: the room reserved, every hold; the merge's result taken, its extents
    // and its inputs' given back; the writer finishes.
    assert!(fx.step(Step::Unreserve).done);
    relays.open();
    assert!(fx.step(Step::TakeInjected).done);
    fx.finish(i);
}
