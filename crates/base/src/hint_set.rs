//! The shard's set of base watchers, awaited as one descriptor (§4.5, §4.15; the event-driven reaper, A-709): each
//! overlay's watcher descriptor (an inotify instance on Linux, a kqueue on macOS) is entered once in one epoll or kqueue
//! of the shard's, keyed by a word the caller chooses (the server's volume handle, index and generation), and the shard
//! awaits that one descriptor's readability through its driver. A ready answer names the keys whose watchers hold
//! hints.
//!
//! **One shot a key.** Each entry fires once and is then out of the set (`EPOLLONESHOT`, `EV_ONESHOT`) until the
//! caller arms it again ([`HintSet::arm`]), which it does once it drained that watcher. A watcher the caller cannot drain
//! now (its host lent to a landing) therefore never makes the set ready again in a loop, and a watcher with hints left
//! after one drain is reported again as soon as it is armed, both kernels evaluating the entry's readiness when it is
//! entered.
//!
//! **A closed watcher leaves the set with its descriptor.** Closing the last descriptor of an inotify instance removes
//! its epoll entry, and closing a kqueue removes its knotes; so a destroyed volume's watcher leaves nothing behind, and
//! a key the set answers whose volume is gone names a stale handle, which the caller refuses as one. Until A-709 the
//! reap loop's tick drained every overlay's watcher once a liveness window, ready or not; awaiting each watcher's own
//! descriptor through the shard's driver was rejected on 2026-10-08 because a destroy closes the descriptor under a
//! waiting task and the number may be reused: here no task waits on a watcher's number, only on the set's.

#[cfg(not(target_os = "linux"))]
use std::os::fd::AsRawFd;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

/// The shard's set of base watchers (the module doc).
#[derive(Debug)]
pub struct HintSet {
  queue: OwnedFd,
}

impl AsFd for HintSet {
  fn as_fd(&self) -> BorrowedFd<'_> {
    self.queue.as_fd()
  }
}

#[cfg(target_os = "linux")]
impl HintSet {
  /// An empty set: an epoll instance, closed on exec.
  pub fn open() -> std::io::Result<HintSet> {
    let queue = rustix::event::epoll::create(rustix::event::epoll::CreateFlags::CLOEXEC)?;
    Ok(HintSet { queue })
  }

  /// Enters `watcher` under `key` for one report of its readability, or arms its entry again after one.
  pub fn arm(&self, watcher: BorrowedFd<'_>, key: u64) -> std::io::Result<()> {
    use rustix::event::epoll::{self, EventData, EventFlags};
    let flags = EventFlags::IN | EventFlags::ONESHOT;
    match epoll::add(&self.queue, watcher, EventData::new_u64(key), flags) {
      // An entry that fired stays in the interest list, disarmed: it is armed again in place.
      Err(rustix::io::Errno::EXIST) => {
        epoll::modify(&self.queue, watcher, EventData::new_u64(key), flags)?;
        Ok(())
      }
      other => Ok(other?),
    }
  }

  /// The keys whose watchers are ready now, at most `room` of them; never blocks. Each is out of the set until armed
  /// again.
  pub fn ready(&self, room: usize) -> std::io::Result<Vec<u64>> {
    let mut events: Vec<rustix::event::epoll::Event> = Vec::with_capacity(room);
    let zero = rustix::time::Timespec {
      tv_sec: 0,
      tv_nsec: 0,
    };
    rustix::event::epoll::wait(
      &self.queue,
      rustix::buffer::spare_capacity(&mut events),
      Some(&zero),
    )?;
    Ok(events.iter().map(|event| event.data.u64()).collect())
  }
}

#[cfg(not(target_os = "linux"))]
impl HintSet {
  /// An empty set: a kqueue (never inherited by a child).
  pub fn open() -> std::io::Result<HintSet> {
    Ok(HintSet {
      queue: rustix::event::kqueue::kqueue()?,
    })
  }

  /// Enters `watcher` under `key` for one report of its readability, or arms its entry again after one (`EV_ADD` of a
  /// knote `EV_ONESHOT` removed once it fired).
  pub fn arm(&self, watcher: BorrowedFd<'_>, key: u64) -> std::io::Result<()> {
    use rustix::event::kqueue::{Event, EventFilter, EventFlags};
    let udata = std::ptr::without_provenance_mut(
      usize::try_from(key).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?,
    );
    let change = Event::new(
      EventFilter::Read(watcher.as_raw_fd()),
      EventFlags::ADD | EventFlags::ONESHOT,
      udata,
    );
    let mut none: [Event; 0] = [];
    kevent_now(&self.queue, &[change], &mut none)?;
    Ok(())
  }

