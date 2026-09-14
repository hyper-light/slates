//! The OCI form's pure parts driven by use on every host (§4.6 A-9; AC-4.11 / T-4.13's refusal
//! leg): a simulated mount table in the shape each kernel reports — the macOS `getfsstat` records
//! and the Linux `mountinfo` text — verified against a volume's expected export, every refusal of the
//! closed taxonomy reached, and the runtime-specification entry built with the attachment's policy.
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_bridge_oci::binding::{DestinationRefusal, OciMountEntry};
use slates_bridge_oci::mount_table::{MountEntry, MountTableError, parse_mountinfo};
use slates_bridge_oci::verify::{
  HostMountKind, HostPathRefusal, expected_mount, verify_host_mount,
};

fn entry(mount_point: &str, fstype: &str, source: &str) -> MountEntry {
  MountEntry {
    mount_point: mount_point.to_owned(),
    fstype: fstype.to_owned(),
    source: source.to_owned(),
  }
}

/// A macOS table as `getfsstat` reports it: the APFS root and data volumes, the developer image, and
/// a slates loopback mount of the volume `work` at a temp directory.
fn macos_table() -> Vec<MountEntry> {
  vec![
    entry("/", "apfs", "/dev/disk3s1s1"),
    entry("/System/Volumes/Data", "apfs", "/dev/disk3s5"),
    entry("/dev", "devfs", "devfs"),
    entry(
      "/private/var/folders/1s/T/tmp.abc",
      "nfs",
      "localhost:/work",
    ),
    entry("/Users/me/other", "nfs", "localhost:/other"),
  ]
}

/// The loopback mount of the volume is verified by name: the export names the volume, so the evidence
/// says so; trailing separators do not matter.
#[test]
fn a_loopback_mount_of_the_volume_is_verified_and_names_it() {
  let expected = expected_mount(HostMountKind::NfsLoopback, "work");
  let verified = verify_host_mount(
    &macos_table(),
    "/private/var/folders/1s/T/tmp.abc/",
    &expected,
  )
  .unwrap();
  assert_eq!(verified.mount_point, "/private/var/folders/1s/T/tmp.abc");
  assert_eq!(verified.fstype, "nfs");
  assert_eq!(verified.source, "localhost:/work");
  assert!(verified.names_volume);
}

/// Every refusal, in the order the checks consult the least: a relative path never reaches the
/// table; a directory inside a mount is not the attachment; the root is another filesystem; another
/// volume's export is not this one.
#[test]
fn every_host_path_refusal_is_typed_and_reached() {
  let table = macos_table();
  let expected = expected_mount(HostMountKind::NfsLoopback, "work");
  assert_eq!(
    verify_host_mount(&table, "relative/path", &expected),
    Err(HostPathRefusal::NotAbsolute)
  );
  assert_eq!(
    verify_host_mount(&[], "relative/path", &expected),
    Err(HostPathRefusal::NotAbsolute),
    "nothing consulted"
  );
  assert_eq!(
    verify_host_mount(&table, "/private/var/folders/1s/T/tmp.abc/sub", &expected),
    Err(HostPathRefusal::NotAMountPoint)
  );
  assert_eq!(
    verify_host_mount(&table, "/", &expected),
    Err(HostPathRefusal::ForeignFilesystem {
      fstype: "apfs".to_owned()
    })
  );
  assert_eq!(
    verify_host_mount(&table, "/Users/me/other", &expected),
    Err(HostPathRefusal::NotThisVolume {
      source: "localhost:/other".to_owned()
    })
  );
}

/// A later mount at the same path shadows an earlier one: the visible mount is the one verified.
#[test]
fn the_last_mount_at_a_path_is_the_visible_one() {
  let mut table = macos_table();
  table.push(entry("/Users/me/other", "nfs", "localhost:/work"));
  let expected = expected_mount(HostMountKind::NfsLoopback, "work");
  let verified = verify_host_mount(&table, "/Users/me/other", &expected).unwrap();
  assert_eq!(verified.source, "localhost:/work");
}

