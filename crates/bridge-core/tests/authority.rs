//! The attachment authority (§4.8/§4.13, the inode-addressed-io design): a context is built only
//! from a validated attachment record, and a revoked, drained or epoch-fenced attachment is
//! refused — the checks are real (`f = 0`), never unconditional success. Every host, no mount.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use slates_bridge_core::{Attachments, Rights, View};
use slates_db::catalog::{Principal, VolumeId};

/// A live attachment yields a context built from its record; a revoked one admits no effect, and a
/// drained one is gone entirely.
#[test]
fn a_context_is_built_from_the_record_and_a_revoked_attachment_is_refused() {
  let mut attachments = Attachments::new();
  let volume = VolumeId { bytes: [1; 16] };
  let subject = Principal::Uid {
    host: slates_db::HostId(0),
    uid: 501,
  };
  let id = attachments
    .attach(
      volume,
      View::Current,
      subject.clone(),
      Rights {
        read: true,
        write: true,
      },
    )
    .unwrap();

  let cx = attachments.context(id).unwrap();
  assert_eq!(
    cx.volume, volume,
    "the context binds the attachment's volume"
  );
  assert_eq!(cx.subject, subject, "and its enrolled subject");
  assert_eq!(
    cx.rights,
    Rights {
      read: true,
      write: true
    },
    "and its granted rights"
  );
  assert!(matches!(cx.view, View::Current));

  // Revoked: no new effect, but the record survives for an in-flight drain.
  attachments.revoke(id);
  assert!(
    attachments.context(id).is_err(),
    "a revoked attachment admits no new effect"
  );
  // Drained: the slot is freed, the handle stale.
  attachments.drain(id);
  assert!(
    attachments.context(id).is_err(),
    "a drained attachment is gone"
  );
}

/// A takeover raises the owner epoch; an attachment admitted under the old epoch is fenced.
/// The owner a teardown sweeps an ending attachment's references under (the server's FUSE mount end, 2026-11-09):
/// answered for a live attachment and for a revoked one, whose references are still its own though it admits no
/// request, and for no drained one, so a sweep after the drain finds no owner and gives nothing back twice. Do: attach;
/// read the owner; revoke; read it again; drain; read it again. Expect the context's owner, the same owner, then none.
#[test]
fn an_attachments_owner_answers_while_live_or_revoked_and_none_once_drained() {
  let mut attachments = Attachments::new();
  let id = attachments
    .attach(
      VolumeId { bytes: [2; 16] },
      View::Current,
      Principal::Uid {
        host: slates_db::HostId(0),
        uid: 501,
      },
      Rights {
        read: true,
        write: false,
      },
    )
    .unwrap();
  let live = attachments.context(id).unwrap().owner;
  assert_eq!(
    attachments.owner(id),
    Some(live),
    "a live attachment's owner is its context's"
  );
  attachments.revoke(id);
  assert_eq!(
    attachments.owner(id),
    Some(live),
    "a revoked attachment's references are still its own"
  );
  attachments.drain(id);
  assert_eq!(
    attachments.owner(id),
    None,
    "a drained attachment has no owner"
  );
}

#[test]
fn a_takeover_fences_attachments_under_the_old_epoch() {
  let mut attachments = Attachments::new();
  let id = attachments
    .attach(
      VolumeId { bytes: [2; 16] },
      View::Current,
      Principal::Uid {
        host: slates_db::HostId(0),
        uid: 0,
      },
      Rights {
        read: true,
        write: false,
      },
    )
    .unwrap();
  assert!(
    attachments.context(id).is_ok(),
    "admitted under the current epoch"
  );

  attachments.take_over();
  assert!(
    attachments.context(id).is_err(),
    "fenced under the old epoch after a takeover"
  );
}
