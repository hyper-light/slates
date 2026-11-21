//! A worker is free from the moment its result is sent: [`Pool::can_take`] takes what the
//! workers sent before it refuses. Counted held until its owner took it, the kept index's
//! finished job held the one seat the trunk's measured need allowed; the index is taken back
//! only once the trunk is idle, and the trunk, refused that seat, never became idle (mantle's
//! ranges bench, `--no-inline`, 2026-10-14).
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
use hyper_block::buf::Alignment;
use hyper_block::file::{CachingRequest, DeviceFile};
use hyper_block::issuer::Issuer;
use std::sync::mpsc;

const CONFIG: Config = Config {
    page_size: 4096,
    extent_pages: 4,
    max_extents: 256,
};

fn open(path: &std::path::Path, create: bool) -> DeviceFile {
    DeviceFile::open(
        path,
        create,
        CachingRequest::Buffered,
        Alignment::new(CONFIG.page_size).unwrap(),
    )
    .unwrap()
}

/// Do: the kept index's job runs on a real worker whose messages a relay puts on the pool's
/// channel, reporting each result once it is there; with the index's result there, untaken,
/// the trunk asks for a seat within the measured need, a new pool's one worker.
/// Expect: the seat is given; the index's result is then held for its owner, who takes it,
/// naming its job, with the index.
#[test]
fn a_worker_is_free_once_its_result_is_sent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store");
    let mut store = Store::create(open(&path, true), CONFIG).unwrap();
    let issuer = Issuer::start_for(&path, 1, 1).unwrap();
    let (results_tx, results) = mpsc::channel();
    let (relays_tx, relays) = mpsc::channel();
    let worker_path = path.clone();
    let spawn: Spawn = Box::new(move |mut seat| {
        let (relay, mut relayed) = slates_rt::sync::channel(MESSAGES_A_WORKER).unwrap();
        let back = std::mem::replace(&mut seat.back, relay);
        let results_tx = results_tx.clone();
        let relaying = std::thread::Builder::new()
            .name(format!("seat-freed-relay-{}", seat.id))
            .spawn(move || {
                while let Ok(message) = relayed.blocking_recv() {
                    let result = matches!(message, Message::Done { .. });
                    if back.blocking_send(message).is_err() {
                        return;
                    }
                    if result {
                        results_tx.send(()).unwrap();
                    }
                }
            })
            .unwrap();
        relays_tx.send(relaying).unwrap();
        let file = open(&worker_path, false);
        std::thread::Builder::new()
            .name(format!("seat-freed-worker-{}", seat.id))
            .spawn(move || serve(file, CONFIG, seat))
            .map_err(|error| Error::Io {
                op: "start a seat-freed worker",
                detail: error.to_string(),
            })
    });
    let mut pool = Pool::new(spawn, 2).unwrap();
    pool.set_attach(issuer.attacher(), 1);
    assert_eq!(pool.want(), 1, "a new pool's measured need is one worker");
    let job = Box::new(Job {
        work: Work::Index(Box::default()),
        grant: Vec::new(),
        file_end: store.end(),
        generation: store.generation(),
        keep: None,
    });
    // Within the cores, as the shard sends the index.
    let ticket = pool.send(job, Owner::Index, true).unwrap().unwrap();
    // The fact the trunk's ask is made on: the index's result is on the pool's channel.
    results.recv().unwrap();
    assert!(
        pool.can_take(&mut store, false).unwrap(),
        "a seat whose result was sent is refused to the trunk"
    );
    let back = pool
        .take(&mut store, Owner::Index, false)
        .unwrap()
        .expect("the index's result is not held for its owner");
    assert_eq!(back.ticket, ticket);
    back.result.unwrap();
    assert!(
        back.indexed.is_some(),
        "the index comes back with its result"
    );
    drop(pool);
    for relaying in relays.try_iter() {
        relaying.join().unwrap();
    }
    drop(issuer);
}
