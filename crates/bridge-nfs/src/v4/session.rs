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
//! - **Departure (A-76).** A session may leave the table that minted it (its *home*) for the shard that owns the
//!   volume its compounds use, so they run where their data lives: [`Sessions::depart`] hands it off whole (its
//!   slots and their kept replies, so exactly-once survives the move) when no slot is in flight, and the home
//!   remembers where it went; [`Sessions::arrive`] takes it in as a *guest*. The client record stays home, where
//!   it is durable (A-37): a guest's `SEQUENCE` asks for a renewal note home at most [`RENEWALS_PER_LEASE`] times
//!   a lease, and a client dropped at home leaves a drop for each of its guests ([`Sessions::take_guest_drops`]).
//!
//! The state lives with whoever owns the connection's shard; it holds no lock and no shared reference.

use std::collections::BTreeMap;

use super::Nfsstat4;
use super::types::{ChannelAttrs, SESSIONID_SIZE, SessionId, VERIFIER_SIZE};

/// Format: `EXCHGID4_FLAG_CONFIRMED_R`, set in a reply for a client id `CREATE_SESSION` has confirmed.
pub const EXCHGID4_FLAG_CONFIRMED_R: u32 = 0x8000_0000;
/// Format: `EXCHGID4_FLAG_USE_NON_PNFS`: this server is a plain NFSv4.1 server (no pNFS roles yet).
pub const EXCHGID4_FLAG_USE_NON_PNFS: u32 = 0x0001_0000;

/// Derived: renewal notes a guest session sends its client's home per lease: twice, so the home's lease, checked
/// against the whole lease, never lapses for a client a guest is still serving (at most half a lease stale).
pub const RENEWALS_PER_LEASE: u64 = 2;

/// A session handed off whole by [`Sessions::depart`], for [`Sessions::arrive`] on the shard it moves to. A clone
/// is what a refused move hands back to its home ([`Sessions::return_home`]).
#[derive(Clone, Debug)]
pub struct DepartedSession(Session);

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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateSession {
  /// The client id.
  pub clientid: u64,
  /// The client's `CREATE_SESSION` sequence id.
  pub sequence: u32,
  /// The fore channel the client asked for.
  pub fore: ChannelAttrs,
  /// The back channel the client asked for, negotiated down to the offer.
  pub back: ChannelAttrs,
  /// The callback program the client serves on this connection (`csa_cb_program`) and the RPC credential to call it
  /// under (an encoded `opaque_auth` of a flavor the client listed), when it asked for the back channel on the
  /// connection (`CREATE_SESSION4_FLAG_CONN_BACK_CHAN`) with a flavor this server can call back with.
  pub callback: Option<(u32, Vec<u8>)>,
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
  /// Whether the connection that created the session carries its back channel (§18.36.3).
  pub back_channel: bool,
}

/// Whether a session's back channel has answered (RFC 8881 §10.2: "a server avoids delegating responsibilities
/// until it has determined that the backchannel exists").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallbackState {
  /// Granted at `CREATE_SESSION`, not yet answered a probe.
  Unproven,
  /// Answered its last callback.
  Up,
  /// Failed or did not answer its last callback (`SEQ4_STATUS_CB_PATH_DOWN` until it answers again).
  Down,
}

/// What the next callback on a session's back channel carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NextCallback {
  /// The client's callback program.
  pub program: u32,
  /// The RPC credential to call under.
  pub credential: Vec<u8>,
  /// The minor version to name.
  pub minor: u32,
  /// The slot's sequence id for this callback.
  pub sequence: u32,
}

