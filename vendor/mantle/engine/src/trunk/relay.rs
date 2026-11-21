//! A test's relays on a pool's seats, for module-local tests of what the shard does with its
//! workers. Each seat's jobs pass a relay thread on their way to its worker, and the worker's
//! messages another on their way to the shard. The relays name what passes, by seat and kind, in
//! the order it passed, and hold what they are armed to hold:
//! - the next job of a kind: its seat is then held by it, the worker not yet given it;
//! - the next result of a kind: its worker is done with it, the shard not yet told.
//!
//! A hold lasts until it is released or the relays are opened, which [`OpenOnDrop`] does, so a
//! failed assertion leaves no worker and no shard waiting on a relay. Each wait for a fact has a
//! failure guard ([`WAIT`]) a passing run never reaches.
#![cfg(test)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::disallowed_types
)]
use super::*;
use std::sync::mpsc::sync_channel;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// The failure guard of every wait for a fact, the native worker fixtures' (pool_lifecycle.rs):
/// a passing run never waits for it, and elapsed time never counts as the fact.
pub(crate) const WAIT: Duration = Duration::from_secs(5);

/// A job's kind, as its work names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Compact,
    Sort,
    Grow,
    Index,
}

fn kind(work: &Work) -> Kind {
    match work {
        Work::Compact(_) => Kind::Compact,
        Work::Sort(_) => Kind::Sort,
        Work::Grow(_) => Kind::Grow,
        Work::Index(_) => Kind::Index,
    }
}

/// What passed a relay, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// Seat `seat` was handed a job of `kind`.
    Job { seat: usize, kind: Kind },
    /// Its worker sent the result of its job of `kind`.
    Done { seat: usize, kind: Kind },
    /// Its worker asked for `n` more extents.
    Need { seat: usize, n: usize },
    /// Its worker reported the growth it was handed as an ask's reply.
    Grown { seat: usize },
    /// A message of its worker's, released or never held, reached the shard's channel.
    Delivered { seat: usize },
}

/// What the relays have seen, and hold now.
#[derive(Clone, Debug, Default)]
pub(crate) struct Seen {
    pub(crate) events: Vec<Event>,
    /// The seats whose job is held now, and those whose result is held now.
    pub(crate) held_jobs: Vec<usize>,
    pub(crate) held_dones: Vec<usize>,
}

#[derive(Debug, Default)]
struct State {
    seen: Seen,
    /// Each seat's job now, as its relay last handed one on.
    jobs: Vec<Option<Kind>>,
    /// The next job of this kind is held on its way to its worker, and the next result of these
    /// kinds on its way to the shard.
    hold_job: Option<Kind>,
    hold_done: Vec<Kind>,
    open: bool,
}

/// One test's relays: statics of its own, so tests running at once share none.
pub(crate) struct Relays {
    state: Mutex<State>,
    changed: Condvar,
}

impl Relays {
    pub(crate) fn leak() -> &'static Self {
        Box::leak(Box::new(Self {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        }))
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Holds the next job of `kind` on its way to its worker.
    pub(crate) fn hold_job(&self, kind: Kind) {
        self.state().hold_job = Some(kind);
    }

    /// Holds the next result of a job of any of `kinds` on its way to the shard.
    pub(crate) fn hold_done(&self, kinds: &[Kind]) {
        self.state().hold_done = kinds.to_vec();
    }

    /// Lets `seat`'s held job go on to its worker.
    pub(crate) fn release_job(&self, seat: usize) {
        self.state().seen.held_jobs.retain(|&s| s != seat);
        self.changed.notify_all();
    }

    /// Lets `seat`'s held result go on to the shard.
    pub(crate) fn release_done(&self, seat: usize) {
        self.state().seen.held_dones.retain(|&s| s != seat);
        self.changed.notify_all();
    }

    /// Lets everything held go, and holds nothing more.
    pub(crate) fn open(&self) {
        let mut s = self.state();
        s.open = true;
        s.hold_job = None;
        s.hold_done.clear();
        s.seen.held_jobs.clear();
        s.seen.held_dones.clear();
        drop(s);
        self.changed.notify_all();
    }

    /// What the relays have seen so far, and hold now.
    pub(crate) fn seen(&self) -> Seen {
        self.state().seen.clone()
    }

