//! NFSv4.1 client ids and sessions (RFC 8881 §2.4, §2.10; operations §18.35 EXCHANGE_ID, §18.36
//! CREATE_SESSION, §18.37 DESTROY_SESSION, §18.46 SEQUENCE, §18.50 DESTROY_CLIENTID, §18.51
//! RECLAIM_COMPLETE): a pure state machine, the clock passed in, so every rule is testable alone.
//!
//! - **Client ids.** `EXCHANGE_ID` names a client by its owner string and boot verifier. A new owner
//!   gets an unconfirmed client id; a repeat with the same verifier gets the same id; a new verifier (the
//!   client rebooted) replaces the old record and its sessions. `CREATE_SESSION` confirms the client.
//! - **Sessions and slots.** A session has a slot table sized by the negotiated `ca_maxrequests`. Each
//!   `SEQUENCE` names a slot and a sequence id. The next id is a new request; the same id is a retry,
//!   answered from that slot's cached reply byte for byte (exactly-once semantics, §2.10.6); any other id
//!   is misordered. Every reply that fits the negotiated cache size is kept, so a retry never has to be
//!   refused as uncached.
//! - **Leases.** Every `SEQUENCE` renews its client's lease; [`Sessions::expire`] drops the clients whose
//!   lease has lapsed, and their sessions.
//! - **Bounds.** The client table and each client's sessions are bounded ([`Limits`]); a request past a
//!   bound is refused `NFS4ERR_RESOURCE`, never grown into.
//!
//! The state lives with whoever owns the connection's shard; it holds no lock and no shared reference.

use std::collections::BTreeMap;

use super::Nfsstat4;
use super::types::{ChannelAttrs, SESSIONID_SIZE, SessionId, VERIFIER_SIZE};

/// Format: `EXCHGID4_FLAG_CONFIRMED_R`, set in a reply for a client id `CREATE_SESSION` has confirmed.
pub const EXCHGID4_FLAG_CONFIRMED_R: u32 = 0x8000_0000;
/// Format: `EXCHGID4_FLAG_USE_NON_PNFS`: this server is a plain NFSv4.1 server (no pNFS roles yet).
pub const EXCHGID4_FLAG_USE_NON_PNFS: u32 = 0x0001_0000;

/// The bounds and offers of the session state, derived by the caller from the machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
  /// The most clients the table holds.
  pub max_clients: usize,
  /// The most sessions one client holds at once.
  pub max_sessions_per_client: usize,
  /// What the server offers a session's fore channel (each field an upper bound).
  pub offer: ChannelAttrs,
  /// A client's lease, in nanoseconds: one that has not sent a `SEQUENCE` within it is dropped.
  pub lease_ns: u64,
}

/// What `EXCHANGE_ID` asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExchangeId {
  /// The client owner's boot verifier (`co_verifier`).
  pub verifier: [u8; VERIFIER_SIZE],
  /// The client owner string (`co_ownerid`).
  pub owner: Vec<u8>,
  /// The principal the call ran as (its `AUTH_SYS` uid, or the TLS identity's).
  pub principal: u32,
}

/// What `EXCHANGE_ID` granted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExchangeIdGranted {
  /// The client id.
  pub clientid: u64,
  /// The sequence id the client's next `CREATE_SESSION` must carry.
  pub sequenceid: u32,
  /// The reply flags.
  pub flags: u32,
}

/// What `CREATE_SESSION` asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CreateSession {
  /// The client id.
  pub clientid: u64,
  /// The client's `CREATE_SESSION` sequence id.
  pub sequence: u32,
  /// The fore channel the client asked for.
  pub fore: ChannelAttrs,
  /// The back channel the client asked for (echoed, negotiated; this server makes no callbacks yet).
  pub back: ChannelAttrs,
}

/// What `CREATE_SESSION` granted (also the reply a retry of it gets back).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionGranted {
  /// The new session's id.
  pub sessionid: SessionId,
  /// The sequence id echoed.
  pub sequence: u32,
  /// The fore channel granted.
  pub fore: ChannelAttrs,
  /// The back channel granted.
  pub back: ChannelAttrs,
}

