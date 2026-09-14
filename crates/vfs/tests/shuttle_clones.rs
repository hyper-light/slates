//! T-1.6 in its shuttle form (§4.5 clones; Part 6 "Concurrency", nightly): two simulated agents
//! interleave clone-and-write on the same base through the store's owner, the way clients reach a
//! shard (one owner per store, D-7; a request is a move over a bounded channel, the reply a move
//! back). shuttle draws the thread schedule and each agent's randomness, so every schedule is
//! replayable from its seed. Under every schedule each clone's view is its own — its bytes and its
//! names, even where both agents write the same offsets and create the same names — and the base
//! (the snapshot and the origin head) is unchanged.
//!
//! The oracle is the one `tests/clones.rs` keeps for the one-thread form (each clone's expected bytes
//! and names; the snapshot and the origin still the base); this file changes only who interleaves:
//! real threads under shuttle's random scheduler instead of a generated schedule on one thread. An
//! agent's clone may be taken after the other agent has written, so the clone must see the base and
//! never the other's bytes.

#![cfg(shuttle)]
// Test harness code: an unwrap or a panic here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{clone_config, store, volume};
use shuttle::rand::{Rng, thread_rng};
use shuttle::scheduler::RandomScheduler;
use shuttle::sync::mpsc::{Receiver, SyncSender, sync_channel};
use shuttle::{Config, Runner};
use slates_vfs::ids::InodeNo;
use slates_vfs::volume::{Store, Volume};

/// Shape: agents — two, the design's words (T-1.6).
const AGENTS: usize = 2;
/// Shape: the most steps an agent takes after its clone (a write and a create each): half of the
/// forty-step schedules `tests/clones.rs` draws over both clones.
const MAX_STEPS: usize = 20;
/// Derived: the schedule budget — the case count `tests/clones.rs` runs (200), so the two forms of
/// T-1.6 explore comparable budgets; measured 2026-09-13 (`docs/wip/concurrency.md`).
const SCHEDULES: usize = 200;
/// Shape: the fixed seed of the schedule budget, so a run is reproducible; changed only with the
/// record.
const SEED: u64 = 0xc10e_5eed;
/// Derived: the stack each simulated thread runs on, 2 MiB — what Rust gives a spawned test thread
/// (`RUST_MIN_STACK`'s default), so the owner runs the volume core on the stack it has under
/// `cargo test`.
const STACK_BYTES: usize = 2 << 20;
/// Shape: the request channel's bound — one outstanding request per agent, since an agent waits for
/// each reply before its next request (the client's request/reply discipline).
const REQUEST_BOUND: usize = AGENTS;
/// Format: the base bytes both clones start from (as in `tests/clones.rs`).
const BASE: &[u8] = b"base bytes of the shared snapshot";
/// Format: the byte each agent writes, distinct per agent so a leak between clones is visible.
const TAGS: [u8; AGENTS] = *b"AB";
/// Shape: the clones' volume prefixes, distinct from the origin's (7 in the fixtures).
const CLONE_PREFIXES: [u16; AGENTS] = [8, 9];
/// Shape: the origin's quota, as `tests/clones.rs` uses (16 MiB).
const QUOTA: u64 = 1 << 24;

/// A request to the store's owner.
enum Request {
  /// Take the agent's clone of the shared snapshot.
  Clone { agent: usize },
  /// Write one byte at `offset` in the agent's clone.
  Write { agent: usize, offset: u64, byte: u8 },
  /// Create a file named `name` in the agent's clone's root.
  Create { agent: usize, name: String },
  /// The agent's clone as it reads it: the file's bytes and the root's names.
  View { agent: usize },
  /// The agent has finished.
  Done,
}

/// The owner's reply.
enum Reply {
  Done,
  View { bytes: Vec<u8>, names: Vec<String> },
}

struct Envelope {
  request: Request,
  reply: SyncSender<Reply>,
}

fn names(vol: &Volume, store: &Store) -> Vec<String> {
  let mut names: Vec<String> = vol
    .readdir(store, vol.root())
    .unwrap()
    .iter()
    .map(|r| r.name.to_string())
    .collect();
  names.sort();
  names
}

fn read_all(vol: &Volume, store: &Store, file: InodeNo) -> Vec<u8> {
  let mut buf = vec![0u8; 64];
  let n = vol.read(store, file, 0, &mut buf).unwrap();
  buf.truncate(n);
  buf
}

/// An agent's line to the owner: one request out, its reply back (the client's discipline).
struct Mailbox {
  requests: SyncSender<Envelope>,
  reply: SyncSender<Reply>,
  replies: Receiver<Reply>,
}

impl Mailbox {
  fn new(requests: SyncSender<Envelope>) -> Mailbox {
    let (reply, replies) = sync_channel::<Reply>(1);
    Mailbox {
      requests,
      reply,
      replies,
    }
  }

  fn ask(&self, request: Request) -> Reply {
    self
      .requests
      .send(Envelope {
        request,
        reply: self.reply.clone(),
      })
      .unwrap();
    self.replies.recv().unwrap()
  }

  /// Asks, expecting a bare acknowledgement.
  fn ask_done(&self, request: Request) {
    assert!(matches!(self.ask(request), Reply::Done));
  }
}

