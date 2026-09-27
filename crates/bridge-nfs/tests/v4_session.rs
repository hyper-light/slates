//! NFSv4.1 sessions (RFC 8881 §2.10, §18.35–18.37, §18.46, §18.50–18.51) driven as a client drives
//! them: a client id, a session, requests on its slots, retries, a reboot, the bounds and the lease.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use slates_bridge_nfs::v4::Nfsstat4;
use slates_bridge_nfs::v4::session::{
  CreateSession, EXCHGID4_FLAG_CONFIRMED_R, ExchangeId, Limits, Sequence, Sequenced, Sessions,
};
use slates_bridge_nfs::v4::types::ChannelAttrs;

/// Shape: a small offer the tests can reach the bounds of.
fn limits() -> Limits {
  Limits {
    max_clients: 2,
    max_sessions_per_client: 1,
    offer: ChannelAttrs {
      header_pad: 0,
      max_request: 1 << 20,
      max_response: 1 << 20,
      max_response_cached: 4096,
      max_operations: 16,
      max_requests: 4,
    },
    lease_ns: 90_000_000_000,
  }
}

fn asked() -> ChannelAttrs {
  ChannelAttrs {
    header_pad: 0,
    max_request: 1 << 22,
    max_response: 1 << 22,
    max_response_cached: 1 << 22,
    max_operations: 64,
    max_requests: 64,
  }
}

fn owner(name: &str, verifier: u8) -> ExchangeId {
  ExchangeId {
    verifier: [verifier; 8],
    owner: name.as_bytes().to_vec(),
    principal: 501,
  }
}

/// A confirmed client and its session.
fn session(sessions: &mut Sessions, name: &str) -> (u64, [u8; 16]) {
  let granted = sessions.exchange_id(&owner(name, 1), 0).unwrap();
  let session = sessions
    .create_session(
      &CreateSession {
        clientid: granted.clientid,
        sequence: granted.sequenceid,
        fore: asked(),
        back: asked(),
      },
      0,
    )
    .unwrap();
  (granted.clientid, session.sessionid)
}

fn seq(sessionid: [u8; 16], slotid: u32, sequenceid: u32) -> Sequence {
  Sequence {
    sessionid,
    sequenceid,
    slotid,
    highest_slotid: slotid,
  }
}

/// §18.35, §18.36: do exchange an id and create a session; expect the client unconfirmed until the
/// session, then confirmed; the channel negotiated down to the offer; the same owner and verifier
/// getting the same id back.
#[test]
fn a_client_id_is_confirmed_by_its_first_session_and_the_channel_is_negotiated_down() {
  let mut sessions = Sessions::new(7, limits());
  let first = sessions.exchange_id(&owner("host-a", 1), 0).unwrap();
  assert_eq!(first.flags & EXCHGID4_FLAG_CONFIRMED_R, 0, "unconfirmed");
  assert_eq!(first.clientid >> 32, 7, "the server instance names the id");
  let session = sessions
    .create_session(
      &CreateSession {
        clientid: first.clientid,
        sequence: first.sequenceid,
        fore: asked(),
        back: asked(),
      },
      0,
    )
    .unwrap();
  assert_eq!(session.fore.max_requests, 4, "slots: the offer");
  assert_eq!(session.fore.max_operations, 16);
  assert_eq!(session.fore.max_response_cached, 4096);
  let again = sessions.exchange_id(&owner("host-a", 1), 0).unwrap();
  assert_eq!(again.clientid, first.clientid);
  assert_ne!(again.flags & EXCHGID4_FLAG_CONFIRMED_R, 0, "confirmed now");
}

/// §2.10.6.1: do send new requests on a slot, retry one, skip one; expect new, the cached reply
/// byte for byte, and `SEQ_MISORDERED`. A slot past the table is `BADSLOT`, an unknown session
/// `BADSESSION`.
#[test]
fn a_slot_runs_each_request_once_and_answers_its_retry_from_the_cache() {
  let mut sessions = Sessions::new(7, limits());
  let (clientid, sessionid) = session(&mut sessions, "host-a");
  assert_eq!(
    sessions.sequence(&seq(sessionid, 0, 1), 1).unwrap(),
    Sequenced::New {
      clientid,
      highest_slotid: 3
    }
  );
  sessions.store_reply(&sessionid, 0, b"reply one");
  assert_eq!(
    sessions.sequence(&seq(sessionid, 0, 1), 2).unwrap(),
    Sequenced::Replay(b"reply one".to_vec()),
    "the retry is answered from the cache"
  );
  assert_eq!(
    sessions.sequence(&seq(sessionid, 0, 3), 3),
    Err(Nfsstat4::SeqMisordered)
  );
  assert!(matches!(
    sessions.sequence(&seq(sessionid, 0, 2), 4).unwrap(),
    Sequenced::New { .. }
  ));
  assert_eq!(
    sessions.sequence(&seq(sessionid, 9, 1), 5),
    Err(Nfsstat4::Badslot)
  );
  assert_eq!(
    sessions.sequence(&seq([9; 16], 0, 1), 5),
    Err(Nfsstat4::Badsession)
  );
}