/// What `SEQUENCE` asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sequence {
  /// The session.
  pub sessionid: SessionId,
  /// The slot's sequence id for this request.
  pub sequenceid: u32,
  /// The slot.
  pub slotid: u32,
  /// The highest slot the client is using.
  pub highest_slotid: u32,
}

/// How a `SEQUENCE` is to proceed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sequenced {
  /// A new request: run the rest of the compound, then [`Sessions::store_reply`] its reply. The client
  /// the session belongs to, and the slot table's highest slot, ride along for the reply.
  New {
    /// The session's client.
    clientid: u64,
    /// The session's highest slot id.
    highest_slotid: u32,
  },
  /// A retry of the last request on this slot: the cached reply, to send back as it is.
  Replay(Vec<u8>),
}

struct Client {
  owner: Vec<u8>,
  verifier: [u8; VERIFIER_SIZE],
  principal: u32,
  confirmed: bool,
  /// The sequence id the next `CREATE_SESSION` must carry.
  create_seq: u32,
  /// The last `CREATE_SESSION`'s grant, for its retry.
  last_create: Option<SessionGranted>,
  sessions: Vec<SessionId>,
  renewed_ns: u64,
  reclaim_complete: bool,
}

/// One slot: its last sequence id, that request's kept reply, and whether that request is still being
/// served (a compound can await another shard, so its retry may arrive before it finishes).
struct Slot {
  seq: u32,
  reply: Option<Vec<u8>>,
  in_flight: bool,
}

struct Session {
  clientid: u64,
  fore: ChannelAttrs,
  slots: Vec<Slot>,
}

/// The server's client ids and sessions.
pub struct Sessions {
  limits: Limits,
  /// This server instance's identity: the high half of every client id and part of every session id,
  /// so an id from a previous instance is recognizably stale.
  boot: u32,
  next_client: u32,
  next_session: u64,
  clients: BTreeMap<u64, Client>,
  owners: BTreeMap<Vec<u8>, u64>,
  sessions: BTreeMap<SessionId, Session>,
}

impl Sessions {
  /// Empty state for a server instance named `boot`, under `limits`.
  pub fn new(boot: u32, limits: Limits) -> Sessions {
    Sessions {
      limits,
      boot,
      next_client: 1,
      next_session: 1,
      clients: BTreeMap::new(),
      owners: BTreeMap::new(),
      sessions: BTreeMap::new(),
    }
  }

  /// `EXCHANGE_ID` (§18.35.5): the client id for an owner, created, repeated or replaced.
  pub fn exchange_id(
    &mut self,
    args: &ExchangeId,
    now_ns: u64,
  ) -> Result<ExchangeIdGranted, Nfsstat4> {
    if let Some(&clientid) = self.owners.get(&args.owner) {
      let existing = self.clients.get(&clientid).ok_or(Nfsstat4::Serverfault)?;
      if existing.principal != args.principal {
        return Err(Nfsstat4::ClidInuse);
      }
      if existing.verifier == args.verifier {
        return Ok(self.granted(clientid));
      }
      // The client rebooted: its old record and sessions go (§18.35.5 case 5).
      self.drop_client(clientid);
    }
    // Expiry is lazy (a courteous server, RFC 8881 §8.3): a lapsed client keeps its state until the
    // table is full, when every lapsed client makes room before a new one is refused.
    if self.clients.len() >= self.limits.max_clients {
      self.expire(now_ns);
    }
    if self.clients.len() >= self.limits.max_clients {
      return Err(Nfsstat4::Resource);
    }
    let clientid = (u64::from(self.boot) << u32::BITS) | u64::from(self.next_client);
    self.next_client = self.next_client.checked_add(1).ok_or(Nfsstat4::Resource)?;
    self.clients.insert(
      clientid,
      Client {
        owner: args.owner.clone(),
        verifier: args.verifier,
        principal: args.principal,
        confirmed: false,
        create_seq: 1,
        last_create: None,
        sessions: Vec::new(),
        renewed_ns: now_ns,
        reclaim_complete: false,
      },
    );
    self.owners.insert(args.owner.clone(), clientid);
    Ok(self.granted(clientid))
  }