/// A session's back channel (§2.10.3.1): the client's callback program and the slot this server calls on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackChannel {
  /// The client's callback program number.
  pub program: u32,
  /// The RPC credential callbacks carry (an encoded `opaque_auth` the client listed at `CREATE_SESSION`).
  pub credential: Vec<u8>,
  /// The minor version callbacks name: the one the client's compounds use on the session (a client matches a
  /// callback to its session by it as well as by the session id; an NFSv4.2 client answers a minor-1 callback
  /// `NFS4ERR_BADSESSION`, measured 2026-10-04).
  pub minor: u32,
  /// The back channel's negotiated sizes.
  pub attrs: ChannelAttrs,
  /// The sequence id of the last callback on slot 0 (one callback at a time per session).
  pub sequence: u32,
  /// Whether it has answered.
  pub state: CallbackState,
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
  /// The whole request's encoded size, its RPC headers included (not its record marker), which the
  /// session's `ca_maxrequestsize` bounds (RFC 8881 §18.36.3).
  pub request_bytes: usize,
  /// The compound's operation count, which `ca_maxoperations` bounds.
  pub operations: u32,
  /// Whether the client asks for the reply to be kept for a retry (`sa_cachethis`).
  pub cache_this: bool,
}

/// The sizes a new request's reply must keep to (RFC 8881 §2.10.6.4): the session's
/// `ca_maxresponsesize`, and — when the client asked for the reply to be kept — its
/// `ca_maxresponsesize_cached`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplyLimits {
  /// The most bytes a reply may take, its RPC header included.
  pub max_response: usize,
  /// The most bytes a reply the client asked to be kept may take; `None` when it did not ask.
  pub max_cached: Option<usize>,
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
    /// The sizes the reply must keep to.
    limits: ReplyLimits,
    /// A guest session whose client's home is due a renewal note (A-76).
    renew_home: bool,
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
  /// Rebuilt from a record after a restart and not yet given a session: its last CREATE_SESSION's
  /// grant was not kept, so a retry of it is served afresh.
  restored: bool,
}

/// One slot: its last sequence id, that request's kept reply, and whether that request is still being
/// served (a compound can await another shard, so its retry may arrive before it finishes).
#[derive(Clone, Debug)]
struct Slot {
  seq: u32,
  reply: Option<Vec<u8>>,
  in_flight: bool,
}

#[derive(Clone, Debug)]
struct Session {
  clientid: u64,
  fore: ChannelAttrs,
  slots: Vec<Slot>,
  /// A session that arrived from its home (A-76): its client's record is there, not here.
  guest: bool,
  /// The back channel, when the client asked for one on the creating connection.
  back: Option<BackChannel>,
  /// When a guest last asked for a renewal note home.
  noted_ns: u64,
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
  /// The clients dropped since the last [`Sessions::take_dropped`], whose state the owners must drop
  /// too; never more than the client table held.
  dropped: Vec<u64>,
  /// The client changes since the last [`Sessions::take_changes`], for the listener's partition to
  /// record, when the table is durable (`None` for a standalone server); drained after every compound.
  changes: Option<Vec<ClientChange>>,
  /// Sessions of this table's clients that departed, and the shard each went to (A-76); never more than the
  /// clients' sessions, since each is one of them.
  away: BTreeMap<SessionId, u16>,
  /// Departed sessions whose client was dropped here, for their guest shards to drop too; never more than `away`
  /// held.
  guest_drops: Vec<(SessionId, u16)>,
  /// Guest sessions destroyed here, for their home to forget; never more than the guests held.
  forgotten: Vec<SessionId>,
}

/// A client as a durable record carries it (§4.6 A-37): enough for a restarted listener to keep the
/// client id valid. Its sessions are not kept: the client re-creates them after `NFS4ERR_BADSESSION`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientRecord {
  /// The client id.
  pub clientid: u64,
  /// The client owner.
  pub owner: Vec<u8>,
  /// The owner's verifier.
  pub verifier: [u8; VERIFIER_SIZE],
  /// The principal that established it.
  pub principal: u32,
  /// The next CREATE_SESSION sequence.
  pub create_seq: u32,
}

