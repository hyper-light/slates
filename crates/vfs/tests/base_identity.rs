//! An untouched base inode is bound to the disk file it was made from (A-151; §4.5, D-25; AC-3.4 across a takeover,
//! A-61). When the disk gives the name to another file, the name becomes a new entry: the old inode leaves it, open or
//! not, and serves the file it was made from only through a descriptor it holds, else it is refused `StaleHandle`. A
//! file is told by its device, its inode number and its birth time, since a filesystem hands a freed number to the next
//! file. Driven over the simulated host, whose `close_files` stands for a daemon's death: every descriptor it held is
//! gone, as the successor's are (`docs/bugs/2026-10-22-a-base-file-opened-before-a-takeover-read-the-file-an-outsider-put-at-its-name.md`).

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::unwrap_in_result
)]

mod common;

use slates_vfs::base::BaseConfig;
use slates_vfs::clock::StepClock;
use slates_vfs::delta::VolumeRecord;
use slates_vfs::error::VfsError;
use slates_vfs::host::HostFs;
use slates_vfs::host::sim::SimHost;
use slates_vfs::ids::{InodeNo, RefOwner};
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::recover::VolumeImage;
use slates_vfs::volume::{Store, Volume, VolumeConfig};
use slates_wire::Wire;

/// Shape: the large-file class boundary, one chunk window, as the base plane's other tests use.
const LARGE: u64 = 65_536;
/// Shape: the journal a volume keeps, in bytes, as the recovery tests use.
const JOURNAL_BYTES: usize = 1 << 20;
/// Shape: the recorded attachment the tests' kernel references belong to, so a recovery gives them back (A-61).
const KERNEL: u64 = 41;
/// Shape: how far the simulated clock moves between the tests' steps: past the base filesystem's one-nanosecond
/// granularity, so each outsider's file is born at its own time and every listing is read after the change it shows.
const STEP_NS: i64 = 10;

fn overlay(host: &mut SimHost, store: &mut Store) -> Volume {
  let root = host.root();
  let facts = host.facts(root).unwrap();
  Volume::create_overlay(
    store,
    VolumeConfig {
      prefix: 7,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: JOURNAL_BYTES,
      clock: Box::new(StepClock::new(1_000_000, 1_000)),
    },
    BaseConfig {
      root,
      facts,
      large_class_bytes: LARGE,
    },
  )
  .unwrap()
}

/// The inode at `path`, looked up as a transport's lookup does (the directory relisted when the disk changed).
fn lookup(vol: &mut Volume, host: &mut SimHost, store: &mut Store, path: &str) -> InodeNo {
  vol.with_host(host).resolve(store, path).unwrap().inode
}

/// Every byte of inode `no`, read by its number as a transport reads through a handle.
fn read_no(
  vol: &mut Volume,
  host: &mut SimHost,
  store: &mut Store,
  no: InodeNo,
) -> Result<Vec<u8>, VfsError> {
  let mut o = vol.with_host(host);
  let size = o.stat(store, no)?.size;
  let mut buf = vec![0u8; usize::try_from(size).unwrap()];
  let n = o.read(store, no, 0, &mut buf)?;
  buf.truncate(n);
  Ok(buf)
}

/// Opens inode `no` as a mount's `OPEN` does: the kernel's lookup reference, the base descriptor, the open pin.
fn open(vol: &mut Volume, host: &mut SimHost, store: &mut Store, no: InodeNo) {
  vol
    .reference_for(store, no, RefOwner::Attachment(KERNEL))
    .unwrap();
  vol.with_host(host).open_base(store, no).unwrap();
  vol.open_for(no, RefOwner::Attachment(KERNEL));
}

/// The publication the daemon's barrier writes for `vol` now (the streamed encoders), decoded as its successor reads it.
fn published(vol: &mut Volume, store: &Store, host: &mut SimHost) -> VolumeImage {
  let mut streamed = Vec::new();
  vol
    .encode_publication(store, Some(host), &mut streamed)
    .unwrap();
  let VolumeRecord::Full { image } = VolumeRecord::decode(&mut streamed.as_slice()).unwrap() else {
    panic!("a volume's first publication is its whole image");
  };
  image
}

/// The daemon dies and its successor recovers `image`: every descriptor the dead daemon held is closed, the volume is
/// rebuilt over the same disk, and the kernel's recorded references are given back to it.
fn taken_over(store: &Store, image: &VolumeImage, host: &mut SimHost) -> (Store, Volume) {
  host.close_files();
  let mut after = common::surviving(store);
  let claims = common::claims(&mut after, &[image]);
  let root = host.root();
  let mut recovered = Volume::from_image(
    &mut after,
    image,
    &claims,
    Box::new(StepClock::new(2_000_000, 1_000)),
    JOURNAL_BYTES,
    Some((host, root)),
  )
  .unwrap();
  recovered
    .settle_recovered_references(&mut after, |id| id == KERNEL)
    .unwrap();
  (after, recovered)
}