/// The agent's record of its own clone (the oracle of `tests/clones.rs`): the file's bytes after
/// its writes and the root's names after its creates, sorted.
struct Expected {
  bytes: Vec<u8>,
  names: Vec<String>,
}

/// `steps` write-and-create steps at the agent's own offsets and names, recorded as the agent
/// expects to read them back.
fn write_and_create(mailbox: &Mailbox, agent: usize, tag: u8, steps: usize) -> Expected {
  let mut bytes = BASE.to_vec();
  let mut names = vec!["f".to_owned()];
  for step in 0..steps {
    let offset = u64::try_from(step).unwrap();
    mailbox.ask_done(Request::Write {
      agent,
      offset,
      byte: tag,
    });
    if bytes.len() <= step {
      bytes.resize(step + 1, 0);
    }
    bytes[step] = tag;
    let name = format!("c{step}");
    mailbox.ask_done(Request::Create {
      agent,
      name: name.clone(),
    });
    names.push(name);
  }
  names.sort();
  Expected { bytes, names }
}

/// Reads the clone back through the owner and checks it against the agent's own record.
fn check_view(mailbox: &Mailbox, agent: usize, expected: &Expected) {
  match mailbox.ask(Request::View { agent }) {
    Reply::View { bytes, names } => {
      assert_eq!(
        bytes, expected.bytes,
        "agent {agent}'s clone holds its own bytes"
      );
      assert_eq!(
        names, expected.names,
        "agent {agent}'s clone lists its own names"
      );
    }
    Reply::Done => panic!("a view was asked for"),
  }
}

/// An agent: clone, then a random number of write-and-create steps at its own offsets and names,
/// then read the clone back and check it against its own record.
fn agent(agent: usize, requests: SyncSender<Envelope>) {
  let mailbox = Mailbox::new(requests);
  let mut rng = thread_rng();
  let steps = rng.gen_range(1..=MAX_STEPS);
  mailbox.ask_done(Request::Clone { agent });
  let expected = write_and_create(&mailbox, agent, TAGS[agent], steps);
  check_view(&mailbox, agent, &expected);
  mailbox.ask_done(Request::Done);
}

/// T-1.6: under every schedule shuttle draws, each clone's view is independent and the base is
/// unchanged.
#[test]
fn two_agents_interleaving_clone_and_write_under_random_schedules_stay_independent() {
  let mut config = Config::new();
  config.stack_size = STACK_BYTES;
  let schedules = Runner::new(RandomScheduler::new_from_seed(SEED, SCHEDULES), config).run(|| {
    let mut store = store();
    let mut origin = volume(&mut store, QUOTA);
    let root = origin.root();
    let file = origin.create_file(&mut store, root, "f", 0o644).unwrap();
    origin.write(&mut store, file, 0, BASE).unwrap();
    let snapshot = origin.snapshot(&mut store).unwrap();
    let mut clones: Vec<Option<Volume>> = (0..AGENTS).map(|_| None).collect();

    let (requests, inbox): (SyncSender<Envelope>, Receiver<Envelope>) = sync_channel(REQUEST_BOUND);
    let agents: Vec<_> = (0..AGENTS)
      .map(|index| {
        let requests = requests.clone();
        shuttle::thread::spawn(move || agent(index, requests))
      })
      .collect();
    drop(requests);

    // The owner: every request in arrival order on the one store.
    let mut done = 0;
    while done < AGENTS {
      let Envelope { request, reply } = inbox.recv().unwrap();
      let answer = match request {
        Request::Clone { agent } => {
          let config = clone_config(CLONE_PREFIXES[agent]);
          clones[agent] = Some(Volume::clone_of(&store, &mut origin, snapshot, config).unwrap());
          Reply::Done
        }
        Request::Write {
          agent,
          offset,
          byte,
        } => {
          let clone = clones[agent].as_mut().unwrap();
          clone.write(&mut store, file, offset, &[byte]).unwrap();
          Reply::Done
        }
        Request::Create { agent, name } => {
          let clone = clones[agent].as_mut().unwrap();
          let croot = clone.root();
          clone.create_file(&mut store, croot, &name, 0o644).unwrap();
          Reply::Done
        }
        Request::View { agent } => {
          let clone = clones[agent].as_ref().unwrap();
          Reply::View {
            bytes: read_all(clone, &store, file),
            names: names(clone, &store),
          }
        }
        Request::Done => {
          done += 1;
          Reply::Done
        }
      };
      reply.send(answer).unwrap();
    }
    for handle in agents {
      handle.join().unwrap();
    }

    // The base is unchanged: the origin head and the snapshot read the base bytes and list only
    // the base file.
    assert_eq!(read_all(&origin, &store, file), BASE, "the origin head");
    let mut buf = vec![0u8; 64];
    let n = origin.read_in(&store, snapshot, file, 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], BASE, "the snapshot");
    assert_eq!(names(&origin, &store), vec!["f".to_owned()]);
  });
  eprintln!("shuttle: T-1.6: {schedules} schedules (seed {SEED:#x})");
  assert_eq!(schedules, SCHEDULES, "the whole budget ran");
}