/// One change to the client table, as the listener's partition records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientChange {
  /// A client was recorded or changed.
  Set(ClientRecord),
  /// A client was dropped.
  Cleared(u64),
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
      dropped: Vec::new(),
      changes: None,
      away: BTreeMap::new(),
      guest_drops: Vec::new(),
      forgotten: Vec::new(),
    }
  }

  /// A durable table (§4.6 A-37): every change is journaled for the listener's partition, and the
  /// clients kept before a restart are back, their leases starting now. A kept client that had created
  /// a session is confirmed; new client ids carry the new `boot`, so they never collide with kept ones.
  pub fn restore(boot: u32, limits: Limits, records: Vec<ClientRecord>, now_ns: u64) -> Sessions {
    let mut sessions = Sessions::new(boot, limits);
    for record in records {
      sessions
        .owners
        .insert(record.owner.clone(), record.clientid);
      sessions.clients.insert(
        record.clientid,
        Client {
          owner: record.owner,
          verifier: record.verifier,
          principal: record.principal,
          confirmed: record.create_seq > 1,
          create_seq: record.create_seq,
          last_create: None,
          sessions: Vec::new(),
          renewed_ns: now_ns,
          reclaim_complete: false,
          restored: true,
        },
      );
    }
    sessions.changes = Some(Vec::new());
    sessions
  }

  /// The changes since the last call, for the listener's partition to record; none for a standalone
  /// server.
  pub fn take_changes(&mut self) -> Vec<ClientChange> {
    self
      .changes
      .as_mut()
      .map(std::mem::take)
      .unwrap_or_default()
  }

  /// Journals the current record of `clientid`, or its clearing.
  fn journal_client(&mut self, clientid: u64) {
    let Some(changes) = self.changes.as_mut() else {
      return;
    };
    changes.push(match self.clients.get(&clientid) {
      Some(client) => ClientChange::Set(ClientRecord {
        clientid,
        owner: client.owner.clone(),
        verifier: client.verifier,
        principal: client.principal,
        create_seq: client.create_seq,
      }),
      None => ClientChange::Cleared(clientid),
    });
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
    // `NFS4ERR_DELAY`: a lapsed client makes room later, and `NFS4ERR_RESOURCE` is not valid in
    // NFSv4.1 (RFC 7863; EXCHANGE_ID's statuses, RFC 8881 §15.2).
    if self.clients.len() >= self.limits.max_clients {
      return Err(Nfsstat4::Delay);
    }
    let clientid = (u64::from(self.boot) << u32::BITS) | u64::from(self.next_client);
    self.next_client = self
      .next_client
      .checked_add(1)
      .ok_or(Nfsstat4::Serverfault)?;
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
        restored: false,
      },
    );
    self.owners.insert(args.owner.clone(), clientid);
    self.journal_client(clientid);
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
    // A retry of the last CREATE_SESSION gets its grant back. A retry after a restart finds no kept
    // grant (sessions are not durable, A-37): the client never received one, so the retry creates the
    // session afresh without advancing the sequence a second time.
    let retry = args.sequence == client.create_seq.wrapping_sub(1);
    if retry && let Some(granted) = client.last_create {
      return Ok(granted);
    }
    if (retry && !client.restored) || (!retry && args.sequence != client.create_seq) {
      return Err(Nfsstat4::SeqMisordered);
    }
    // `NFS4ERR_NOSPC`, the exhaustion status CREATE_SESSION allows (RFC 8881 §15.2).
    if client.sessions.len() >= limits.max_sessions_per_client {
      return Err(Nfsstat4::Nospc);
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
      back_channel: args.callback.is_some(),
    };
    client.confirmed = true;
    client.restored = false;
    if !retry {
      client.create_seq = client.create_seq.wrapping_add(1);
    }
    client.last_create = Some(granted);
    client.sessions.push(sessionid);
    client.renewed_ns = now_ns;
    let clientid = args.clientid;
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
        guest: false,
        back: args
          .callback
          .clone()
          .map(|(program, credential)| BackChannel {
            program,
            credential,
            minor: 1,
            attrs: back,
            sequence: 0,
            state: CallbackState::Unproven,
          }),
        noted_ns: now_ns,
      },
    );
    self.journal_client(clientid);
    Ok(granted)
  }

  /// `SEQUENCE` (§18.46.3, slot rules §2.10.6.1): whether the compound is new, a retry to answer from
  /// the cache, or refused. A new request renews the client's lease.
  pub fn sequence(&mut self, args: &Sequence, now_ns: u64) -> Result<Sequenced, Nfsstat4> {
    let session = self
      .sessions
      .get_mut(&args.sessionid)
      .ok_or(Nfsstat4::Badsession)?;
    // The negotiated sizes bound the request before its slot is touched, so a refused request consumes
    // no sequence id and the client resends a smaller one (RFC 8881 §2.10.6.4, §18.46.3).
    let max_request = usize::try_from(session.fore.max_request).unwrap_or(usize::MAX);
    if args.request_bytes > max_request {
      return Err(Nfsstat4::ReqTooBig);
    }
    if args.operations > session.fore.max_operations {
      return Err(Nfsstat4::TooManyOps);
    }
    let limits = ReplyLimits {
      max_response: usize::try_from(session.fore.max_response).unwrap_or(usize::MAX),
      max_cached: args
        .cache_this
        .then(|| usize::try_from(session.fore.max_response_cached).unwrap_or(usize::MAX)),
    };
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
    let note_every = self.limits.lease_ns / RENEWALS_PER_LEASE;
    let renew_home = session.guest && now_ns.saturating_sub(session.noted_ns) >= note_every;
    if renew_home {
      session.noted_ns = now_ns;
    }
    if let Some(client) = self.clients.get_mut(&clientid) {
      client.renewed_ns = now_ns;
    }
    Ok(Sequenced::New {
      clientid,
      highest_slotid: highest,
      limits,
      renew_home,
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

  /// `DESTROY_SESSION` (§18.37). A session that departed (A-76) is destroyed from its home too: its client forgets
  /// it here and its guest is left a drop.
  pub fn destroy_session(&mut self, sessionid: &SessionId) -> Result<(), Nfsstat4> {
    if let Some(to) = self.away.remove(sessionid) {
      self.guest_drops.push((*sessionid, to));
      for client in self.clients.values_mut() {
        client.sessions.retain(|id| id != sessionid);
      }
      return Ok(());
    }
    let session = self
      .sessions
      .remove(sessionid)
      .ok_or(Nfsstat4::Badsession)?;
    if session.guest {
      self.forgotten.push(*sessionid);
    }
    if let Some(client) = self.clients.get_mut(&session.clientid) {
      client.sessions.retain(|id| id != sessionid);
    }
    Ok(())
  }

  /// The guest sessions destroyed here since the last call, for their home to forget ([`Sessions::forget_away`]).
  pub fn take_forgotten(&mut self) -> Vec<SessionId> {
    std::mem::take(&mut self.forgotten)
  }

  /// Hands session `sessionid` off whole for the shard `to` (A-76), its slots and their kept replies with it; the
  /// home keeps its client and remembers where the session went. `None`, and nothing changes, when the session is
  /// not this table's own (absent, or a guest here) or a slot is still being served (its reply must be kept here).
  pub fn depart(&mut self, sessionid: &SessionId, to: u16) -> Option<DepartedSession> {
    let session = self.sessions.get(sessionid)?;
    if session.guest
      || session.slots.iter().any(|slot| slot.in_flight)
      || !self.clients.contains_key(&session.clientid)
    {
      return None;
    }
    let session = self.sessions.remove(sessionid)?;
    self.away.insert(*sessionid, to);
    Some(DepartedSession(session))
  }

  /// Takes in a departed session as a guest (A-76); its client stays at its home, which the caller knows. A session
  /// id already held here is left as it is.
  pub fn arrive(&mut self, sessionid: SessionId, departed: DepartedSession, now_ns: u64) {
    let DepartedSession(mut session) = departed;
    session.guest = true;
    session.noted_ns = now_ns;
    self.sessions.entry(sessionid).or_insert(session);
  }

  /// Takes back a session whose departure could not complete (its move was refused before it left): it is this
  /// table's own again, as if it had never departed.
  pub fn return_home(&mut self, sessionid: SessionId, departed: DepartedSession) {
    if self.away.remove(&sessionid).is_some() {
      let DepartedSession(mut session) = departed;
      session.guest = false;
      self.sessions.entry(sessionid).or_insert(session);
    }
  }

  /// The back channel of `sessionid`, when it has one.
  pub fn back_channel(&self, sessionid: &SessionId) -> Option<BackChannel> {
    self
      .sessions
      .get(sessionid)
      .and_then(|session| session.back.clone())
  }

  /// The next callback on `sessionid`'s back channel: its program, its credential and the sequence id slot 0 takes,
  /// advanced.
  pub fn next_callback(&mut self, sessionid: &SessionId) -> Option<NextCallback> {
    let back = self.sessions.get_mut(sessionid)?.back.as_mut()?;
    back.sequence = back.sequence.wrapping_add(1);
    Some(NextCallback {
      program: back.program,
      credential: back.credential.clone(),
      minor: back.minor,
      sequence: back.sequence,
    })
  }

  /// Records the minor version `sessionid`'s client uses, for its callbacks to name.
  pub fn set_callback_minor(&mut self, sessionid: &SessionId, minor: u32) {
    if let Some(back) = self
      .sessions
      .get_mut(sessionid)
      .and_then(|session| session.back.as_mut())
    {
      back.minor = minor;
    }
  }

  /// Records whether `sessionid`'s back channel answered its last callback.
  pub fn set_callback_state(&mut self, sessionid: &SessionId, state: CallbackState) {
    if let Some(back) = self
      .sessions
      .get_mut(sessionid)
      .and_then(|session| session.back.as_mut())
    {
      back.state = state;
    }
  }

  /// The shard a session of this table's departed to (A-76).
  pub fn away(&self, sessionid: &SessionId) -> Option<u16> {
    self.away.get(sessionid).copied()
  }

  /// Whether this table serves `sessionid` (its own or a guest).
  pub fn holds_session(&self, sessionid: &SessionId) -> bool {
    self.sessions.contains_key(sessionid)
  }

  /// A guest's renewal note (A-76): `clientid` was served at `now_ns`.
  pub fn renew(&mut self, clientid: u64, now_ns: u64) {
    if let Some(client) = self.clients.get_mut(&clientid) {
      client.renewed_ns = client.renewed_ns.max(now_ns);
    }
  }

  /// A departed session its guest destroyed (A-76): forgotten here, its client's count freed.
  pub fn forget_away(&mut self, sessionid: &SessionId) {
    if self.away.remove(sessionid).is_some() {
      for client in self.clients.values_mut() {
        client.sessions.retain(|id| id != sessionid);
      }
    }
  }

  /// Drops guest session `sessionid` (its client was dropped at its home); whether it was held.
  pub fn drop_guest(&mut self, sessionid: &SessionId) -> bool {
    match self.sessions.get(sessionid) {
      Some(session) if session.guest => self.sessions.remove(sessionid).is_some(),
      _ => false,
    }
  }

  /// The departed sessions whose client was dropped or that were destroyed here since the last call, each with the
  /// shard holding it, for that shard to drop.
  pub fn take_guest_drops(&mut self) -> Vec<(SessionId, u16)> {
    std::mem::take(&mut self.guest_drops)
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
        match self.away.remove(&sessionid) {
          Some(to) => self.guest_drops.push((sessionid, to)),
          None => {
            self.sessions.remove(&sessionid);
          }
        }
      }
      self.dropped.push(clientid);
      self.journal_client(clientid);
    }
  }

  /// The clients dropped (replaced, expired or destroyed) since the last call.
  pub fn take_dropped(&mut self) -> Vec<u64> {
    std::mem::take(&mut self.dropped)
  }
}