  fn granted(&self, clientid: u64) -> ExchangeIdGranted {
    let client = &self.clients[&clientid];
    let confirmed = if client.confirmed {
      EXCHGID4_FLAG_CONFIRMED_R
    } else {
      0
    };
    ExchangeIdGranted {
      clientid,
      sequenceid: client.create_seq,
      flags: EXCHGID4_FLAG_USE_NON_PNFS | confirmed,
    }
  }

  /// `CREATE_SESSION` (§18.36.4): a new session for a client, confirming it; a retry of the last one
  /// gets its grant back.
  pub fn create_session(
    &mut self,
    args: &CreateSession,
    now_ns: u64,
  ) -> Result<SessionGranted, Nfsstat4> {
    let limits = self.limits;
    let boot = self.boot;
    let client = self
      .clients
      .get_mut(&args.clientid)
      .ok_or(Nfsstat4::StaleClientid)?;
    if args.sequence == client.create_seq.wrapping_sub(1) {
      return client.last_create.ok_or(Nfsstat4::SeqMisordered);
    }
    if args.sequence != client.create_seq {
      return Err(Nfsstat4::SeqMisordered);
    }
    if client.sessions.len() >= limits.max_sessions_per_client {
      return Err(Nfsstat4::Resource);
    }
    let fore = ChannelAttrs::negotiated(&args.fore, &limits.offer);
    let back = ChannelAttrs::negotiated(&args.back, &limits.offer);
    let mut sessionid = [0u8; SESSIONID_SIZE];
    sessionid[..8].copy_from_slice(&args.clientid.to_be_bytes());
    sessionid[8..12].copy_from_slice(&boot.to_be_bytes());
    sessionid[12..].copy_from_slice(&u32::try_from(self.next_session).unwrap_or(0).to_be_bytes());
    self.next_session = self.next_session.saturating_add(1);
    let granted = SessionGranted {
      sessionid,
      sequence: args.sequence,
      fore,
      back,
    };
    client.confirmed = true;
    client.create_seq = client.create_seq.wrapping_add(1);
    client.last_create = Some(granted);
    client.sessions.push(sessionid);
    client.renewed_ns = now_ns;
    let slots = usize::try_from(fore.max_requests).unwrap_or(1);
    self.sessions.insert(
      sessionid,
      Session {
        clientid: args.clientid,
        fore,
        slots: (0..slots)
          .map(|_| Slot {
            seq: 0,
            reply: None,
            in_flight: false,
          })
          .collect(),
      },
    );
    Ok(granted)
  }

  /// `SEQUENCE` (§18.46.3, slot rules §2.10.6.1): whether the compound is new, a retry to answer from
  /// the cache, or refused. A new request renews the client's lease.
  pub fn sequence(&mut self, args: &Sequence, now_ns: u64) -> Result<Sequenced, Nfsstat4> {
    let session = self
      .sessions
      .get_mut(&args.sessionid)
      .ok_or(Nfsstat4::Badsession)?;
    let highest = u32::try_from(session.slots.len().saturating_sub(1)).unwrap_or(0);
    let slot = usize::try_from(args.slotid)
      .ok()
      .and_then(|slot| session.slots.get_mut(slot))
      .ok_or(Nfsstat4::Badslot)?;
    if args.highest_slotid > highest {
      return Err(Nfsstat4::BadHighSlot);
    }
    if args.sequenceid == slot.seq {
      // A retry of the request still being served waits for it (§2.10.6.1: `NFS4ERR_DELAY`); one whose
      // reply was not kept is refused so the client resends it as new.
      if slot.in_flight {
        return Err(Nfsstat4::Delay);
      }
      return slot
        .reply
        .clone()
        .map(Sequenced::Replay)
        .ok_or(Nfsstat4::RetryUncachedRep);
    }
    if args.sequenceid != slot.seq.wrapping_add(1) {
      return Err(Nfsstat4::SeqMisordered);
    }
    slot.seq = args.sequenceid;
    slot.reply = None;
    slot.in_flight = true;
    let clientid = session.clientid;
    if let Some(client) = self.clients.get_mut(&clientid) {
      client.renewed_ns = now_ns;
    }
    Ok(Sequenced::New {
      clientid,
      highest_slotid: highest,
    })
  }