/// A-151 (§4.5): a base file no handle holds open, replaced on the disk by an outsider, is a new entry. Do: look a base
/// file up, take the kernel's reference on it and read it (its descriptor is kept), close nothing more, then let an
/// outsider rename another file over its name and look the name up again. Expect a new inode at the name reading the
/// outsider's file, and the old number, still referenced, refused `StaleHandle`: the file it was made from is gone from
/// every name. Before A-151 the old inode was refreshed in place onto the outsider's file and kept its number.
#[test]
fn a_closed_base_file_an_outsider_replaced_is_a_new_inode_and_its_old_number_refuses() {
  let mut host = SimHost::new();
  host.replace_file("/notes", b"made from this file");
  let mut store = common::store();
  let mut vol = overlay(&mut host, &mut store);
  let no = lookup(&mut vol, &mut host, &mut store, "/notes");
  vol
    .reference_for(&store, no, RefOwner::Attachment(KERNEL))
    .unwrap();
  assert_eq!(
    read_no(&mut vol, &mut host, &mut store, no).unwrap(),
    b"made from this file"
  );
  host.advance_ns(STEP_NS);
  host.replace_file("/notes", b"the outsider's file");
  host.advance_ns(STEP_NS);
  let renamed = lookup(&mut vol, &mut host, &mut store, "/notes");
  assert_ne!(
    renamed, no,
    "the name the disk gave another file is a new inode"
  );
  assert_eq!(
    read_no(&mut vol, &mut host, &mut store, renamed).unwrap(),
    b"the outsider's file"
  );
  assert_eq!(
    read_no(&mut vol, &mut host, &mut store, no),
    Err(VfsError::StaleHandle),
    "the old number, closed and still referenced, refuses: its file is gone"
  );
}

/// A-151 with AC-3.4 (A-61): a handle opened before a takeover reads its file after it, while the name still names that
/// file. Do: open a base file and read it, publish, let the daemon die (its descriptors closed) and recover, then read
/// through the old handle's number. Expect the bytes it opened: the successor opens the name again and finds the very
/// file the inode was made from. This holds only if the image carries the file's identity, so it is the round trip's
/// check.
#[test]
fn a_handle_opened_before_a_takeover_reads_its_file_while_the_name_still_names_it() {
  let mut host = SimHost::new();
  host.replace_file("/notes", b"opened before the takeover");
  let mut store = common::store();
  let mut vol = overlay(&mut host, &mut store);
  let no = lookup(&mut vol, &mut host, &mut store, "/notes");
  open(&mut vol, &mut host, &mut store, no);
  assert_eq!(
    read_no(&mut vol, &mut host, &mut store, no).unwrap(),
    b"opened before the takeover"
  );
  let image = published(&mut vol, &store, &mut host);
  let (mut after, mut recovered) = taken_over(&store, &image, &mut host);
  host.advance_ns(STEP_NS);
  assert_eq!(
    read_no(&mut recovered, &mut host, &mut after, no).unwrap(),
    b"opened before the takeover",
    "the successor serves the old handle the file it opened"
  );
  assert_eq!(
    lookup(&mut recovered, &mut host, &mut after, "/notes"),
    no,
    "an unchanged file keeps its number across the takeover"
  );
}

/// A-151 with AC-3.4 (A-61), the box's red test in the volume core: a handle opened before a takeover never reads the
/// file an outsider put at its name after it. Do: open a base file and read it, publish, let the daemon die and
/// recover, rename another file over the name on the disk, look the name up, then read through the old handle's
/// number. Expect `StaleHandle`: the file it opened died with the daemon's descriptor, and the name serves the
/// outsider's file as a new inode. Before A-151 the successor refreshed the inode in place and the old handle read the
/// outsider's bytes.
#[test]
fn a_handle_opened_before_a_takeover_is_refused_once_an_outsider_replaces_its_file() {
  let mut host = SimHost::new();
  host.replace_file("/notes", b"opened before the takeover");
  let mut store = common::store();
  let mut vol = overlay(&mut host, &mut store);
  let no = lookup(&mut vol, &mut host, &mut store, "/notes");
  open(&mut vol, &mut host, &mut store, no);
  assert_eq!(
    read_no(&mut vol, &mut host, &mut store, no).unwrap(),
    b"opened before the takeover"
  );
  let image = published(&mut vol, &store, &mut host);
  let (mut after, mut recovered) = taken_over(&store, &image, &mut host);
  host.advance_ns(STEP_NS);
  host.replace_file("/notes", b"an outsider put this file here");
  host.advance_ns(STEP_NS);
  let renamed = lookup(&mut recovered, &mut host, &mut after, "/notes");
  assert_ne!(
    renamed, no,
    "the name serves the outsider's file as a new inode"
  );
  assert_eq!(
    read_no(&mut recovered, &mut host, &mut after, renamed).unwrap(),
    b"an outsider put this file here"
  );
  assert_eq!(
    read_no(&mut recovered, &mut host, &mut after, no),
    Err(VfsError::StaleHandle),
    "the old handle is refused, never served the outsider's file"
  );
}

