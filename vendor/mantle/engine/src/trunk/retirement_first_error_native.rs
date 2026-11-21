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
use hyper_block::DiskError;
use hyper_block::buf::Alignment;
use hyper_block::file::{CachingRequest, DeviceFile};
use hyper_block::issuer::Issuer;
use slates_rt::{RtError, Runtime, RuntimeConfig};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};

const CONFIG: Config = Config {
  page_size: 4096,
  extent_pages: 4,
  max_extents: 256,
};
const WITNESS: &str = "first physical write witness";

struct File {
  native: DeviceFile,
  worker: bool,
  refused: Arc<AtomicUsize>,
}
impl BlockFile for File {
  fn alignment(&self) -> Alignment {
    self.native.alignment()
  }
  fn len(&self) -> Result<u64, DiskError> {
    self.native.len()
  }
  fn read_exact_at(&self, bytes: &mut [u8], offset: u64) -> Result<(), DiskError> {
    self.native.read_exact_at(bytes, offset)
  }
  fn write_all_at(&self, bytes: &[u8], offset: u64) -> Result<(), DiskError> {
    if self.worker {
      self.refused.fetch_add(1, Ordering::SeqCst);
      return Err(DiskError::Io {
        op: WITNESS,
        path: Default::default(),
        source: std::io::Error::other(WITNESS),
      });
    }
    self.native.write_all_at(bytes, offset)
  }
  fn sync_data(&self) -> Result<(), DiskError> {
    self.native.sync_data()
  }
  fn try_clone(&self) -> Result<Self, DiskError> {
    Ok(Self {
      native: self.native.try_clone()?,
      worker: self.worker,
      refused: Arc::clone(&self.refused),
    })
  }
}
struct Returned(Arc<AtomicUsize>);
impl Drop for Returned {
  fn drop(&mut self) {
    self.0.fetch_add(1, Ordering::SeqCst);
  }
}
fn open(path: &std::path::Path, create: bool, worker: bool, refused: &Arc<AtomicUsize>) -> File {
  File {
    native: DeviceFile::open(
      path,
      create,
      CachingRequest::Buffered,
      Alignment::new(CONFIG.page_size).unwrap(),
    )
    .unwrap(),
    worker,
    refused: Arc::clone(refused),
  }
}
fn runtime() -> Runtime {
  let roles = 1; // One terminal actor; the native maintenance worker is independently adopted.
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

#[test]
fn a_physical_job_failure_stays_first_after_native_worker_join_failure() {
  let dir = tempfile::tempdir().unwrap();
  let path = dir.path().join("store");
  let refused = Arc::new(AtomicUsize::new(0));
  let returned = Arc::new(AtomicUsize::new(0));
  let mut store = Store::create(open(&path, true, false, &refused), CONFIG).unwrap();
  let issuer = Issuer::start_for(&path, 1, 1).unwrap();
  let worker_path = path.clone();
  let worker_refused = Arc::clone(&refused);
  let worker_returned = Arc::clone(&returned);
  let spawn: Spawn = Box::new(move |seat| {
    let file = open(&worker_path, false, true, &worker_refused);
    let capture = Returned(Arc::clone(&worker_returned));
    std::thread::Builder::new()
      .name("physical-first-worker".into())
      .spawn(move || {
        let _capture = capture;
        serve(file, CONFIG, seat);
        // The only ordinary thread-panic injection permitted for this fixture.
        // serve returned after physical drain and channel EOF, not while a job owns I/O.
        panic!("ordinary test thread failure after physical write refusal");
      })
      .map_err(|error| Error::Io {
        op: "start physical-first worker",
        detail: error.to_string(),
      })
  });
  let mut pool = Pool::new(spawn, 1).unwrap();
  pool.set_attach(issuer.attacher(), 1);
  pool.prepare().unwrap();
  // slates: the leases come from a `Reaper` (`slates_rt::retirement`), not the runtime (local change 1); made before
  // the runtime, so it drops after it, as the runtime's own owners did upstream.
  let mut reaper = slates_rt::retirement::Reaper::new(1).unwrap();
  let mut rt = runtime();
  let mut leases = reaper.prepare_retirement(&[1]).unwrap();
  pool.adopt_retirement(leases.pop().unwrap()).unwrap();
  let mut mem = crate::memtable::hashed::HashMem::new(1 << 16).unwrap();
  mem.mirror_into(Vec::new()).unwrap();
  mem
    .insert(b"written", crate::branch::Op::Put, b"value")
    .unwrap();
  let job = Box::new(Job {
    work: Work::Sort(SortPack {
      entries: 1,
      skip: mem.skip(),
      arena: mem.take_mirror().unwrap(),
      sorter: crate::memtable::hashed::Sorter::default(),
      keys: Default::default(),
      cuts: Vec::new(),
      ends: Vec::new(),
    }),
    grant: store
      .grant(usize::try_from(CONFIG.extent_pages).unwrap())
      .unwrap(),
    file_end: store.end(),
    generation: store.generation(),
    keep: None,
  });
  assert!(pool.send(job, Owner::Pack, true).unwrap().is_ok());
  let (answer, result) = mpsc::sync_channel(1);
  let observation = Arc::clone(&returned);
  rt.spawn_on(rt.shard_ids()[0], async move {
    let result = loop {
      match pool.close_paced(&mut store) {
        Ok(true) => break Ok(()),
        Err(error) => break Err(error),
        Ok(false) => match pool.receive().await {
          Ok(Some(message)) => {
            if let Err(error) = pool.accept(&mut store, message) {
              pool.note_close_error(error);
            }
          }
          Ok(None) => {}
          Err(error) => break Err(error),
        },
      }
    };
    let owners_returned = observation.load(Ordering::SeqCst);
    drop(pool);
    answer.try_send((result, owners_returned)).unwrap();
  })
  .unwrap();
  let (result, owners_returned) = result.recv().unwrap();
  // slates: the runtime's shutdown joins its shards; the retirement owners, which upstream's joined too, are the
  // reaper's (local change 1).
  let owner_result = rt.shutdown().and(reaper.shutdown());
  drop(issuer);
  assert!(
    refused.load(Ordering::SeqCst) > 0,
    "the real worker file never refused a write"
  );
  assert_eq!(
    owners_returned, 1,
    "native worker ownership remains at terminal reply"
  );
  assert_eq!(returned.load(Ordering::SeqCst), 1);
  assert!(
    matches!(result, Err(Error::Io { detail, .. }) if detail.contains(WITNESS)),
    "the later join failure replaced the first physical failure"
  );
  assert!(matches!(owner_result, Err(RtError::BadConfig { .. })));
}