  /// Ends the new request on `slot` of `sessionid`, keeping its reply for its retries when it fits the
  /// session's negotiated cache size. A reply that does not fit is not kept; its retry is then refused
  /// `NFS4ERR_RETRY_UNCACHED_REP`, which a client handles by resending as new.
  pub fn store_reply(&mut self, sessionid: &SessionId, slotid: u32, reply: &[u8]) {
    let Some(session) = self.sessions.get_mut(sessionid) else {
      return;
    };
    let fits = u32::try_from(reply.len()).is_ok_and(|len| len <= session.fore.max_response_cached);
    if let Some(slot) = usize::try_from(slotid)
      .ok()
      .and_then(|slot| session.slots.get_mut(slot))
    {
      slot.in_flight = false;
      if fits {
        slot.reply = Some(reply.to_vec());
      }
    }
  }

  /// `DESTROY_SESSION` (§18.37).
  pub fn destroy_session(&mut self, sessionid: &SessionId) -> Result<(), Nfsstat4> {
    let session = self
      .sessions
      .remove(sessionid)
      .ok_or(Nfsstat4::Badsession)?;
    if let Some(client) = self.clients.get_mut(&session.clientid) {
      client.sessions.retain(|id| id != sessionid);
    }
    Ok(())
  }

  /// `DESTROY_CLIENTID` (§18.50): refused while the client holds a session.
  pub fn destroy_clientid(&mut self, clientid: u64) -> Result<(), Nfsstat4> {
    let client = self.clients.get(&clientid).ok_or(Nfsstat4::StaleClientid)?;
    if !client.sessions.is_empty() {
      return Err(Nfsstat4::ClientidBusy);
    }
    self.drop_client(clientid);
    Ok(())
  }

  /// `RECLAIM_COMPLETE` (§18.51) for the client of a session: the first call records it, a second is
  /// `NFS4ERR_COMPLETE_ALREADY`. This server keeps no state across a restart that needs reclaiming,
  /// so there is never a grace period.
  pub fn reclaim_complete(&mut self, clientid: u64) -> Result<(), Nfsstat4> {
    let client = self
      .clients
      .get_mut(&clientid)
      .ok_or(Nfsstat4::StaleClientid)?;
    if client.reclaim_complete {
      return Err(Nfsstat4::CompleteAlready);
    }
    client.reclaim_complete = true;
    Ok(())
  }

  /// Drops every client whose lease lapsed before `now_ns`, with its sessions; returns how many.
  pub fn expire(&mut self, now_ns: u64) -> usize {
    let lease = self.limits.lease_ns;
    let lapsed: Vec<u64> = self
      .clients
      .iter()
      .filter(|(_, client)| now_ns.saturating_sub(client.renewed_ns) > lease)
      .map(|(id, _)| *id)
      .collect();
    for clientid in &lapsed {
      self.drop_client(*clientid);
    }
    lapsed.len()
  }

  /// The client a session belongs to.
  pub fn client_of(&self, sessionid: &SessionId) -> Option<u64> {
    self.sessions.get(sessionid).map(|session| session.clientid)
  }

  /// The fore channel a session negotiated.
  pub fn fore_channel(&self, sessionid: &SessionId) -> Option<ChannelAttrs> {
    self.sessions.get(sessionid).map(|session| session.fore)
  }

  /// Whether the table holds `clientid`.
  pub fn holds_client(&self, clientid: u64) -> bool {
    self.clients.contains_key(&clientid)
  }

  /// How many clients the table holds.
  pub fn client_count(&self) -> usize {
    self.clients.len()
  }

  fn drop_client(&mut self, clientid: u64) {
    if let Some(client) = self.clients.remove(&clientid) {
      self.owners.remove(&client.owner);
      for sessionid in client.sessions {
        self.sessions.remove(&sessionid);
      }
    }
  }
}