/// A-151: a file is told by its birth time as well as its number, since ext4 and XFS hand a freed inode number to the
/// next file. Do: open a base file, publish, let the daemon die and recover; then save over the name twice, as an
/// editor's save-by-rename does, the second save's file taking the number the first freed (the opened file's); read
/// through the old handle's number. Expect `StaleHandle`, never the second save's bytes: the number is the opened
/// file's, the birth time is not.
#[test]
fn a_number_the_filesystem_reused_never_passes_for_the_file_a_recovered_handle_opened() {
  let mut host = SimHost::new();
  host.replace_file("/notes", b"opened before the takeover");
  let opened_number = host.fingerprint("/notes").unwrap().ino;
  let mut store = common::store();
  let mut vol = overlay(&mut host, &mut store);
  let no = lookup(&mut vol, &mut host, &mut store, "/notes");
  open(&mut vol, &mut host, &mut store, no);
  let image = published(&mut vol, &store, &mut host);
  let (mut after, mut recovered) = taken_over(&store, &image, &mut host);
  host.advance_ns(STEP_NS);
  host.replace_file("/notes", b"the first save");
  host.advance_ns(STEP_NS);
  assert!(
    host.replace_file_with_number(
      "/notes",
      b"the second save, at the opened file's number",
      opened_number
    ),
    "the opened file's number is free once the daemon that held it died"
  );
  host.advance_ns(STEP_NS);
  assert_eq!(
    host.fingerprint("/notes").unwrap().ino,
    opened_number,
    "the name holds the opened file's number again"
  );
  assert_eq!(
    read_no(&mut recovered, &mut host, &mut after, no),
    Err(VfsError::StaleHandle),
    "a reused number never passes for the file the handle opened"
  );
}

/// A-151: an open is never bound to a file the inode was not made from. Do: look a base file up, let an outsider
/// rename another file over its name before the open, then open the inode the lookup gave. Expect `StaleHandle` (the
/// kernel then looks the name up again), and that lookup serving the outsider's file as a new inode. Before A-151 the
/// open took a descriptor on the outsider's file under the first file's inode and attributes.
#[test]
fn an_open_racing_a_replacement_is_refused_never_bound_to_the_new_file() {
  let mut host = SimHost::new();
  host.replace_file("/notes", b"listed at the lookup");
  let mut store = common::store();
  let mut vol = overlay(&mut host, &mut store);
  let no = lookup(&mut vol, &mut host, &mut store, "/notes");
  host.advance_ns(STEP_NS);
  host.replace_file("/notes", b"replaced between the lookup and the open");
  host.advance_ns(STEP_NS);
  assert_eq!(
    vol.with_host(&mut host).open_base(&mut store, no),
    Err(VfsError::StaleHandle),
    "the open is refused, never bound to the outsider's file"
  );
  let renamed = lookup(&mut vol, &mut host, &mut store, "/notes");
  assert_ne!(renamed, no);
  vol
    .with_host(&mut host)
    .open_base(&mut store, renamed)
    .unwrap();
  assert_eq!(
    read_no(&mut vol, &mut host, &mut store, renamed).unwrap(),
    b"replaced between the lookup and the open"
  );
}

/// A-151 on a filesystem that records no birth time (some older and network filesystems): a recovered volume cannot tell
/// a reused inode number from the file it numbered, so it proves nothing it cannot. Do: over such a disk, look two base
/// files up and open one, publish, let the daemon die and recover; read through the open handle's number, look both
/// names up, and open and read the second. Expect the old handle refused `StaleHandle`, the first listing after the
/// takeover giving both entries new numbers (the bytes unchanged), and a file the successor numbered itself opened and
/// served as on any disk.
#[test]
fn without_birth_times_a_recovered_volume_renumbers_its_untouched_entries_and_refuses_old_handles()
{
  let mut host = SimHost::new().without_birth_times();
  host.replace_file("/notes", b"opened before the takeover");
  host.replace_file("/other", b"first opened after the takeover");
  let mut store = common::store();
  let mut vol = overlay(&mut host, &mut store);
  let no = lookup(&mut vol, &mut host, &mut store, "/notes");
  let other = lookup(&mut vol, &mut host, &mut store, "/other");
  open(&mut vol, &mut host, &mut store, no);
  let image = published(&mut vol, &store, &mut host);
  let (mut after, mut recovered) = taken_over(&store, &image, &mut host);
  host.advance_ns(STEP_NS);
  assert_eq!(
    read_no(&mut recovered, &mut host, &mut after, no),
    Err(VfsError::StaleHandle),
    "a handle opened before the takeover cannot be proven its file without a birth time"
  );
  let renamed = lookup(&mut recovered, &mut host, &mut after, "/notes");
  assert_ne!(renamed, no, "the unprovable entry has a new number");
  assert_eq!(
    read_no(&mut recovered, &mut host, &mut after, renamed).unwrap(),
    b"opened before the takeover"
  );
  let renumbered = lookup(&mut recovered, &mut host, &mut after, "/other");
  assert_ne!(
    renumbered, other,
    "every untouched entry the recovery restored is renumbered once"
  );
  recovered
    .with_host(&mut host)
    .open_base(&mut after, renumbered)
    .unwrap();
  assert_eq!(
    read_no(&mut recovered, &mut host, &mut after, renumbered).unwrap(),
    b"first opened after the takeover"
  );
}
