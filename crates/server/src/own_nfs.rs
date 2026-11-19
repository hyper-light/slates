//! The macOS NFS mounts this daemon serves, recorded as its own (§4.6, A-227): no daemon thread opens a path on a
//! volume the daemon serves, and on macOS the mount table's source (`slates:/<volume>`) names no daemon, so the daemon
//! keeps the record itself.
//!
//! A mount is recorded when the CLI binds it (`BindMount`, after `mount(2)` returned) and when a successor learns
//! the mounts its predecessor's catalog still names as its shard starts, before any base or landing is served
//! (`learn`, from `daemon::init_shard`, under the process's `slates_base::own_mounts::Learning`). Either way the
//! kernel's table (`getfsstat(MNT_NOWAIT)`, no RPC, nothing opened) names the mount at exactly the bound point (the CLI
//! binds the path the kernel records, `std::fs::canonicalize`) whose source is the volume's, and its filesystem id is
//! what the base plane and the landing walk compare (`slates_base::own_mounts`). A record ends with its attachment
//! (`verbs::end_attachment`); a mount the table no longer names was unmounted while no daemon ran, and nothing is
//! recorded for it.
//!
//! **The kernel's unmount ends the attachment (A-511).** A shard that holds an own mount watches the kernel's unmounts
//! ([`watch`]): the kernel's own event (`slates_bridge_oci::mount_table::UnmountWatch`, `EVFILT_FS` on `VQ_UNMOUNT`)
//! wakes the shard through its readiness wait (A-149), the table is read once, and every own mount whose id the table
//! no longer names has its attachment ended, as a `detach` would end it. Nothing in the watch is timed, so it never
//! gives up on a mount and never takes a hint for an unmount: a mount the kernel lets go long after any `UMNT`, or with
//! none at all, ends its attachment at the event, and a mount that stays keeps it. The NFS `UMNT` ends nothing. It is
//! a hint any local process can send, and the kernel sends none at a forced unmount (`umount -f`, the CLI's busy
//! unmount) or for an NFSv4 mount (Apple's NFS client, `kext/nfs_vfsops.c`, `nfs_mount_zombie`, read 2026-11-18).
//! Until A-511 a `UMNT` started a timed confirmation against the table (A-34, A-493), so a mount unmounted with no
//! `UMNT`, or more slowly than the confirmation's bound, kept its attachment for good.

use std::collections::BTreeSet;

use crate::state::{self, ShardState};
use slates_base::own_mounts::Unrecorded;
use slates_bridge_oci::mount_table::{MountTableError, UnmountWatch};
use slates_db::catalog::{AttachmentRecord, Consumer};
use slates_ipc::protocol::Refusal;
use slates_rt::futures;

/// The refusal of a mount at `mount_point` this daemon could not record, typed (A-299); the point and why are logged.
fn refusal(why: Unrecorded, mount_point: &str) -> Refusal {
  slates_machine::log_line!("slates-server: the NFS mount at {mount_point} {why}");
  crate::error::target(crate::error::target_of_unrecorded(why))
}

/// Format: the source a macOS slates mount names in the kernel's table, `slates:/<volume name>`
/// (`crates/cli/src/mount.rs`, `mount_volume`).
fn source_of(volume_name: &str) -> String {
  format!("slates:/{volume_name}")
}

/// One NFS mount of this daemon's, recorded as its own while this value lives. Its drop forgets the mount, so each
/// mount has exactly one: a second for a mount already held is never made ([`held`]).
#[derive(Debug)]
pub(crate) struct OwnNfsMount(u64);

impl OwnNfsMount {
  /// Records mount `id` and holds it.
  fn record(id: u64) -> Result<Self, Unrecorded> {
    slates_base::own_mounts::record(id).map_err(Unrecorded::Full)?;
    Ok(Self(id))
  }
}

/// The id of the mount of the volume named `volume_name` at exactly `mount_point` in the kernel's table, or `None`.
fn id_at(mount_point: &str, volume_name: &str) -> Option<u64> {
  let source = source_of(volume_name);
  slates_base::own_mounts::mount_id_at(mount_point, |named| named == source)
}

/// Whether this shard already holds mount `id`.
fn held(state: &ShardState, id: u64) -> bool {
  state.own_nfs_mounts.values().any(|own| own.0 == id)
}

impl Drop for OwnNfsMount {
  fn drop(&mut self) {
    slates_base::own_mounts::forget(self.0);
  }
}