    /// Waits until `fact` holds of what the relays have seen, failing the test past the guard,
    /// naming the fact and what was seen.
    pub(crate) fn until(&self, what: &str, fact: impl Fn(&Seen) -> bool) -> Seen {
        let deadline = std::time::Instant::now() + WAIT;
        let mut s = self.state();
        while !fact(&s.seen) {
            let left = deadline
                .checked_duration_since(std::time::Instant::now())
                .unwrap_or_else(|| {
                    panic!(
                        "{what}: not seen within the failure guard; seen {:?}",
                        s.seen
                    )
                });
            s = self
                .changed
                .wait_timeout(s, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        s.seen.clone()
    }

    /// A job for `seat` passes: named, then held while it is the one armed for.
    fn job(&self, seat: usize, kind: Kind) {
        let mut s = self.state();
        s.seen.events.push(Event::Job { seat, kind });
        if s.jobs.len() <= seat {
            s.jobs.resize(seat + 1, None);
        }
        s.jobs[seat] = Some(kind);
        if !s.open && s.hold_job == Some(kind) {
            s.hold_job = None;
            s.seen.held_jobs.push(seat);
        }
        self.changed.notify_all();
        while s.seen.held_jobs.contains(&seat) && !s.open {
            s = self
                .changed
                .wait(s)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// A message from `seat`'s worker passes: named, then held while it is the result armed for.
    fn message(&self, seat: usize, message: &Message) {
        let mut s = self.state();
        let kind = s.jobs.get(seat).copied().flatten();
        match message {
            Message::Done { .. } => {
                let Some(kind) = kind else {
                    panic!("seat {seat} sent a result with no job relayed to it");
                };
                s.seen.events.push(Event::Done { seat, kind });
                if !s.open && s.hold_done.contains(&kind) {
                    s.hold_done.clear();
                    s.seen.held_dones.push(seat);
                }
            }
            Message::Need { n, .. } => s.seen.events.push(Event::Need { seat, n: *n }),
            Message::Grown { .. } => s.seen.events.push(Event::Grown { seat }),
            Message::Ready { .. } | Message::Retired { .. } => {}
        }
        self.changed.notify_all();
        while s.seen.held_dones.contains(&seat) && !s.open {
            s = self
                .changed
                .wait(s)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

impl Relays {
    fn delivered(&self, seat: usize) {
        self.state().seen.events.push(Event::Delivered { seat });
        self.changed.notify_all();
    }
}

/// The seats with a job out, as the pool counts them: a seat is out until the pool has taken its
/// job's result off the channel, whatever the relays have passed.
pub(crate) fn seats_out(pool: &Pool) -> usize {
    pool.held()
}

/// Takes every message the workers have sent, as the pool takes them: an ask answered from the
/// room admitted or kept waiting for growth, a result kept for its owner. No result is taken for
/// an owner, so none of the engine's own is taken from it.
pub(crate) fn drain<F: BlockFile>(pool: &mut Pool, store: &mut Store<F>) -> Result<(), Error> {
    // The channel holds at most this many: each pass takes one or ends.
    for _ in 0..pool.most.saturating_mul(MESSAGES_A_WORKER) {
        if !pool.next(store, false)? {
            break;
        }
    }
    Ok(())
}

/// The result of the job `ticket` names, if the pool has taken it: every other result stays kept
/// for its own owner. The test's jobs go out as `Owner::Index`, as the engine's kept index does,
/// so only the ticket tells the test's result from the engine's.
pub(crate) fn take_ticket(pool: &mut Pool, ticket: Ticket) -> Option<Back> {
    let at = pool
        .undelivered
        .iter()
        .position(|(_, b)| b.ticket == ticket)?;
    pool.undelivered.remove(at).map(|(_, b)| b)
}

/// Opens the relays when dropped: a failed assertion leaves nothing held.
pub(crate) struct OpenOnDrop(pub(crate) &'static Relays);

impl Drop for OpenOnDrop {
    fn drop(&mut self) {
        self.0.open();
    }
}

/// The pool's spawn with `relays` on each seat: each worker opens its file by `open` and serves
/// with `config`, its jobs and messages passing the relays' threads, which end as the pool's and
/// the worker's ends of their channels close.
pub(crate) fn spawn<F: BlockFile + Send + 'static>(
    mut open: impl FnMut() -> Result<F, Error> + Send + 'static,
    config: Config,
    relays: &'static Relays,
) -> Spawn {
    Box::new(move |mut seat: Seat| {
        let file = open()?;
        let id = seat.id;
        let (to_worker, worker_jobs) = sync_channel::<Box<Job>>(1);
        let from_pool = std::mem::replace(&mut seat.jobs, worker_jobs);
        let (relay_back, mut relayed) =
            slates_rt::sync::channel(MESSAGES_A_WORKER).map_err(|error| Error::Io {
                op: "make a relay's channel back",
                detail: error.to_string(),
            })?;
        let back = std::mem::replace(&mut seat.back, relay_back);
        let thread = |name: String, f: Box<dyn FnOnce() + Send>| {
            std::thread::Builder::new()
                .name(name)
                .spawn(f)
                .map_err(|error| Error::Io {
                    op: "start a relay",
                    detail: error.to_string(),
                })
        };
        thread(
            format!("relay-jobs-{id}"),
            Box::new(move || {
                while let Ok(job) = from_pool.recv() {
                    relays.job(id, kind(&job.work));
                    if to_worker.send(job).is_err() {
                        return;
                    }
                }
            }),
        )?;
        thread(
            format!("relay-back-{id}"),
            Box::new(move || {
                while let Ok(message) = relayed.blocking_recv() {
                    relays.message(id, &message);
                    if back.blocking_send(message).is_err() {
                        return;
                    }
                    relays.delivered(id);
                }
            }),
        )?;
        std::thread::Builder::new()
            .name(format!("mantle-maint-{id}"))
            .spawn(move || serve(file, config, seat))
            .map_err(|e| Error::Io {
                op: "start a maintenance worker",
                detail: e.to_string(),
            })
    })
}