/// §2.10.6.1: a reply larger than the negotiated cache is not kept; its retry is
/// `RETRY_UNCACHED_REP`, which the client answers by resending as new.
#[test]
fn a_reply_past_the_cache_size_is_not_kept() {
  let mut sessions = Sessions::new(7, limits());
  let (_, sessionid) = session(&mut sessions, "host-a");
  sessions.sequence(&seq(sessionid, 1, 1), 1).unwrap();
  sessions.store_reply(&sessionid, 1, &vec![0u8; 8192]);
  assert_eq!(
    sessions.sequence(&seq(sessionid, 1, 1), 2),
    Err(Nfsstat4::RetryUncachedRep)
  );
}

/// §18.36.4: do retry a `CREATE_SESSION`; expect its grant back, not a second session. A skipped
/// sequence id is misordered.
#[test]
fn a_retried_create_session_returns_its_grant() {
  let mut sessions = Sessions::new(7, limits());
  let granted = sessions.exchange_id(&owner("host-a", 1), 0).unwrap();
  let args = CreateSession {
    clientid: granted.clientid,
    sequence: granted.sequenceid,
    fore: asked(),
    back: asked(),
  };
  let first = sessions.create_session(&args, 0).unwrap();
  assert_eq!(sessions.create_session(&args, 0).unwrap(), first);
  let skipped = CreateSession {
    sequence: granted.sequenceid + 5,
    ..args
  };
  assert_eq!(
    sessions.create_session(&skipped, 0),
    Err(Nfsstat4::SeqMisordered)
  );
}

/// §18.35.5: do exchange an id again with a new verifier (the client rebooted); expect a new id and the
/// old sessions gone. Another principal presenting the same owner is `CLID_INUSE`.
#[test]
fn a_rebooted_client_replaces_its_record_and_another_principal_cannot_take_it() {
  let mut sessions = Sessions::new(7, limits());
  let (old, sessionid) = session(&mut sessions, "host-a");
  let reborn = sessions.exchange_id(&owner("host-a", 2), 0).unwrap();
  assert_ne!(reborn.clientid, old);
  assert_eq!(
    sessions.sequence(&seq(sessionid, 0, 1), 1),
    Err(Nfsstat4::Badsession)
  );
  let intruder = ExchangeId {
    principal: 0,
    ..owner("host-a", 2)
  };
  assert_eq!(sessions.exchange_id(&intruder, 0), Err(Nfsstat4::ClidInuse));
}

/// Bounds: do fill the client table and a client's sessions; expect `RESOURCE` past each bound.
#[test]
fn the_tables_refuse_at_their_bounds() {
  let mut sessions = Sessions::new(7, limits());
  let (clientid, _) = session(&mut sessions, "host-a");
  session(&mut sessions, "host-b");
  assert_eq!(
    sessions.exchange_id(&owner("host-c", 1), 0),
    Err(Nfsstat4::Resource)
  );
  let again = sessions.exchange_id(&owner("host-a", 1), 0).unwrap();
  assert_eq!(
    sessions.create_session(
      &CreateSession {
        clientid,
        sequence: again.sequenceid,
        fore: asked(),
        back: asked(),
      },
      0
    ),
    Err(Nfsstat4::Resource),
    "one session per client in this offer"
  );
}

/// §18.50, §18.51, leases: do destroy a busy client, then its session and the client; reclaim twice; let
/// a lease lapse; expect `CLIENTID_BUSY`, success, `COMPLETE_ALREADY`, and the lapsed client dropped.
#[test]
fn destruction_reclaim_and_the_lease() {
  let mut sessions = Sessions::new(7, limits());
  let (clientid, sessionid) = session(&mut sessions, "host-a");
  assert_eq!(
    sessions.destroy_clientid(clientid),
    Err(Nfsstat4::ClientidBusy)
  );
  assert_eq!(sessions.reclaim_complete(clientid), Ok(()));
  assert_eq!(
    sessions.reclaim_complete(clientid),
    Err(Nfsstat4::CompleteAlready)
  );
  sessions.destroy_session(&sessionid).unwrap();
  assert_eq!(sessions.destroy_clientid(clientid), Ok(()));
  let (_, live) = session(&mut sessions, "host-b");
  sessions.sequence(&seq(live, 0, 1), 10).unwrap();
  assert_eq!(
    sessions.expire(10 + limits().lease_ns),
    0,
    "within the lease"
  );
  assert_eq!(sessions.expire(11 + limits().lease_ns), 1, "lapsed");
  assert_eq!(sessions.client_count(), 0);
}