/// The host mount `record` names and the volume's name, when it is one.
fn mount_of<'a>(state: &'a ShardState, record: &'a AttachmentRecord) -> Option<(&'a str, &'a str)> {
  if !matches!(record.consumer, Consumer::Bridge) {
    return None;
  }
  let point = record.form.mount_point()?;
  let volume = state.db.partition().volume(record.volume)?;
  Some((point, volume.name.as_str()))
}

/// Counter: binds whose point the kernel's table names no mount of the volume at, so nothing was recorded (A-227). The
/// CLI binds the point the kernel records once `mount(2)` has returned, so a count here is a bind of a point with no
/// mount (the server's NFS mount tests make one on purpose) or a point the CLI and the kernel name differently.
pub(crate) const BIND_UNCONFIRMED: &str = "own_nfs.bind_unconfirmed";

/// Records the mount the CLI just bound for `record` at `mount_point`, before the bind is committed. The record mirrors
/// the kernel's table: a point the table names no mount of the volume at holds nothing this daemon serves, so nothing is
/// recorded and the bind is counted [`BIND_UNCONFIRMED`]. `None` then, and when this shard holds the mount already (a
/// bind made again, whose holder stays); refused at the bound of mounts this daemon records.
pub(crate) fn bound(
  state: &mut ShardState,
  record: &AttachmentRecord,
  mount_point: &str,
) -> Result<Option<OwnNfsMount>, Refusal> {
  let volume = state
    .db
    .partition()
    .volume(record.volume)
    .ok_or(Refusal::NotFound)?;
  let Some(id) = id_at(mount_point, &volume.name) else {
    state.count(BIND_UNCONFIRMED, 1);
    return Ok(None);
  };
  if held(state, id) {
    return Ok(None);
  }
  OwnNfsMount::record(id)
    .map(Some)
    .map_err(|why| refusal(why, mount_point))
}

/// Learns the mounts this shard's catalog names that the kernel's table still holds, a predecessor's that outlived
/// it (A-227): each recorded before the shard serves any base or landing. A record the table does not hold is a mount
/// that went while no daemon ran, and nothing is recorded for it. Refused at the set's bound, which is the most
/// attachments the shards hold, so a start that meets it is refused rather than serve a mount it does not record.
pub(crate) fn learn(state: &mut ShardState) -> Result<(), Refusal> {
  let mut learned = Vec::new();
  for volume in state.db.partition().volumes() {
    for record in state.db.partition().attachments_of(volume.id) {
      let Some((point, name)) = mount_of(state, record) else {
        continue;
      };
      // Two records naming one point find one mount, which has one holder.
      let Some(id) = id_at(point, name) else {
        continue;
      };
      if held(state, id)
        || learned
          .iter()
          .any(|(_, own): &(u64, OwnNfsMount)| own.0 == id)
      {
        continue;
      }
      learned.push((
        record.id,
        OwnNfsMount::record(id).map_err(|why| refusal(why, point))?,
      ));
    }
  }
  state.own_nfs_mounts.extend(learned);
  Ok(())
}

/// Counter: the kernel's table read to judge this shard's own mounts ([`let_go_of_unmounted`], A-511): once as the
/// shard's watch starts and once after each event the kernel posts. The watch's non-vacuity counter.
const JUDGED: &str = "own_nfs.unmount_judged";

/// Counter: attachments ended because the kernel let their mount go (A-511).
const UNMOUNTED: &str = "own_nfs.unmounted";

/// Counter: judgments that ended nothing because the kernel's table could not be read (A-511): nothing ends on a read
/// error.
const TABLE_UNREAD: &str = "own_nfs.table_unread";

/// Counter: mounts the kernel let go whose attachment's end could not be recorded (A-511). The mount stays recorded as
/// this shard's own, so the next event judges it again.
const END_REFUSED: &str = "own_nfs.unmount_end_refused";

/// Counter: unmount watches this shard could not start or keep (A-511): the kernel refused the queue or its filter, or
/// the runtime refused the task or its wait. The shard's own mounts are judged again when a later bind starts a watch.
const WATCH_REFUSED: &str = "own_nfs.unmount_watch_refused";

/// Where the unmount watch learns that the kernel's mount table changed (A-511): the kernel's own event
/// ([`UnmountWatch`]) or a test's stand-in. The shard's readiness wait registers the descriptor (A-149), and a take
/// empties what was posted, so the descriptor reads ready again only at the next change.
trait TableEvents {
  /// The descriptor that reads ready once a change was posted.
  fn descriptor(&self) -> i32;
  /// Takes what was posted, without waiting.
  fn take(&mut self) -> Result<(), MountTableError>;
}

