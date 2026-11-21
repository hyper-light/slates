//! A ready compaction no seat can take waits for a worker's message: only one frees a seat, so
//! the step that found every seat held says so, and the shard's task waits for that message
//! rather than pass again. Seen as a range spinning with one worker, its seat held by a growth
//! while the root, out of room, waited on its compaction: the writer's rotation never went on
//! and the shard never parked (ranges_room.rs on 2 CPUs beside 2 busy loops, 2026-10-16).
//!
//! Facts decide every verdict: the relays name each job and message as it passes, and each wait
//! for a fact has their failure guard (`relay::WAIT`), which a passing run never reaches.
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
use crate::branch::Builder;
use crate::branch::filter::Keys;
use crate::store::Config;
use crate::trunk::pool::Job;
use crate::trunk::pool::relay::{self, Event, Kind, OpenOnDrop, Relays};
use hyper_block::buf::Alignment;
use hyper_block::file::{CachingRequest, DeviceFile};
use hyper_block::issuer::Issuer;

const CONFIG: Config = Config {
    page_size: 4096,
    extent_pages: 4,
    max_extents: 1 << 12,
};
const TRUNK: TrunkConfig = TrunkConfig {
    fanout: 4,
    leaf_entries: 64,
};
/// The rows of the branch the root takes: more than a leaf holds, so the root plans a job.
const ROWS: u64 = 512;
/// The work units a paced step is given, as a range's slice gives them.
const KEYS: u64 = 64;

fn open(path: &std::path::Path, create: bool) -> DeviceFile {
    DeviceFile::open(
        path,
        create,
        CachingRequest::Buffered,
        Alignment::new(CONFIG.page_size).unwrap(),
    )
    .unwrap()
}

fn key(i: u64) -> Vec<u8> {
    format!("key-{i:06}").into_bytes()
}

fn value(i: u64) -> Vec<u8> {
    vec![(i % 251) as u8; 48]
}

/// Whether a compaction waits for a seat: planned for a worker, not yet out.
fn ready(trunk: &Trunk) -> bool {
    trunk.tasks.iter().any(|t| {
        matches!(
            t.run,
            Running::Remote {
                state: State::Ready(_),
                ..
            }
        )
    })
}

/// Do: a pool of one seat, held by a job the trunk does not own; a branch of more rows than a
/// leaf holds enters the root, and one paced step runs.
/// Expect: the root's compaction is ready, no seat takes it, and the trunk says the shard waits
/// for a worker's message. Let the held job go: once its result is on the channel, the next
/// paced step hands the compaction the freed seat, which its worker takes (its result may come
/// back within that step: on one CPU the worker ran it and the step applied it, 55 of 300 runs,
/// 2026-10-17). Drained, every row reads back.
#[test]
fn a_compaction_no_seat_takes_waits_for_a_workers_message() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let mut store = Store::create(open(&path, true), CONFIG).unwrap();
    let issuer = Issuer::start_for(&path, 1, 1).unwrap();
    let relays = Relays::leak();
    let worker_path = path.clone();
    let mut pool = Pool::new(
        relay::spawn(move || Ok(open(&worker_path, false)), CONFIG, relays),
        1,
    )
    .unwrap();
    pool.set_attach(issuer.attacher(), 1);
    // The seat's holder: a job of the kept index's kind, which the trunk does not own.
    relays.hold_job(Kind::Index);
    let job = Box::new(Job {
        work: Work::Index(Box::default()),
        grant: Vec::new(),
        file_end: store.end(),
        generation: store.generation(),
        keep: None,
    });
    let ticket = pool.send(job, Owner::Index, true).unwrap().unwrap();
    let mut trunk = Trunk::new(TRUNK).unwrap();
    trunk.set_pool(pool);
    // Declared after the trunk, it drops first: a failed assertion lets the held job go before
    // the pool's teardown joins its worker.
    let _open = OpenOnDrop(relays);
    let seat = relays
        .until("the holder on its seat", |s| s.held_jobs.len() == 1)
        .held_jobs[0];
    let mut b = Builder::new(&mut store, Keys::Exactly(ROWS)).unwrap();
    for i in 0..ROWS {
        b.add(&mut store, &key(i), Op::Put, &value(i)).unwrap();
    }
    trunk.add(b.finish(&mut store).unwrap());
    trunk.clear_io_wait();
    trunk.step_paced(&mut store, KEYS).unwrap();
    assert!(
        ready(&trunk) && !trunk.workers_out(),
        "the setup: the root's compaction ready, none out"
    );
    assert!(
        trunk.waiting_for_worker(),
        "a compaction no seat takes: the shard is not told to wait for the message that frees one"
    );
    // The holder goes on; its result reaches the channel; the next step takes the freed seat.
    let from = relays.seen().events.len();
    relays.release_job(seat);
    relays.until("the holder's result on the shard's channel", |s| {
        s.events
            .iter()
            .skip(from)
            .any(|e| *e == Event::Delivered { seat })
    });
    trunk.clear_io_wait();
    trunk.step_paced(&mut store, KEYS).unwrap();
    assert!(!ready(&trunk), "the compaction did not take the freed seat");
    relays.until("the compaction on the freed seat", |s| {
        s.events.iter().skip(from).any(|e| {
            *e == Event::Job {
                seat,
                kind: Kind::Compact,
            }
        })
    });
    // Drained: each step waits for every job out, and planning is bounded by the rows.
    for _ in 0..=ROWS {
        if trunk.is_idle() {
            break;
        }
        trunk.step(&mut store, u64::MAX).unwrap();
    }
    assert!(trunk.is_idle(), "the trunk's maintenance did not finish");
    let back = relay::take_ticket(trunk.pool_mut().unwrap(), ticket)
        .expect("the holder's result kept for its owner");
    back.result.unwrap();
    let mut out = Vec::new();
    for i in 0..ROWS {
        assert_eq!(
            trunk.get(&mut store, &key(i), &mut out).unwrap(),
            Some(Op::Put),
            "row {i}"
        );
        assert_eq!(out, value(i), "row {i}");
    }
    drop(trunk);
    drop(issuer);
}
