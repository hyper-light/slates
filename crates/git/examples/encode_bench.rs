//! The pack fragments' encoder over a real pack, one encode a process (A-718; main's ruling of 2026-11-20 on the
//! encoder's level: the A/B of a level timed per object against one chosen from the machine's measured rates, CPU per
//! encoded MiB and bytes out; BENCHMARKS.md, "git: the fragments' encoder").
//!
//! Reads a pack on standard input, pushes its objects into a fresh repository as one version whose head is the commit
//! named by the first argument (hex), then encodes that version's fragments once, as A-718's encoding does a turn at a
//! time, driven here to its end. Prints one line: the encode's wall nanoseconds (one thread, nothing else running in the
//! process), the input's bytes (every object's content), the fragments' bytes, and nanoseconds per MiB of input.
//!
//! removes them.

use std::io::Read;
use std::time::Instant;

use slates_git::Admission;
use slates_git::object_name::{ObjectFormat, ObjectId};
use slates_git::store::learn::{Clock, Exchange};
use slates_git::store::{Entries, Incoming, Push, Repository};
use slates_git::unpack::{NoIndex, Object, read_whole};
use slates_vcs::ContentStore;

/// Shape: the content store's budget: room for git/git cut at v2.0.0 inflated (about 0.3 GB) many times over.
const BUDGET: u64 = 16 << 30;
/// Shape: the content store's window, the macOS arm64 page-derived one (16 x 16 KiB).
const WINDOW: usize = 256 << 10;
/// Shape: the bytes copied to measure the exchange's memcpy rate, as the daemon prices a byte at neutral.
const MEMCPY_PROBE: usize = 256 << 20;
/// Format: nanoseconds in a second.
const NANOS_PER_SECOND: u64 = 1_000_000_000;
/// Format: bytes in a MiB.
const MIB: u128 = 1 << 20;

/// The machine's monotonic clock.
struct Wall(Instant);

impl Clock for Wall {
  fn now_ns(&mut self) -> u64 {
    u64::try_from(self.0.elapsed().as_nanos()).unwrap_or(u64::MAX)
  }
}

/// The pack's objects, handed in in pack order with the chain depth each was stored at.
struct Objects<'a> {
  objects: &'a [Object],
  at: usize,
}

impl Entries for Objects<'_> {
  type Refused = std::convert::Infallible;

  fn next_entry(&mut self) -> Option<Result<Incoming<'_>, Self::Refused>> {
    let object = self.objects.get(self.at)?;
    self.at += 1;
    Some(Ok(Incoming {
      kind: object.kind,
      claimed: Some(object.id),
      content: &object.content,
      depth: object.chain.map_or(0, |chain| chain.depth),
    }))
  }
}

/// Bytes a second this machine copies, over one copy of [`MEMCPY_PROBE`] bytes.
fn memcpy_rate() -> u64 {
  let source = vec![1_u8; MEMCPY_PROBE];
  let mut target = vec![0_u8; MEMCPY_PROBE];
  let start = Instant::now();
  target.copy_from_slice(&source);
  std::hint::black_box(&target);
  let ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX).max(1);
  u64::try_from(MEMCPY_PROBE).unwrap_or(u64::MAX).saturating_mul(NANOS_PER_SECOND) / ns
}

fn main() {
  let head = std::env::args().nth(1).expect("the head commit's hex");
  let head = ObjectId::from_hex(ObjectFormat::Sha1, head.as_bytes()).expect("a SHA-1 hex id");
  let mut pack = Vec::new();
  std::io::stdin().read_to_end(&mut pack).expect("the pack on standard input");
  let unpacked = read_whole(&pack, ObjectFormat::Sha1, &NoIndex, &mut Admission::new(u64::MAX)).expect("a whole pack");
  drop(pack);
  let input: u64 = unpacked.objects.iter().map(|object| object.content.len() as u64).sum();
  let mut store = ContentStore::new(BUDGET, WINDOW).expect("a content store");
  let mut repository = Repository::new(ObjectFormat::Sha1);
  let mut push = Push::new(ObjectFormat::Sha1);
  let naming = repository.naming();
  push
    .admit_all(&mut store, &naming, &mut Objects { objects: &unpacked.objects, at: 0 })
    .expect("every object admitted");
  drop(unpacked);
  repository.seal(&store, &mut push).expect("sealed");
  let moved = repository.admit(&store, &push, 1, Some(head)).expect("main admitted");
  repository.commit(&mut store, &mut push, &[moved]).expect("committed");
  let exchange = Exchange {
    ns: NANOS_PER_SECOND,
    bytes: memcpy_rate(),
  };
  let start = Instant::now();
  repository
    .encode_fragments(&mut store, (1, 0), &mut Wall(Instant::now()), exchange)
    .expect("encoded");
  let ns = start.elapsed().as_nanos();
  let out = repository.fragments_len(1, 0).expect("the fragments kept");
  let per_mib = ns.saturating_mul(MIB) / u128::from(input.max(1));
  println!("encode_ns={ns} input_bytes={input} fragments_bytes={out} ns_per_input_mib={per_mib}");
}