impl TableEvents for UnmountWatch {
  fn descriptor(&self) -> i32 {
    UnmountWatch::descriptor(self)
  }

  fn take(&mut self) -> Result<(), MountTableError> {
    UnmountWatch::take(self)
  }
}

/// The ids of the mounts the kernel's table names now, as own mounts are recorded
/// (`slates_base::own_mounts::table_id`), read once: `getfsstat(MNT_NOWAIT)` asks no filesystem.
fn table_ids() -> Result<BTreeSet<u64>, MountTableError> {
  slates_bridge_oci::mount_table::mount_table().map(|table| {
    table
      .iter()
      .map(slates_base::own_mounts::table_id)
      .collect()
  })
}

/// Starts this shard's watch of the kernel's unmounts (A-511) when it holds an own mount and no watch runs: from a bind
/// that recorded one (`verbs::bind_mount`) and as the shard starts over the mounts it learned ([`learn`]). The kernel's
/// queue is opened here, so a refusal is counted at once ([`WATCH_REFUSED`]) and the next bind tries again.
pub(crate) fn watch(state: &mut ShardState) {
  watch_with(state, UnmountWatch::open, table_ids);
}

/// [`watch`] over the events `open` makes and the table `table` reads: the kernel's, or a test's stand-ins.
fn watch_with<E: TableEvents + 'static>(
  state: &mut ShardState,
  open: impl FnOnce() -> Result<E, MountTableError>,
  table: impl Fn() -> Result<BTreeSet<u64>, MountTableError> + 'static,
) {
  if state.unmount_watch || state.own_nfs_mounts.is_empty() {
    return;
  }
  let Ok(events) = open() else {
    state.count(WATCH_REFUSED, 1);
    return;
  };
  // Detached: the watch ends itself once the shard holds no own mount, and a shutdown drops it with the shard's tasks,
  // which closes its queue.
  match futures::spawn(watch_unmounts(events, table)).and_then(futures::detach) {
    Ok(()) => state.unmount_watch = true,
    Err(_) => state.count(WATCH_REFUSED, 1),
  }
}

/// The shard's watch of the kernel's unmounts (A-511): it judges the shard's own mounts ([`let_go_of_unmounted`]) as it
/// starts and after every event the kernel posts, and ends once the shard holds no own mount. The first judgment comes
/// before the first wait because the kernel's filter sees only events after it was attached (XNU's `filt_fsattach`), so
/// a mount that left between its bind and this start is found by that first read. A wait the runtime refuses, or a
/// take the kernel refuses, ends the watch counted ([`WATCH_REFUSED`]); a later bind starts another.
async fn watch_unmounts(
  mut events: impl TableEvents,
  table: impl Fn() -> Result<BTreeSet<u64>, MountTableError>,
) {
  loop {
    let watching = state::with_state_counted(|s| {
      let_go_of_unmounted(s, &table);
      if s.own_nfs_mounts.is_empty() {
        s.unmount_watch = false;
      }
      s.unmount_watch
    });
    if watching != Some(true) {
      return;
    }
    let waited = slates_rt::readiness::readable(events.descriptor()).await;
    if waited.is_err() || events.take().is_err() {
      let _ = state::with_state_counted(|s| {
        s.unmount_watch = false;
        s.count(WATCH_REFUSED, 1);
      });
      return;
    }
  }
}

/// Ends the attachment of each own mount of this shard the kernel's table no longer names (A-511): the kernel let that
/// mount go, so the attachment ends as a `detach` would (`verbs::end_attachment`, `Ending::Otherwise`, since the mount
/// is gone already), its own-mount record with it. The table is read once, and only while the shard holds an own mount;
/// a table that cannot be read ends nothing ([`TABLE_UNREAD`]). How many attachments ended.
fn let_go_of_unmounted(
  state: &mut ShardState,
  table: &impl Fn() -> Result<BTreeSet<u64>, MountTableError>,
) -> usize {
  if state.own_nfs_mounts.is_empty() {
    return 0;
  }
  state.count(JUDGED, 1);
  let Ok(named) = table() else {
    state.count(TABLE_UNREAD, 1);
    return 0;
  };
  let gone = unmounted(
    state
      .own_nfs_mounts
      .iter()
      .map(|(attachment, own)| (*attachment, own.0)),
    &named,
  );
  let mut ended: usize = 0;
  for attachment in gone {
    let Some(record) = state.db.partition().attachment(attachment).cloned() else {
      // The record ended without letting its mount go: only the record of ours is left to end.
      state.own_nfs_mounts.remove(&attachment);
      continue;
    };
    if crate::verbs::end_attachment(state, &record, crate::verbs::Ending::Otherwise).is_ok() {
      ended = ended.saturating_add(1);
    } else {
      state.count(END_REFUSED, 1);
    }
  }
  state.count(UNMOUNTED, u64::try_from(ended).unwrap_or(u64::MAX));
  ended
}

