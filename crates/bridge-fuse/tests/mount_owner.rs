//! AC-3.12, R10: kernel permission checks see the provisioning user's ownership even
//! when a developer runs the test process as root. Exercise the same fixture as the mount.

#![allow(clippy::unwrap_used)]

use slates_bridge_core::{Attachments, Bridge, ObjectId, Rights, View, VolumeBridge};
use slates_db::catalog::{Principal, VolumeId};

mod common;

/// Do: provision for a non-root user and read the root attributes through the bridge.
/// Expect: the kernel selects the writable owner permission class for that user.
#[test]
fn a_mounted_fixture_reports_its_provisioning_owner() {
  let uid = 1234;
  let gid = 2345;
  let mut store = common::store();
  let mut volume = common::volume_for_owner(&mut store, uid, gid);
  let id = VolumeId { bytes: [7; 16] };
  let mut attachments = Attachments::new();
  let attachment = attachments
    .attach(
      id,
      View::Current,
      Principal::Uid { uid },
      Rights {
        read: true,
        write: true,
      },
    )
    .unwrap();
  let cx = attachments.begin(attachment).unwrap();
  let mut bridge = VolumeBridge::new(id, &mut volume, &mut store);
  let root = bridge.root(&cx).unwrap();
  let attr = bridge.getattr(ObjectId::new(root, 0), &cx).unwrap();
  assert_eq!(
    (attr.uid, attr.gid),
    (uid, gid),
    "ownership reported to the kernel"
  );
  assert_eq!(
    attr.mode & 0o300,
    0o300,
    "the mounting owner can create names"
  );
}