  /// The keys whose watchers are ready now, at most `room` of them; never blocks. Each is out of the set until armed
  /// again.
  pub fn ready(&self, room: usize) -> std::io::Result<Vec<u64>> {
    let mut events = Vec::with_capacity(room);
    kevent_now(
      &self.queue,
      &[],
      rustix::buffer::spare_capacity(&mut events),
    )?;
    Ok(
      events
        .iter()
        .filter_map(|event| u64::try_from(event.udata().addr()).ok())
        .collect(),
    )
  }
}

/// One `kevent` call on `queue` that never blocks: `changes` submitted, then pending events written into `out` as far as
/// it has room: none for a registration's empty array, up to a vector's spare capacity for a drain
/// (`rustix::buffer::spare_capacity`, which refuses a vector with none). A bare `&mut Vec` hands rustix 1.x the
/// vector's *length* as the list's size, so a drain through one reported nothing
/// (docs/bugs/2026-09-30-the-kqueue-base-watcher-drained-into-a-zero-length-list.md). Shared by the macOS directory
/// watcher's registrations and drains (`crate::unix`) and the set's.
#[cfg(not(target_os = "linux"))]
pub(crate) fn kevent_now<Out: rustix::buffer::Buffer<rustix::event::kqueue::Event>>(
  queue: &OwnedFd,
  changes: &[rustix::event::kqueue::Event],
  out: Out,
) -> std::io::Result<Out::Output> {
  // SAFETY: every change names a descriptor its caller holds open across the call (a watched directory's or file's, or
  // a watcher's queue), and a descriptor closed later leaves the queue with it; the kernel writes at most `out`'s room;
  // a zero timeout never blocks.
  let written =
    unsafe { rustix::event::kqueue::kevent(queue, changes, out, Some(std::time::Duration::ZERO)) }?;
  Ok(written)
}

#[cfg(test)]
mod tests {
  use std::os::fd::AsFd;

  use super::HintSet;

  /// Shape: the room each test reads the set with: more than the keys it enters.
  const ROOM: usize = 8;
  /// Shape: the key the test enters its readable source under.
  const READY_KEY: u64 = (7 << 32) | 3;
  /// Shape: the key of the source nothing is written to.
  const QUIET_KEY: u64 = (9 << 32) | 1;

  /// A-709. Do: enter two pipes' read ends in a set, write to one; read the set, read it again, arm the written one again.
  /// Expect: the written one's key alone, once; nothing on the second read (one shot); its key again once armed, its byte
  /// still unread.
  #[test]
  fn a_ready_watcher_is_named_by_its_key_once_until_armed_again() {
    let set = HintSet::open().expect("the set");
    let (ready_read, ready_write) = rustix::pipe::pipe().expect("a pipe");
    let (quiet_read, _quiet_write) = rustix::pipe::pipe().expect("another");
    set.arm(ready_read.as_fd(), READY_KEY).expect("entered");
    set.arm(quiet_read.as_fd(), QUIET_KEY).expect("entered");
    assert_eq!(
      set.ready(ROOM).expect("read"),
      Vec::<u64>::new(),
      "nothing ready yet"
    );
    rustix::io::write(&ready_write, b"h").expect("written");
    assert_eq!(
      set.ready(ROOM).expect("read"),
      vec![READY_KEY],
      "the written one, by its key"
    );
    assert_eq!(
      set.ready(ROOM).expect("read"),
      Vec::<u64>::new(),
      "one shot: out until armed"
    );
    set.arm(ready_read.as_fd(), READY_KEY).expect("armed again");
    assert_eq!(
      set.ready(ROOM).expect("read"),
      vec![READY_KEY],
      "still readable: named again"
    );
  }

  /// A-709. Do: enter a pipe's read end, write to it, then close the read end; read the set. Expect: nothing: a closed
  /// watcher leaves the set with its descriptor.
  #[test]
  fn a_closed_watcher_leaves_the_set() {
    let set = HintSet::open().expect("the set");
    let (read, write) = rustix::pipe::pipe().expect("a pipe");
    set.arm(read.as_fd(), READY_KEY).expect("entered");
    rustix::io::write(&write, b"h").expect("written");
    drop(read);
    assert_eq!(set.ready(ROOM).expect("read"), Vec::<u64>::new());
  }
}