/// The attachments of `own` (each with its mount's id) whose mount `named`, the ids the kernel's table names now, does
/// not hold: the mounts the kernel let go. A mount is judged by its id, never by its point, so a volume mounted again
/// at the same point (a new mount with a new id) does not keep the old mount's attachment. Pure.
fn unmounted(own: impl Iterator<Item = (u64, u64)>, named: &BTreeSet<u64>) -> Vec<u64> {
  own
    .filter(|(_, id)| !named.contains(id))
    .map(|(attachment, _)| attachment)
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::os::fd::{AsRawFd, OwnedFd};
  use std::sync::atomic::{AtomicBool, Ordering};
  use std::time::{Duration, Instant};

  use slates_db::catalog::{AttachForm, Principal};
  use slates_ipc::protocol::{NamePolicy, ReplyBody, RequestBody, SizeClass};

  /// Shape: the uid of the tests' volume owner.
  const OWNER_UID: u32 = 4511;
  /// Shape: the tests' mount point; nothing is read there (the kernel's table is stood in for).
  const POINT: &str = "/Volumes/unmount-watch-test";
  /// Shape: how long the test thread waits for the watch to judge an event it posted: four liveness windows, many times
  /// the one shard turn a judgment takes.
  const JUDGE_WAIT: Duration = Duration::from_nanos(4 * crate::daemon::LIVENESS_BUDGET_NS);
  /// Shape: the test thread's pause between two looks at the shard.
  const LOOK_PAUSE: Duration = Duration::from_millis(2);

  /// A stand-in for the kernel's unmount events: a pipe whose write end the test holds. A byte written is an event
  /// posted, and a take reads every byte waiting.
  struct Posted(OwnedFd);

  impl TableEvents for Posted {
    fn descriptor(&self) -> i32 {
      self.0.as_raw_fd()
    }

    fn take(&mut self) -> Result<(), MountTableError> {
      let mut bytes = [0u8; 16];
      loop {
        match rustix::io::read(&self.0, &mut bytes) {
          Ok(0) | Err(rustix::io::Errno::AGAIN) => return Ok(()),
          Ok(_) => {}
          Err(e) => {
            return Err(MountTableError::Query {
              errno: e.raw_os_error(),
            });
          }
        }
      }
    }
  }

  /// A volume named `name` on this shard and a host mount's attachment of it bound at [`POINT`], recorded as this
  /// shard's own mount `id`, as a bind of a mount the kernel's table named records it: the attachment.
  fn own_mount(s: &mut ShardState, name: &str, id: u64) -> u64 {
    let owner = Principal::Uid {
      host: slates_db::HostId(0),
      uid: OWNER_UID,
    };
    let created = crate::verbs::dispatch(
      s,
      1,
      &owner,
      RequestBody::Create {
        name: name.to_owned(),
        size: SizeClass::Bounded { limit: 1 << 20 },
        names: NamePolicy::Exact,
        require_locked: false,
        base: None,
      },
    );
    assert!(
      matches!(created, ReplyBody::Created { .. }),
      "{name}: {created:?}"
    );
    let volume = s.db.partition().volume_by_name(name).cloned().unwrap();
    let attachment = crate::verbs::next_attachment_id(s);
    let now = slates_vfs::clock::Clock::monotonic_ns(&mut s.clock);
    let op = slates_db::Op::AttachmentAdded {
      record: AttachmentRecord {
        id: attachment,
        volume: volume.id,
        consumer: Consumer::Bridge,
        snapshot: None,
        form: AttachForm::ChosenPath {
          path: POINT.to_owned(),
        },
        principal: volume.owner.clone(),
        rights: crate::verbs::rights_of(&volume, &volume.owner),
        token: [0; 16],
      },
    };
    s.db.mutate(&mut s.segment, &op, now).unwrap();
    s.own_nfs_mounts
      .insert(attachment, OwnNfsMount::record(id).unwrap());
    attachment
  }

  /// Starts the shard's watch over a stand-in table that names mount `id` while `mounted` holds: the pipe end that
  /// posts its events.
  fn start_watch(s: &mut ShardState, id: u64, mounted: &'static AtomicBool) -> OwnedFd {
    let (read, write) = rustix::pipe::pipe().unwrap();
    rustix::io::ioctl_fionbio(&read, true).unwrap();
    watch_with(
      s,
      move || Ok(Posted(read)),
      move || {
        Ok(if mounted.load(Ordering::SeqCst) {
          BTreeSet::from([id])
        } else {
          BTreeSet::new()
        })
      },
    );
    write
  }

  /// Posts one stand-in event.
  fn post(events: &OwnedFd) {
    assert_eq!(rustix::io::write(events, &[1u8]).unwrap(), 1);
  }

  /// Waits, at most [`JUDGE_WAIT`], for the shard's watch to have judged `times` times: the count reached.
  fn judged(daemon: &crate::daemon::Daemon, times: u64) -> u64 {
    let deadline = Instant::now() + JUDGE_WAIT;
    loop {
      let now =
        crate::daemon::observe_first(daemon, |s| s.refusals.get(JUDGED).copied().unwrap_or(0));
      if now >= times || Instant::now() >= deadline {
        return now;
      }
      std::thread::sleep(LOOK_PAUSE);
    }
  }

  /// What the shard holds of `attachment` and its watch: whether the attachment and its own-mount record live, whether
  /// the watch runs, and how many attachments it ended.
  fn held(daemon: &crate::daemon::Daemon, attachment: u64) -> (bool, bool, bool, u64) {
    crate::daemon::observe_first(daemon, move |s| {
      (
        s.db.partition().attachment(attachment).is_some(),
        s.own_nfs_mounts.contains_key(&attachment),
        s.unmount_watch,
        s.refusals.get(UNMOUNTED).copied().unwrap_or(0),
      )
    })
  }

  /// A-511 (§4.6 A-34): an unmount the kernel performs ends its attachment at the kernel's own event, with no `UMNT`
  /// (a forced unmount sends none, nor does an NFSv4 mount). Do: hold an own mount, start the watch over a stand-in
  /// table that names it, post an event while the table still names it, then take it out of the table and post
  /// another. Expect the attachment kept after the first event and ended after the second, its own-mount record and
  /// the watch ended with it (the shard holds no own mount), and each event judged once. Before, only a `UMNT` started
  /// a confirmation, so an unmount that sent none left its attachment for good.
  #[test]
  fn an_unmount_with_no_umnt_ends_its_attachment_at_the_kernels_event() {
    static MOUNTED: AtomicBool = AtomicBool::new(true);
    const ID: u64 = u64::MAX - 1;
    let daemon = crate::daemon::audit_daemon();
    let (attachment, events) = crate::daemon::observe_first(&daemon, |s| {
      let attachment = own_mount(s, "forced-unmount", ID);
      (attachment, start_watch(s, ID, &MOUNTED))
    });
    let started = judged(&daemon, 1);
    let watching = held(&daemon, attachment);
    post(&events);
    let after_kept = judged(&daemon, 2);
    let kept = held(&daemon, attachment);
    MOUNTED.store(false, Ordering::SeqCst);
    post(&events);
    let after_unmount = judged(&daemon, 3);
    let ended = held(&daemon, attachment);
    daemon.stop();
    println!(
      "judged {started}, {after_kept}, {after_unmount}; watching {watching:?}, kept {kept:?}, ended {ended:?}"
    );
    assert_eq!(
      (started, after_kept, after_unmount),
      (1, 2, 3),
      "the watch judged as it started and once per event"
    );
    assert_eq!(
      watching,
      (true, true, true, 0),
      "the watch runs over a live mount"
    );
    assert_eq!(
      kept,
      (true, true, true, 0),
      "an event while the mount is named ends nothing"
    );
    assert_eq!(
      ended,
      (false, false, false, 1),
      "the kernel's unmount ended the attachment, its record of ours and the watch"
    );
  }

  /// A-511: nothing in the watch is timed, so a mount that stays keeps its attachment however long it stays, and the
  /// kernel's later unmount still ends it. Do: hold an own mount, start the watch, post an event while the mount is
  /// named, let one liveness window pass (the first bound a `UMNT`'s confirmation had, A-493), then take the mount out
  /// of the table and post. Expect the attachment kept through the window and ended at the late event. Before, a
  /// confirmation that ran out kept the attachment for good, though the kernel let the mount go later.
  #[test]
  fn a_mount_the_kernel_lets_go_after_any_wait_still_ends_its_attachment() {
    static MOUNTED: AtomicBool = AtomicBool::new(true);
    const ID: u64 = u64::MAX - 2;
    let daemon = crate::daemon::audit_daemon();
    let (attachment, events) = crate::daemon::observe_first(&daemon, |s| {
      let attachment = own_mount(s, "late-unmount", ID);
      (attachment, start_watch(s, ID, &MOUNTED))
    });
    let _ = judged(&daemon, 1);
    post(&events);
    let _ = judged(&daemon, 2);
    std::thread::sleep(Duration::from_nanos(crate::daemon::LIVENESS_BUDGET_NS));
    let kept = held(&daemon, attachment);
    MOUNTED.store(false, Ordering::SeqCst);
    post(&events);
    let late = judged(&daemon, 3);
    let ended = held(&daemon, attachment);
    daemon.stop();
    println!("kept {kept:?} through a window; judged {late}, ended {ended:?}");
    assert_eq!(
      kept,
      (true, true, true, 0),
      "a mount that stays keeps its attachment past any bound"
    );
    assert_eq!(
      ended,
      (false, false, false, 1),
      "the kernel's late unmount ended the attachment"
    );
  }

  /// A-511: the kernel's filter sees only events after it is attached (XNU's `filt_fsattach`), so the watch reads the
  /// table once as it starts. Do: hold an own mount the stand-in table no longer names, and start the watch with no
  /// event ever posted. Expect the attachment ended by the start's own read, and the watch ended with it. Before, a
  /// mount that left with no `UMNT` reaching the daemon kept its attachment.
  #[test]
  fn a_mount_that_left_before_its_watch_started_ends_at_the_watchs_first_read() {
    static MOUNTED: AtomicBool = AtomicBool::new(false);
    const ID: u64 = u64::MAX - 3;
    let daemon = crate::daemon::audit_daemon();
    let (attachment, _events) = crate::daemon::observe_first(&daemon, |s| {
      let attachment = own_mount(s, "left-early", ID);
      (attachment, start_watch(s, ID, &MOUNTED))
    });
    let first = judged(&daemon, 1);
    let ended = held(&daemon, attachment);
    daemon.stop();
    println!("judged {first}, ended {ended:?}");
    assert_eq!(first, 1, "the start read the table once");
    assert_eq!(
      ended,
      (false, false, false, 1),
      "a mount gone before the watch started ended its attachment at the first read"
    );
  }

  /// A-511: a mount is judged by its id, never its point. Do: judge four own mounts against a table that names two of
  /// them and a mount the shard does not hold. Expect exactly the two unnamed ones let go; a volume mounted again at
  /// the same point is a new id, so its old mount's attachment is among them.
  #[test]
  fn only_the_mounts_the_table_no_longer_names_are_let_go() {
    let own = [(10, 100), (11, 101), (12, 102), (13, 103)];
    let named = BTreeSet::from([101, 103, 999]);
    assert_eq!(unmounted(own.into_iter(), &named), vec![10, 12]);
    assert_eq!(
      unmounted(own.into_iter(), &BTreeSet::new()),
      vec![10, 11, 12, 13],
      "an empty table holds none of them"
    );
    assert!(
      unmounted(std::iter::empty(), &named).is_empty(),
      "a shard with no own mount lets nothing go"
    );
  }

  /// A-511: nothing ends on a read error. Do: judge an own mount against a table that cannot be read. Expect the
  /// attachment and its record kept, and the unread table counted.
  #[test]
  fn a_table_that_cannot_be_read_ends_nothing() {
    const ID: u64 = u64::MAX - 4;
    let (ended, kept, own, unread) = crate::daemon::audit_on_shard(|s| {
      let attachment = own_mount(s, "unread-table", ID);
      let unreadable =
        || -> Result<BTreeSet<u64>, MountTableError> { Err(MountTableError::Query { errno: 5 }) };
      let ended = let_go_of_unmounted(s, &unreadable);
      (
        ended,
        s.db.partition().attachment(attachment).is_some(),
        s.own_nfs_mounts.contains_key(&attachment),
        s.refusals.get(TABLE_UNREAD).copied().unwrap_or(0),
      )
    });
    assert_eq!(
      (ended, kept, own, unread),
      (0, true, true, 1),
      "an unreadable table ended nothing and was counted"
    );
  }
}