/// Format: a Linux `mountinfo` text (proc(5)) with an escaped space in a mount point, a slates FUSE
/// mount, and an overlay root — the shapes a CI runner and a container show.
const MOUNTINFO: &str = "\
22 27 0:21 / /proc rw,nosuid,nodev,noexec,relatime shared:5 - proc proc rw
27 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw,errors=remount-ro
90 27 0:45 / /home/runner/mnt\\040point rw,nosuid,nodev,relatime shared:60 - fuse.slates slates rw,user_id=1001,group_id=1001,default_permissions
95 27 0:50 / /var/lib/docker/overlay2/x/merged rw,relatime - overlay overlay rw,lowerdir=/a,upperdir=/b,workdir=/c
";

/// The Linux table parses with its escapes undone, and a slates FUSE mount is verified by its
/// filesystem type alone — the source does not name the volume, and the evidence says so.
#[test]
fn a_fuse_mount_is_verified_by_type_and_the_evidence_says_the_source_does_not_name_the_volume() {
  let table = parse_mountinfo(MOUNTINFO).unwrap();
  assert_eq!(table.len(), 4);
  assert_eq!(table[2].mount_point, "/home/runner/mnt point");
  assert_eq!(table[2].fstype, "fuse.slates");
  assert_eq!(table[2].source, "slates");
  let expected = expected_mount(HostMountKind::Fuse, "work");
  let verified = verify_host_mount(&table, "/home/runner/mnt point", &expected).unwrap();
  assert!(!verified.names_volume);
  assert_eq!(
    verify_host_mount(&table, "/", &expected),
    Err(HostPathRefusal::ForeignFilesystem {
      fstype: "ext4".to_owned()
    })
  );
}

/// A hostile or truncated table is refused at its line, never misread: a line with no separator, and
/// one with nothing after it.
#[test]
fn a_malformed_mountinfo_is_refused_at_its_line() {
  assert_eq!(
    parse_mountinfo("22 27 0:21 / /proc rw shared:5 proc proc rw\n"),
    Err(MountTableError::Malformed { line: 1 })
  );
  assert_eq!(
    parse_mountinfo("27 1 8:1 / / rw shared:1 - ext4 /dev/sda1 rw\n90 27 0:45 / /m rw -\n"),
    Err(MountTableError::Malformed { line: 2 })
  );
  assert_eq!(parse_mountinfo("\n\n").unwrap(), Vec::<MountEntry>::new());
}

/// The runtime entry: a recursive bind, read-only for a read attachment and read-write otherwise, at
/// an absolute destination; a relative destination is refused.
#[test]
fn the_runtime_entry_carries_the_attachment_policy() {
  let expected = expected_mount(HostMountKind::NfsLoopback, "work");
  let verified = verify_host_mount(
    &macos_table(),
    "/private/var/folders/1s/T/tmp.abc",
    &expected,
  )
  .unwrap();
  let read_only = OciMountEntry::new(&verified, "/work", true).unwrap();
  assert_eq!(read_only.source, "/private/var/folders/1s/T/tmp.abc");
  assert_eq!(read_only.destination, "/work");
  assert_eq!(read_only.mount_type(), "bind");
  assert_eq!(
    read_only.options(),
    vec!["rbind".to_owned(), "ro".to_owned()]
  );
  let read_write = OciMountEntry::new(&verified, "/work", false).unwrap();
  assert_eq!(
    read_write.options(),
    vec!["rbind".to_owned(), "rw".to_owned()]
  );
  assert_eq!(
    OciMountEntry::new(&verified, "work", false),
    Err(DestinationRefusal::NotAbsolute)
  );
}

/// The real table of this host is readable where a query is built, and lists the root filesystem at
/// `/` (the kernel's word, not a fixture); elsewhere the query is refused typed.
#[test]
fn the_real_table_lists_the_root_where_a_query_is_built() {
  match slates_bridge_oci::mount_table::mount_table() {
    Ok(table) => {
      assert!(
        table.iter().any(|e| e.mount_point == "/"),
        "the root is mounted: {table:?}"
      );
      assert!(table.iter().all(|e| !e.fstype.is_empty()));
    }
    Err(MountTableError::Unsupported { platform }) => {
      assert!(
        platform != "macos" && platform != "linux",
        "a query is built for {platform}, so it must not be refused as unsupported"
      );
    }
    Err(other) => panic!("the table could not be read: {other}"),
  }
}
