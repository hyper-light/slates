//! The Raft wire (§4.8, §4.10a) — the on-the-wire encoding of the configuration group's Raft messages
//! ([`RequestVote`], [`VoteReply`], [`AppendEntries`], [`AppendReply`], [`PreVote`], [`PreVoteReply`],
//! [`TimeoutNow`], [`InstallSnapshot`], [`InstallSnapshotReply`]) so the dialect can be driven over
//! the fleet transport. The state machine ([`crate::raft`]) is sans-io and message-passing; this module
//! is the pure codec that turns those messages into bytes and back.
//!
//! Every decode is a parser of external bytes: it checks each length against the bytes that actually
//! arrived before allocating, so a hostile datagram claiming a huge entry count, command length or voter
//! count is a typed [`RaftWireError`], never a panic or an over-allocation. Little-endian throughout, so
//! two hosts encode a message identically (the determinism the golden vectors pin).

use std::mem::size_of;

use slates_db::register::{
  DomainId, HostEpoch, HostId, Neighbourhood, OBJECT_BYTES, ObjectId, Quorum, RegionId,
  RegionalConfiguration, Retirement, RootConfiguration, Settled,
};
use slates_transport::connection::Priority;
use slates_transport::endpoint::{Endpoint, EndpointError};

use crate::raft::{
  AppendEntries, AppendReply, ElectionPriority, FastPropose, FastVote, InstallSnapshot,
  InstallSnapshotReply, LogEntry, PreVote, PreVoteReply, RaftNode, RequestVote, SlotReport,
  TimeoutNow, VoteReply, VoterConfig, WindowSlot,
};

/// A Raft message on the wire.
#[derive(Clone, Debug, PartialEq)]
pub enum RaftMessage {
  /// A vote request.
  RequestVote(RequestVote),
  /// A vote reply.
  VoteReply(VoteReply),
  /// An append (replication or heartbeat).
  AppendEntries(AppendEntries),
  /// An append reply.
  AppendReply(AppendReply),
  /// A pre-vote request (Raft §9.6): "could a real election win?", asked without inflating the term, so a
  /// partitioned node cannot disrupt a healthy leader.
  PreVote(PreVote),
  /// A pre-vote reply.
  PreVoteReply(PreVoteReply),
  /// A leader's invitation to a caught-up voter to campaign at once (thesis §3.10, leadership transfer).
  /// It has no reply: the invited voter's vote requests are its answer.
  TimeoutNow(TimeoutNow),
  /// A leader's snapshot for a follower whose next entries were compacted away (Raft §7).
  InstallSnapshot(InstallSnapshot),
  /// A follower's reply to a snapshot: how far its log now matches the leader's.
  InstallSnapshotReply(InstallSnapshotReply),
  /// A proposer's command sent straight to every voter (the fast track, research record §3.7).
  FastPropose(FastPropose),
  /// A voter's fast vote, sent to its term's leader.
  FastVote(FastVote),
}

/// A refusal to decode a Raft message from received bytes (the closed hostile-input taxonomy).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RaftWireError {
  /// No consensus group has been installed on this fresh member.
  Uninitialized,
  /// A message belongs to another bootstrap's group, even if its certificate is rostered.
  ForeignGroup,
  /// The group envelope is malformed.
  MalformedEnvelope,
  /// The message claims a member other than the authenticated session's peer.
  ForeignSender,
  /// The bytes are shorter than the message shape requires.
  Truncated,
  /// The leading tag byte is not a known message kind.
  UnknownTag {
    /// The tag byte that arrived.
    tag: u8,
  },
  /// A declared count (entries, a command length, or a voter set) exceeds the bytes that followed.
  LengthMismatch,
  /// A boolean or option-presence byte was neither zero nor one.
  BadFlag {
    /// The byte that arrived.
    flag: u8,
  },
}

/// Format: the leading tag byte; these are its values.
const TAG_REQUEST_VOTE: u8 = 1;
const TAG_VOTE_REPLY: u8 = 2;
const TAG_APPEND_ENTRIES: u8 = 3;
const TAG_APPEND_REPLY: u8 = 4;
/// Format: the pre-election tag bytes, continuing the leading-tag sequence.
const TAG_PRE_VOTE: u8 = 5;
const TAG_PRE_VOTE_REPLY: u8 = 6;
/// Format: the leadership-transfer invitation's tag byte, continuing the sequence.
const TAG_TIMEOUT_NOW: u8 = 7;
/// Format: the snapshot transfer and its reply (Raft §7), continuing the sequence.
const TAG_INSTALL_SNAPSHOT: u8 = 8;
const TAG_INSTALL_SNAPSHOT_REPLY: u8 = 9;
/// Format: the fast track's proposal and vote (research record §3.7), continuing the sequence.
const TAG_FAST_PROPOSE: u8 = 10;
const TAG_FAST_VOTE: u8 = 11;

/// Format: an append's fixed bytes before its entries — the tag, six `u64` fields (term, leader, previous
/// index and term, commit index, read context) and the `u32` entry count.
pub const APPEND_HEADER_BYTES: usize = 1 + 6 * size_of::<u64>() + size_of::<u32>();
/// Format: an append's fixed bytes after its priority table — the sync point and the fast track's opening,
/// two `u64` fields.
pub const APPEND_TRAILER_BYTES: usize = 2 * size_of::<u64>();

/// Derived: the entry bytes one append may carry (the `budget` of [`RaftNode::replicate_to`]) so that the
/// whole message — its fixed header ([`APPEND_HEADER_BYTES`]), its priority table (a count and one
/// [`PRIORITY_ROW_BYTES`] row for each of the `voters`), its trailer ([`APPEND_TRAILER_BYTES`]), the
/// `envelope` bytes its sender frames it in, and its entries — fits the credit a fresh session with frames of `frame_cap` bytes grants before any window
/// update (`slates_transport::connection::initial_receive_window`: the reorder threshold plus one packets of
/// stream data, the least credit loss detection needs). A batch within it reaches its follower in one
/// flight on any session, however new; the session's window grows past it on its own, and an entry larger
/// than it still goes, alone.
pub fn append_batch_bytes(frame_cap: usize, envelope: usize, voters: usize) -> usize {
  let table = size_of::<u32>().saturating_add(voters.saturating_mul(PRIORITY_ROW_BYTES));
  usize::try_from(slates_transport::connection::initial_receive_window(
    frame_cap,
  ))
  .unwrap_or(usize::MAX)
  .saturating_sub(APPEND_HEADER_BYTES)
  .saturating_sub(table)
  .saturating_sub(APPEND_TRAILER_BYTES)
  .saturating_sub(envelope)
}

/// Shape: the largest entry count, command length or voter count a decoder accepts before allocating —
/// a hostile datagram cannot force an unbounded allocation. Far above any real Raft batch or fleet size.
const MAX_ITEMS: usize = 1 << 20;

impl RaftMessage {
  /// The member claiming to send this message. The live transport must bind it to the peer
  /// whose session delivered the bytes, including delayed replies from an earlier incarnation.
  pub fn sender(&self) -> HostId {
    match self {
      Self::RequestVote(request) => request.candidate,
      Self::PreVote(request) => request.candidate,
      Self::AppendEntries(append) => append.leader,
      Self::VoteReply(reply) => reply.voter,
      Self::PreVoteReply(reply) => reply.voter,
      Self::AppendReply(reply) => reply.follower,
      Self::TimeoutNow(invitation) => invitation.leader,
      Self::InstallSnapshot(snapshot) => snapshot.leader,
      Self::InstallSnapshotReply(reply) => reply.follower,
      Self::FastPropose(proposal) => proposal.proposer,
      Self::FastVote(vote) => vote.voter,
    }
  }

  /// Decodes a message only when its sender matches the authenticated member.
  pub fn decode_from(bytes: &[u8], peer: HostId) -> Result<Self, RaftWireError> {
    let message = Self::decode(bytes)?;
    if message.sender() != peer {
      return Err(RaftWireError::ForeignSender);
    }
    Ok(message)
  }

  /// The canonical little-endian bytes.
  pub fn encode(&self) -> Vec<u8> {
    let mut out = Vec::new();
    match self {
      RaftMessage::RequestVote(request) => {
        out.push(TAG_REQUEST_VOTE);
        put_u64(&mut out, request.term);
        put_u64(&mut out, request.candidate.0);
        put_u64(&mut out, request.last_log_index);
        put_u64(&mut out, request.last_log_term);
      }
      RaftMessage::VoteReply(reply) => {
        out.push(TAG_VOTE_REPLY);
        put_u64(&mut out, reply.voter.0);
        put_u64(&mut out, reply.term);
        out.push(u8::from(reply.granted));
        put_u32(
          &mut out,
          u32::try_from(reply.reports.len()).unwrap_or(u32::MAX),
        );
        for report in &reply.reports {
          encode_report(&mut out, report);
        }
      }
      RaftMessage::AppendEntries(append) => {
        out.push(TAG_APPEND_ENTRIES);
        put_u64(&mut out, append.term);
        put_u64(&mut out, append.leader.0);
        put_u64(&mut out, append.prev_log_index);
        put_u64(&mut out, append.prev_log_term);
        put_u64(&mut out, append.leader_commit);
        put_u64(&mut out, append.read_context);
        put_u32(
          &mut out,
          u32::try_from(append.entries.len()).unwrap_or(u32::MAX),
        );
        for entry in &append.entries {
          encode_entry(&mut out, entry);
        }
        put_u32(
          &mut out,
          u32::try_from(append.priorities.len()).unwrap_or(u32::MAX),
        );
        for (voter, priority) in &append.priorities {
          put_u64(&mut out, voter.0);
          encode_priority(&mut out, priority);
        }
        put_u64(&mut out, append.sync_index);
        put_u64(&mut out, append.open_from);
      }
      RaftMessage::AppendReply(reply) => {
        out.push(TAG_APPEND_REPLY);
        put_u64(&mut out, reply.follower.0);
        put_u64(&mut out, reply.term);
        out.push(u8::from(reply.success));
        put_u64(&mut out, reply.match_index);
        put_u64(&mut out, reply.read_context);
        put_u64(&mut out, reply.conflict_term);
        put_u64(&mut out, reply.conflict_index);
        encode_priority(&mut out, &reply.priority);
      }
      RaftMessage::PreVote(request) => {
        out.push(TAG_PRE_VOTE);
        put_u64(&mut out, request.term);
        put_u64(&mut out, request.candidate.0);
        put_u64(&mut out, request.last_log_index);
        put_u64(&mut out, request.last_log_term);
      }
      RaftMessage::PreVoteReply(reply) => {
        out.push(TAG_PRE_VOTE_REPLY);
        put_u64(&mut out, reply.voter.0);
        put_u64(&mut out, reply.term);
        out.push(u8::from(reply.granted));
      }
      RaftMessage::TimeoutNow(invitation) => {
        out.push(TAG_TIMEOUT_NOW);
        put_u64(&mut out, invitation.term);
        put_u64(&mut out, invitation.leader.0);
      }
      RaftMessage::InstallSnapshot(snapshot) => {
        out.push(TAG_INSTALL_SNAPSHOT);
        put_u64(&mut out, snapshot.term);
        put_u64(&mut out, snapshot.leader.0);
        put_u64(&mut out, snapshot.last_included_index);
        put_u64(&mut out, snapshot.last_included_term);
        encode_voter_config(&mut out, &snapshot.config);
        put_u32(
          &mut out,
          u32::try_from(snapshot.state.len()).unwrap_or(u32::MAX),
        );
        out.extend_from_slice(&snapshot.state);
      }
      RaftMessage::InstallSnapshotReply(reply) => {
        out.push(TAG_INSTALL_SNAPSHOT_REPLY);
        put_u64(&mut out, reply.follower.0);
        put_u64(&mut out, reply.term);
        put_u64(&mut out, reply.match_index);
      }
      RaftMessage::FastPropose(proposal) => {
        out.push(TAG_FAST_PROPOSE);
        put_u64(&mut out, proposal.term);
        put_u64(&mut out, proposal.proposer.0);
        put_u64(&mut out, proposal.index);
        put_bytes(&mut out, &proposal.command);
      }
      RaftMessage::FastVote(vote) => {
        out.push(TAG_FAST_VOTE);
        put_u64(&mut out, vote.term);
        put_u64(&mut out, vote.voter.0);
        put_u64(&mut out, vote.index);
        put_bytes(&mut out, &vote.command);
      }
    }
    out
  }

  /// Decodes a message, or a typed refusal for hostile or truncated bytes.
  pub fn decode(bytes: &[u8]) -> Result<RaftMessage, RaftWireError> {
    let (&tag, rest) = bytes.split_first().ok_or(RaftWireError::Truncated)?;
    match tag {
      TAG_REQUEST_VOTE => {
        let (term, rest) = take_u64(rest)?;
        let (candidate, rest) = take_u64(rest)?;
        let (last_log_index, rest) = take_u64(rest)?;
        let (last_log_term, rest) = take_u64(rest)?;
        expect_end(rest)?;
        Ok(RaftMessage::RequestVote(RequestVote {
          term,
          candidate: HostId(candidate),
          last_log_index,
          last_log_term,
        }))
      }
      TAG_VOTE_REPLY => {
        let (voter, rest) = take_u64(rest)?;
        let (term, rest) = take_u64(rest)?;
        let (granted, rest) = take_bool(rest)?;
        let (count, mut rest) = take_count(rest)?;
        let mut reports = Vec::with_capacity(count);
        for _ in 0..count {
          let (report, tail) = decode_report(rest)?;
          reports.push(report);
          rest = tail;
        }
        expect_end(rest)?;
        Ok(RaftMessage::VoteReply(VoteReply {
          voter: HostId(voter),
          term,
          granted,
          reports,
        }))
      }
      TAG_APPEND_ENTRIES => {
        let (term, rest) = take_u64(rest)?;
        let (leader, rest) = take_u64(rest)?;
        let (prev_log_index, rest) = take_u64(rest)?;
        let (prev_log_term, rest) = take_u64(rest)?;
        let (leader_commit, rest) = take_u64(rest)?;
        let (read_context, rest) = take_u64(rest)?;
        let (count, mut rest) = take_count(rest)?;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
          let (entry, tail) = decode_entry(rest)?;
          entries.push(entry);
          rest = tail;
        }
        let (count, mut rest) = take_count(rest)?;
        let mut priorities = Vec::with_capacity(count);
        for _ in 0..count {
          let (voter, tail) = take_u64(rest)?;
          let (priority, tail) = decode_priority(tail)?;
          priorities.push((HostId(voter), priority));
          rest = tail;
        }
        let (sync_index, rest) = take_u64(rest)?;
        let (open_from, rest) = take_u64(rest)?;
        expect_end(rest)?;
        Ok(RaftMessage::AppendEntries(AppendEntries {
          read_context,
          term,
          leader: HostId(leader),
          prev_log_index,
          prev_log_term,
          entries,
          leader_commit,
          priorities,
          sync_index,
          open_from,
        }))
      }
      TAG_APPEND_REPLY => {
        let (follower, rest) = take_u64(rest)?;
        let (term, rest) = take_u64(rest)?;
        let (success, rest) = take_bool(rest)?;
        let (match_index, rest) = take_u64(rest)?;
        let (read_context, rest) = take_u64(rest)?;
        let (conflict_term, rest) = take_u64(rest)?;
        let (conflict_index, rest) = take_u64(rest)?;
        let (priority, rest) = decode_priority(rest)?;
        expect_end(rest)?;
        Ok(RaftMessage::AppendReply(AppendReply {
          read_context,
          follower: HostId(follower),
          term,
          success,
          match_index,
          conflict_term,
          conflict_index,
          priority,
        }))
      }
      TAG_PRE_VOTE => {
        let (term, rest) = take_u64(rest)?;
        let (candidate, rest) = take_u64(rest)?;
        let (last_log_index, rest) = take_u64(rest)?;
        let (last_log_term, rest) = take_u64(rest)?;
        expect_end(rest)?;
        Ok(RaftMessage::PreVote(PreVote {
          term,
          candidate: HostId(candidate),
          last_log_index,
          last_log_term,
        }))
      }
      TAG_PRE_VOTE_REPLY => {
        let (voter, rest) = take_u64(rest)?;
        let (term, rest) = take_u64(rest)?;
        let (granted, rest) = take_bool(rest)?;
        expect_end(rest)?;
        Ok(RaftMessage::PreVoteReply(PreVoteReply {
          voter: HostId(voter),
          term,
          granted,
        }))
      }
      TAG_TIMEOUT_NOW => {
        let (term, rest) = take_u64(rest)?;
        let (leader, rest) = take_u64(rest)?;
        expect_end(rest)?;
        Ok(RaftMessage::TimeoutNow(TimeoutNow {
          term,
          leader: HostId(leader),
        }))
      }
      TAG_INSTALL_SNAPSHOT => {
        let (term, rest) = take_u64(rest)?;
        let (leader, rest) = take_u64(rest)?;
        let (last_included_index, rest) = take_u64(rest)?;
        let (last_included_term, rest) = take_u64(rest)?;
        let (config, rest) = decode_voter_config(rest)?;
        let (state, rest) = take_bytes(rest)?;
        expect_end(rest)?;
        Ok(RaftMessage::InstallSnapshot(InstallSnapshot {
          term,
          leader: HostId(leader),
          last_included_index,
          last_included_term,
          config,
          state,
        }))
      }
      TAG_INSTALL_SNAPSHOT_REPLY => {
        let (follower, rest) = take_u64(rest)?;
        let (term, rest) = take_u64(rest)?;
        let (match_index, rest) = take_u64(rest)?;
        expect_end(rest)?;
        Ok(RaftMessage::InstallSnapshotReply(InstallSnapshotReply {
          follower: HostId(follower),
          term,
          match_index,
        }))
      }
      TAG_FAST_PROPOSE => {
        let (term, rest) = take_u64(rest)?;
        let (proposer, rest) = take_u64(rest)?;
        let (index, rest) = take_u64(rest)?;
        let (command, rest) = take_bytes(rest)?;
        expect_end(rest)?;
        Ok(RaftMessage::FastPropose(FastPropose {
          term,
          proposer: HostId(proposer),
          index,
          command,
        }))
      }
      TAG_FAST_VOTE => {
        let (term, rest) = take_u64(rest)?;
        let (voter, rest) = take_u64(rest)?;
        let (index, rest) = take_u64(rest)?;
        let (command, rest) = take_bytes(rest)?;
        expect_end(rest)?;
        Ok(RaftMessage::FastVote(FastVote {
          term,
          voter: HostId(voter),
          index,
          command,
        }))
      }
      other => Err(RaftWireError::UnknownTag { tag: other }),
    }
  }
}

/// Appends a log entry: its term, its command (length-prefixed), then its optional voter configuration.
/// [`LogEntry::encoded_len`] counts exactly these bytes (`an_entry_takes_the_bytes_the_core_counts`).
/// Appends a window slot's report: its index, the term it was accepted in, whether it is a fast vote, then
/// its entry.
fn encode_report(out: &mut Vec<u8>, report: &SlotReport) {
  put_u64(out, report.index);
  put_u64(out, report.slot.term);
  out.push(u8::from(report.slot.fast));
  encode_entry(out, &report.slot.entry);
}

/// Decodes a window slot's report from the front of `bytes`, returning it and the remainder.
fn decode_report(bytes: &[u8]) -> Result<(SlotReport, &[u8]), RaftWireError> {
  let (index, rest) = take_u64(bytes)?;
  let (term, rest) = take_u64(rest)?;
  let (fast, rest) = take_bool(rest)?;
  let (entry, rest) = decode_entry(rest)?;
  Ok((
    SlotReport {
      index,
      slot: WindowSlot { term, fast, entry },
    },
    rest,
  ))
}

fn encode_entry(out: &mut Vec<u8>, entry: &LogEntry) {
  put_u64(out, entry.term);
  put_u32(out, u32::try_from(entry.command.len()).unwrap_or(u32::MAX));
  out.extend_from_slice(&entry.command);
  match &entry.config {
    None => out.push(0),
    Some(config) => {
      out.push(1);
      encode_voter_config(out, config);
    }
  }
}

/// Appends an election priority: its quorum round trip, then its spread, in nanoseconds.
fn encode_priority(out: &mut Vec<u8>, priority: &ElectionPriority) {
  put_u64(out, priority.quorum_ns);
  put_u64(out, priority.spread_ns);
}

/// Decodes an election priority from the front of `bytes`, returning it and the remainder.
fn decode_priority(bytes: &[u8]) -> Result<(ElectionPriority, &[u8]), RaftWireError> {
  let (quorum_ns, rest) = take_u64(bytes)?;
  let (spread_ns, rest) = take_u64(rest)?;
  Ok((
    ElectionPriority {
      quorum_ns,
      spread_ns,
    },
    rest,
  ))
}

/// Format: one row of an append's priority table — the voter, then its round trip and spread.
pub const PRIORITY_ROW_BYTES: usize = 3 * size_of::<u64>();

/// Appends a voter configuration: the voter set, then the joint set's presence byte and set.
fn encode_voter_config(out: &mut Vec<u8>, config: &VoterConfig) {
  encode_hosts(out, &config.voters);
  match &config.joint {
    None => out.push(0),
    Some(joint) => {
      out.push(1);
      encode_hosts(out, joint);
    }
  }
}

/// Decodes a voter configuration from the front of `bytes`, returning it and the remainder.
fn decode_voter_config(bytes: &[u8]) -> Result<(VoterConfig, &[u8]), RaftWireError> {
  let (voters, rest) = decode_hosts(bytes)?;
  let (has_joint, rest) = take_bool(rest)?;
  let (joint, rest) = if has_joint {
    let (joint, rest) = decode_hosts(rest)?;
    (Some(joint), rest)
  } else {
    (None, rest)
  };
  Ok((VoterConfig { voters, joint }, rest))
}

/// Decodes a log entry from the front of `bytes`, returning it and the remainder.
fn decode_entry(bytes: &[u8]) -> Result<(LogEntry, &[u8]), RaftWireError> {
  let (term, rest) = take_u64(bytes)?;
  let (command, rest) = take_bytes(rest)?;
  let (has_config, rest) = take_bool(rest)?;
  if !has_config {
    return Ok((LogEntry::command(term, command), rest));
  }
  let (config, rest) = decode_voter_config(rest)?;
  Ok((
    LogEntry {
      term,
      command,
      config: Some(config),
    },
    rest,
  ))
}

/// Appends a host-id set: its count then each id.
fn encode_hosts(out: &mut Vec<u8>, hosts: &[HostId]) {
  put_u32(out, u32::try_from(hosts.len()).unwrap_or(u32::MAX));
  for host in hosts {
    put_u64(out, host.0);
  }
}

/// Decodes a host-id set (count then each id), bounding the count before allocating.
fn decode_hosts(bytes: &[u8]) -> Result<(Vec<HostId>, &[u8]), RaftWireError> {
  let (count, mut rest) = take_count(bytes)?;
  let mut hosts = Vec::with_capacity(count);
  for _ in 0..count {
    let (host, tail) = take_u64(rest)?;
    hosts.push(HostId(host));
    rest = tail;
  }
  Ok((hosts, rest))
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
  out.extend_from_slice(&value.to_le_bytes());
}

/// Appends a length-prefixed byte string (the length a little-endian `u32`), which [`take_bytes`] reads.
fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
  put_u32(out, u32::try_from(bytes.len()).unwrap_or(u32::MAX));
  out.extend_from_slice(bytes);
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
  out.extend_from_slice(&value.to_le_bytes());
}

fn take_u64(bytes: &[u8]) -> Result<(u64, &[u8]), RaftWireError> {
  if bytes.len() < size_of::<u64>() {
    return Err(RaftWireError::Truncated);
  }
  let (head, rest) = bytes.split_at(size_of::<u64>());
  let mut word = [0u8; size_of::<u64>()];
  word.copy_from_slice(head);
  Ok((u64::from_le_bytes(word), rest))
}

fn take_u32(bytes: &[u8]) -> Result<(u32, &[u8]), RaftWireError> {
  if bytes.len() < size_of::<u32>() {
    return Err(RaftWireError::Truncated);
  }
  let (head, rest) = bytes.split_at(size_of::<u32>());
  let mut word = [0u8; size_of::<u32>()];
  word.copy_from_slice(head);
  Ok((u32::from_le_bytes(word), rest))
}

/// Reads a u32 count and refuses one larger than [`MAX_ITEMS`] (bounding allocation before it happens).
fn take_count(bytes: &[u8]) -> Result<(usize, &[u8]), RaftWireError> {
  let (count, rest) = take_u32(bytes)?;
  let count = usize::try_from(count).unwrap_or(usize::MAX);
  if count > MAX_ITEMS || count > rest.len() {
    // Every item is at least one byte, so a count beyond the remaining bytes cannot be backed.
    return Err(RaftWireError::LengthMismatch);
  }
  Ok((count, rest))
}

fn take_bool(bytes: &[u8]) -> Result<(bool, &[u8]), RaftWireError> {
  let (&flag, rest) = bytes.split_first().ok_or(RaftWireError::Truncated)?;
  match flag {
    0 => Ok((false, rest)),
    1 => Ok((true, rest)),
    other => Err(RaftWireError::BadFlag { flag: other }),
  }
}

/// Reads a length-prefixed byte string, bounding the length against what arrived before allocating.
fn take_bytes(bytes: &[u8]) -> Result<(Vec<u8>, &[u8]), RaftWireError> {
  let (length, rest) = take_u32(bytes)?;
  let length = usize::try_from(length).unwrap_or(usize::MAX);
  if length > rest.len() {
    return Err(RaftWireError::LengthMismatch);
  }
  let (head, tail) = rest.split_at(length);
  Ok((head.to_vec(), tail))
}

/// Refuses trailing bytes after a message (a truncated or over-long datagram).
fn expect_end(rest: &[u8]) -> Result<(), RaftWireError> {
  if rest.is_empty() {
    Ok(())
  } else {
    Err(RaftWireError::LengthMismatch)
  }
}

/// Format: the Raft RPC's request **kind** (the low bits of each exchange's fresh stream id); the server's
/// `serve_once` serves it whatever the sequence above, so the value is a label, not a tunable. Raft RPCs
/// ride the `Control` class.
const RAFT_STREAM: u64 = 1;

/// Serves one incoming Raft request on `node` over `endpoint` (§4.8): a received vote request or append
/// is run through the node's handler and the reply is sent back. A reply-typed or malformed request is
/// answered with nothing (the requester counts it as no reply). The caller loops this to keep serving.
pub async fn serve_raft_once(
  endpoint: &mut Endpoint,
  node: &mut RaftNode,
) -> Result<(), EndpointError> {
  endpoint
    .serve_once(|_, request| match RaftMessage::decode(&request) {
      Ok(RaftMessage::PreVote(pre)) => RaftMessage::PreVoteReply(node.on_pre_vote(pre)).encode(),
      Ok(RaftMessage::RequestVote(vote)) => {
        RaftMessage::VoteReply(node.on_request_vote(vote)).encode()
      }
      Ok(RaftMessage::AppendEntries(append)) => {
        RaftMessage::AppendReply(node.on_append_entries(append)).encode()
      }
      Ok(RaftMessage::InstallSnapshot(snapshot)) => {
        RaftMessage::InstallSnapshotReply(node.on_install_snapshot(snapshot)).encode()
      }
      _ => Vec::new(),
    })
    .await
}

/// Sends one Raft `message` over `endpoint` and returns the decoded reply, or `None` if the reply is
/// missing or malformed. The caller (a candidate or leader) feeds the reply back to its node
/// (`on_vote_reply`/`on_append_reply`).
pub async fn request_raft(
  endpoint: &mut Endpoint,
  message: &RaftMessage,
) -> Result<Option<RaftMessage>, EndpointError> {
  let reply = endpoint
    .request(RAFT_STREAM, Priority::Control, &message.encode())
    .await?;
  Ok(RaftMessage::decode(&reply).ok())
}

/// Encodes a [`RegionalConfiguration`] for the wire — a **learner** fetches it from a council voter to catch
/// up on the committed configuration (§4.8, D-14: the council is a small elected set but the region is large,
/// so a non-voter member *learns* the configuration rather than voting on it). Little-endian throughout,
/// each map as a count then its entries, so two nodes encode it identically.
pub fn encode_regional_configuration(config: &RegionalConfiguration) -> Vec<u8> {
  let mut out = Vec::new();
  put_u64(&mut out, config.version);
  put_u32(&mut out, config.quorum.f);
  out.push(u8::from(config.has_mirror));
  encode_hosts(&mut out, &config.members);
  put_u32(
    &mut out,
    u32::try_from(config.neighbourhoods.len()).unwrap_or(u32::MAX),
  );
  for (owner, neighbourhood) in &config.neighbourhoods {
    put_u64(&mut out, owner.0);
    put_u64(&mut out, neighbourhood.generation);
    encode_hosts(&mut out, &neighbourhood.hosts);
  }
  put_u32(
    &mut out,
    u32::try_from(config.epochs.len()).unwrap_or(u32::MAX),
  );
  for (host, epoch) in &config.epochs {
    put_u64(&mut out, host.0);
    put_u64(&mut out, epoch.0);
  }
  encode_domains(&mut out, &config.domains);
  put_u32(
    &mut out,
    u32::try_from(config.settled.len()).unwrap_or(u32::MAX),
  );
  for (owner, settled) in &config.settled {
    put_u64(&mut out, owner.0);
    encode_settled(&mut out, settled);
  }
  put_u32(
    &mut out,
    u32::try_from(config.retired.len()).unwrap_or(u32::MAX),
  );
  for (host, retirement) in &config.retired {
    put_u64(&mut out, host.0);
    put_u64(&mut out, retirement.version);
    encode_settled(&mut out, &retirement.settled);
    encode_hosts(&mut out, &retirement.confirmed);
    encode_hosts(&mut out, &retirement.unconfirmed);
  }
  out
}

/// Encodes a domains map: its count, then each `(host, domain)`.
fn encode_domains(out: &mut Vec<u8>, domains: &std::collections::BTreeMap<HostId, DomainId>) {
  put_u32(out, u32::try_from(domains.len()).unwrap_or(u32::MAX));
  for (host, domain) in domains {
    put_u64(out, host.0);
    put_u64(out, *domain);
  }
}

/// Encodes a settled neighbourhood: its generation, its hosts, then its hosts' declared domains.
fn encode_settled(out: &mut Vec<u8>, settled: &Settled) {
  put_u64(out, settled.generation);
  encode_hosts(out, &settled.hosts);
  encode_domains(out, &settled.domains);
}

/// Decodes a settled neighbourhood ([`encode_settled`]), bounding every count.
fn decode_settled(bytes: &[u8]) -> Result<(Settled, &[u8]), RaftWireError> {
  let (generation, rest) = take_u64(bytes)?;
  let (hosts, rest) = decode_hosts(rest)?;
  let (domains, rest) = decode_domains(rest)?;
  Ok((
    Settled {
      hosts,
      generation,
      domains,
    },
    rest,
  ))
}

/// Decodes the settled map (count, then each `(owner, settled)`), bounding the count.
fn decode_settled_map(
  bytes: &[u8],
) -> Result<(std::collections::BTreeMap<HostId, Settled>, &[u8]), RaftWireError> {
  let (count, mut rest) = take_count(bytes)?;
  let mut settled = std::collections::BTreeMap::new();
  for _ in 0..count {
    let (owner, tail) = take_u64(rest)?;
    let (neighbourhood, tail) = decode_settled(tail)?;
    settled.insert(HostId(owner), neighbourhood);
    rest = tail;
  }
  Ok((settled, rest))
}

/// Decodes the retirements map (count, then each `(host, version, settled, confirmed, unconfirmed)`), bounding
/// every count.
fn decode_retired(
  bytes: &[u8],
) -> Result<(std::collections::BTreeMap<HostId, Retirement>, &[u8]), RaftWireError> {
  let (count, mut rest) = take_count(bytes)?;
  let mut retired = std::collections::BTreeMap::new();
  for _ in 0..count {
    let (host, tail) = take_u64(rest)?;
    let (version, tail) = take_u64(tail)?;
    let (settled, tail) = decode_settled(tail)?;
    let (confirmed, tail) = decode_hosts(tail)?;
    let (unconfirmed, tail) = decode_hosts(tail)?;
    retired.insert(
      HostId(host),
      Retirement {
        version,
        settled,
        confirmed,
        unconfirmed,
      },
    );
    rest = tail;
  }
  Ok((retired, rest))
}

/// Decodes a [`RegionalConfiguration`], or a typed refusal for hostile or truncated bytes — every declared
/// count is bounded against the bytes that arrived before allocating (a lying count is
/// [`RaftWireError::LengthMismatch`]), so a hostile datagram cannot force an over-allocation or a panic.
pub fn decode_regional_configuration(bytes: &[u8]) -> Result<RegionalConfiguration, RaftWireError> {
  let (version, rest) = take_u64(bytes)?;
  let (f, rest) = take_u32(rest)?;
  let (has_mirror, rest) = take_bool(rest)?;
  let (members, rest) = decode_hosts(rest)?;
  let (neighbourhoods, rest) = decode_neighbourhoods(rest)?;
  let (epochs, rest) = decode_epochs(rest)?;
  let (domains, rest) = decode_domains(rest)?;
  let (settled, rest) = decode_settled_map(rest)?;
  let (retired, rest) = decode_retired(rest)?;
  expect_end(rest)?;
  Ok(RegionalConfiguration {
    version,
    members,
    neighbourhoods,
    epochs,
    domains,
    quorum: Quorum { f },
    has_mirror,
    settled,
    retired,
  })
}

/// Decodes the neighbourhoods map (count, then each `(owner, generation, hosts)`), bounding the count.
fn decode_neighbourhoods(
  bytes: &[u8],
) -> Result<(std::collections::BTreeMap<HostId, Neighbourhood>, &[u8]), RaftWireError> {
  let (count, mut rest) = take_count(bytes)?;
  let mut neighbourhoods = std::collections::BTreeMap::new();
  for _ in 0..count {
    let (owner, tail) = take_u64(rest)?;
    let (generation, tail) = take_u64(tail)?;
    let (hosts, tail) = decode_hosts(tail)?;
    neighbourhoods.insert(HostId(owner), Neighbourhood { hosts, generation });
    rest = tail;
  }
  Ok((neighbourhoods, rest))
}

/// Decodes the epochs map (count, then each `(host, epoch)`), bounding the count.
fn decode_epochs(
  bytes: &[u8],
) -> Result<(std::collections::BTreeMap<HostId, HostEpoch>, &[u8]), RaftWireError> {
  let (count, mut rest) = take_count(bytes)?;
  let mut epochs = std::collections::BTreeMap::new();
  for _ in 0..count {
    let (host, tail) = take_u64(rest)?;
    let (epoch, tail) = take_u64(tail)?;
    epochs.insert(HostId(host), HostEpoch(epoch));
    rest = tail;
  }
  Ok((epochs, rest))
}

/// Decodes the domains map (count, then each `(host, domain)`), bounding the count.
fn decode_domains(
  bytes: &[u8],
) -> Result<(std::collections::BTreeMap<HostId, DomainId>, &[u8]), RaftWireError> {
  let (count, mut rest) = take_count(bytes)?;
  let mut domains = std::collections::BTreeMap::new();
  for _ in 0..count {
    let (host, tail) = take_u64(rest)?;
    let (domain, tail) = take_u64(tail)?;
    domains.insert(HostId(host), domain);
    rest = tail;
  }
  Ok((domains, rest))
}

/// Encodes a [`RootConfiguration`] for the wire — the cross-region counterpart of
/// [`encode_regional_configuration`], carried when a region catches up on the root group's committed state
/// (§4.8, D-14). Little-endian throughout, each collection as a count then its entries, so two nodes encode
/// it identically: version, the regions, the moved homes (`(volume, region)` each), and the promotions
/// (`(lost, mirror)` each).
pub fn encode_root_configuration(config: &RootConfiguration) -> Vec<u8> {
  let mut out = Vec::new();
  put_u64(&mut out, config.version);
  put_u32(
    &mut out,
    u32::try_from(config.regions.len()).unwrap_or(u32::MAX),
  );
  for region in &config.regions {
    put_u64(&mut out, region.0);
  }
  put_u32(
    &mut out,
    u32::try_from(config.homes.len()).unwrap_or(u32::MAX),
  );
  for (volume, region) in &config.homes {
    out.extend_from_slice(&volume.0);
    put_u64(&mut out, region.0);
  }
  put_u32(
    &mut out,
    u32::try_from(config.promotions.len()).unwrap_or(u32::MAX),
  );
  for (lost, mirror) in &config.promotions {
    put_u64(&mut out, lost.0);
    put_u64(&mut out, mirror.0);
  }
  out
}

/// Decodes a [`RootConfiguration`], or a typed refusal for hostile or truncated bytes — every declared count
/// is bounded against the bytes that arrived before allocating (a lying count is
/// [`RaftWireError::LengthMismatch`]), so a hostile datagram cannot force an over-allocation or a panic.
pub fn decode_root_configuration(bytes: &[u8]) -> Result<RootConfiguration, RaftWireError> {
  let (version, rest) = take_u64(bytes)?;
  let (regions, rest) = decode_regions(rest)?;
  let (homes, rest) = decode_homes(rest)?;
  let (promotions, rest) = decode_promotions(rest)?;
  expect_end(rest)?;
  Ok(RootConfiguration {
    version,
    regions,
    homes,
    promotions,
  })
}

/// Decodes the regions list (count, then each region id), bounding the count.
fn decode_regions(bytes: &[u8]) -> Result<(Vec<RegionId>, &[u8]), RaftWireError> {
  let (count, mut rest) = take_count(bytes)?;
  let mut regions = Vec::with_capacity(count);
  for _ in 0..count {
    let (region, tail) = take_u64(rest)?;
    regions.push(RegionId(region));
    rest = tail;
  }
  Ok((regions, rest))
}

/// Decodes the moved-homes map (count, then each `(volume, region)`), bounding the count.
fn decode_homes(
  bytes: &[u8],
) -> Result<(std::collections::BTreeMap<ObjectId, RegionId>, &[u8]), RaftWireError> {
  let (count, mut rest) = take_count(bytes)?;
  let mut homes = std::collections::BTreeMap::new();
  for _ in 0..count {
    let (volume, tail) = take_object(rest)?;
    let (region, tail) = take_u64(tail)?;
    homes.insert(volume, RegionId(region));
    rest = tail;
  }
  Ok((homes, rest))
}

/// Decodes the promotions map (count, then each `(lost, mirror)`), bounding the count.
fn decode_promotions(
  bytes: &[u8],
) -> Result<(std::collections::BTreeMap<RegionId, RegionId>, &[u8]), RaftWireError> {
  let (count, mut rest) = take_count(bytes)?;
  let mut promotions = std::collections::BTreeMap::new();
  for _ in 0..count {
    let (lost, tail) = take_u64(rest)?;
    let (mirror, tail) = take_u64(tail)?;
    promotions.insert(RegionId(lost), RegionId(mirror));
    rest = tail;
  }
  Ok((promotions, rest))
}

/// Reads an object id at the front of `bytes`, returning it and the remainder, or `Truncated`.
fn take_object(bytes: &[u8]) -> Result<(ObjectId, &[u8]), RaftWireError> {
  if bytes.len() < OBJECT_BYTES {
    return Err(RaftWireError::Truncated);
  }
  let (head, rest) = bytes.split_at(OBJECT_BYTES);
  let mut id = [0u8; OBJECT_BYTES];
  id.copy_from_slice(head);
  Ok((ObjectId(id), rest))
}

#[cfg(test)]
mod tests {
  use super::*;

  const A: HostId = HostId(1);
  const B: HostId = HostId(2);

  fn append_with_entries() -> RaftMessage {
    RaftMessage::AppendEntries(AppendEntries {
      read_context: 42,
      term: 5,
      leader: A,
      prev_log_index: 3,
      prev_log_term: 4,
      entries: vec![
        LogEntry::command(5, b"cmd".to_vec()),
        LogEntry::configuration(
          5,
          VoterConfig {
            voters: vec![A, B],
            joint: Some(vec![B, HostId(3)]),
          },
        ),
      ],
      leader_commit: 2,
      priorities: Vec::new(),
      sync_index: 0,
      open_from: 0,
    })
  }

  fn snapshot_with_joint_config() -> RaftMessage {
    RaftMessage::InstallSnapshot(InstallSnapshot {
      term: 9,
      leader: A,
      last_included_index: 7,
      last_included_term: 8,
      config: VoterConfig {
        voters: vec![A, B],
        joint: Some(vec![B, HostId(3)]),
      },
      state: b"configuration".to_vec(),
    })
  }

  /// Every message kind round-trips through encode/decode unchanged — including an append carrying a
  /// command entry and a configuration entry with a joint voter set, a refusal carrying its conflict hint,
  /// and a snapshot whose configuration is joint.
  #[test]
  fn every_message_round_trips() {
    let messages = [
      RaftMessage::RequestVote(RequestVote {
        term: 7,
        candidate: A,
        last_log_index: 4,
        last_log_term: 3,
      }),
      RaftMessage::VoteReply(VoteReply {
        voter: B,
        term: 7,
        granted: true,
        reports: Vec::new(),
      }),
      append_with_entries(),
      RaftMessage::AppendReply(AppendReply {
        read_context: 0,
        follower: B,
        term: 5,
        success: true,
        match_index: 4,
        conflict_term: 0,
        conflict_index: 0,
        priority: ElectionPriority::default(),
      }),
      RaftMessage::PreVote(PreVote {
        term: 8,
        candidate: A,
        last_log_index: 4,
        last_log_term: 3,
      }),
      RaftMessage::PreVoteReply(PreVoteReply {
        voter: B,
        term: 8,
        granted: false,
      }),
      RaftMessage::TimeoutNow(TimeoutNow { term: 9, leader: A }),
      RaftMessage::AppendReply(AppendReply {
        read_context: 3,
        follower: B,
        term: 5,
        success: false,
        match_index: 0,
        conflict_term: 4,
        conflict_index: 2,
        priority: ElectionPriority::default(),
      }),
      snapshot_with_joint_config(),
      RaftMessage::InstallSnapshotReply(InstallSnapshotReply {
        follower: B,
        term: 9,
        match_index: 7,
      }),
      fast_propose(),
      RaftMessage::FastVote(FastVote {
        term: 6,
        voter: B,
        index: 12,
        command: b"cmd".to_vec(),
      }),
    ];
    for message in messages {
      let bytes = message.encode();
      assert_eq!(
        RaftMessage::decode_from(&bytes, message.sender()),
        Ok(message.clone())
      );
      assert_eq!(
        RaftMessage::decode_from(&bytes, HostId(message.sender().0 ^ u64::MAX)),
        Err(RaftWireError::ForeignSender),
        "a session cannot speak for another voter"
      );
      assert_eq!(
        RaftMessage::decode(&bytes),
        Ok(message),
        "round-trip is identity"
      );
    }
  }

  /// The encoding is fixed and little-endian: a golden vector per message kind pins it, spelled out byte by
  /// byte rather than produced by the encoder, with every field a distinct value, so a change of field
  /// order, width or byte order fails here even when encode and decode change together (a round trip alone
  /// cannot see that).
  #[test]
  fn every_message_has_a_golden_encoding() {
    let golden: [(RaftMessage, Vec<u8>); 11] = [
      (
        RaftMessage::RequestVote(RequestVote {
          term: 7,
          candidate: HostId(1),
          last_log_index: 4,
          last_log_term: 3,
        }),
        [
          &[TAG_REQUEST_VOTE][..],
          &[7, 0, 0, 0, 0, 0, 0, 0],
          &[1, 0, 0, 0, 0, 0, 0, 0],
          &[4, 0, 0, 0, 0, 0, 0, 0],
          &[3, 0, 0, 0, 0, 0, 0, 0],
        ]
        .concat(),
      ),
      (
        RaftMessage::VoteReply(VoteReply {
          voter: HostId(2),
          term: 7,
          granted: true,
          reports: vec![SlotReport {
            index: 0x11,
            slot: WindowSlot {
              term: 0x12,
              fast: true,
              entry: LogEntry::command(0x12, b"fv".to_vec()),
            },
          }],
        }),
        [
          &[TAG_VOTE_REPLY][..],
          &[2, 0, 0, 0, 0, 0, 0, 0],
          &[7, 0, 0, 0, 0, 0, 0, 0],
          &[1],
          &[1, 0, 0, 0],                // reports
          &[0x11, 0, 0, 0, 0, 0, 0, 0], // its index
          &[0x12, 0, 0, 0, 0, 0, 0, 0], // the term it was accepted in
          &[1],                         // a fast vote
          &[0x12, 0, 0, 0, 0, 0, 0, 0], // its entry: term
          &[2, 0, 0, 0],                // command length
          b"fv",
          &[0], // no configuration
        ]
        .concat(),
      ),
      (
        RaftMessage::AppendEntries(AppendEntries {
          read_context: 9,
          term: 5,
          leader: HostId(1),
          prev_log_index: 3,
          prev_log_term: 4,
          entries: vec![
            LogEntry::command(5, b"cm".to_vec()),
            LogEntry::configuration(
              6,
              VoterConfig {
                voters: vec![HostId(1), HostId(2)],
                joint: None,
              },
            ),
          ],
          leader_commit: 2,
          priorities: vec![(
            HostId(2),
            ElectionPriority {
              quorum_ns: 0x21,
              spread_ns: 0x22,
            },
          )],
          sync_index: 0x23,
          open_from: 0x24,
        }),
        [
          &[TAG_APPEND_ENTRIES][..],
          &[5, 0, 0, 0, 0, 0, 0, 0], // term
          &[1, 0, 0, 0, 0, 0, 0, 0], // leader
          &[3, 0, 0, 0, 0, 0, 0, 0], // prev_log_index
          &[4, 0, 0, 0, 0, 0, 0, 0], // prev_log_term
          &[2, 0, 0, 0, 0, 0, 0, 0], // leader_commit
          &[9, 0, 0, 0, 0, 0, 0, 0], // read_context
          &[2, 0, 0, 0],             // entry count
          &[5, 0, 0, 0, 0, 0, 0, 0], // entry 1: term
          &[2, 0, 0, 0],             // command length
          b"cm",
          &[0],                      // no configuration
          &[6, 0, 0, 0, 0, 0, 0, 0], // entry 2: term
          &[0, 0, 0, 0],             // empty command
          &[1],                      // a configuration
          &[2, 0, 0, 0],             // voter count
          &[1, 0, 0, 0, 0, 0, 0, 0],
          &[2, 0, 0, 0, 0, 0, 0, 0],
          &[0],                         // no joint set
          &[1, 0, 0, 0],                // priority rows
          &[2, 0, 0, 0, 0, 0, 0, 0],    // voter
          &[0x21, 0, 0, 0, 0, 0, 0, 0], // its quorum round trip
          &[0x22, 0, 0, 0, 0, 0, 0, 0], // its spread
          &[0x23, 0, 0, 0, 0, 0, 0, 0], // the sync point
          &[0x24, 0, 0, 0, 0, 0, 0, 0], // the fast track's opening
        ]
        .concat(),
      ),
      (
        RaftMessage::AppendReply(AppendReply {
          read_context: 6,
          follower: HostId(2),
          term: 5,
          success: false,
          match_index: 4,
          conflict_term: 0x0a,
          conflict_index: 0x0b,
          priority: ElectionPriority {
            quorum_ns: 0x0c,
            spread_ns: 0x0d,
          },
        }),
        [
          &[TAG_APPEND_REPLY][..],
          &[2, 0, 0, 0, 0, 0, 0, 0],    // follower
          &[5, 0, 0, 0, 0, 0, 0, 0],    // term
          &[0],                         // success
          &[4, 0, 0, 0, 0, 0, 0, 0],    // match_index
          &[6, 0, 0, 0, 0, 0, 0, 0],    // read_context
          &[0x0a, 0, 0, 0, 0, 0, 0, 0], // conflict_term
          &[0x0b, 0, 0, 0, 0, 0, 0, 0], // conflict_index
          &[0x0c, 0, 0, 0, 0, 0, 0, 0], // priority: quorum round trip
          &[0x0d, 0, 0, 0, 0, 0, 0, 0], // priority: spread
        ]
        .concat(),
      ),
      (
        RaftMessage::PreVote(PreVote {
          term: 8,
          candidate: HostId(1),
          last_log_index: 4,
          last_log_term: 3,
        }),
        [
          &[TAG_PRE_VOTE][..],
          &[8, 0, 0, 0, 0, 0, 0, 0],
          &[1, 0, 0, 0, 0, 0, 0, 0],
          &[4, 0, 0, 0, 0, 0, 0, 0],
          &[3, 0, 0, 0, 0, 0, 0, 0],
        ]
        .concat(),
      ),
      (
        RaftMessage::PreVoteReply(PreVoteReply {
          voter: HostId(2),
          term: 8,
          granted: false,
        }),
        [
          &[TAG_PRE_VOTE_REPLY][..],
          &[2, 0, 0, 0, 0, 0, 0, 0],
          &[8, 0, 0, 0, 0, 0, 0, 0],
          &[0],
        ]
        .concat(),
      ),
      (
        RaftMessage::TimeoutNow(TimeoutNow {
          term: 0x0102_0304_0506_0708,
          leader: HostId(0x1112_1314_1516_1718),
        }),
        [
          &[TAG_TIMEOUT_NOW][..],
          &[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01],
          &[0x18, 0x17, 0x16, 0x15, 0x14, 0x13, 0x12, 0x11],
        ]
        .concat(),
      ),
      (
        RaftMessage::InstallSnapshot(InstallSnapshot {
          term: 9,
          leader: HostId(1),
          last_included_index: 7,
          last_included_term: 8,
          config: VoterConfig {
            voters: vec![HostId(1)],
            joint: Some(vec![HostId(2)]),
          },
          state: b"st".to_vec(),
        }),
        [
          &[TAG_INSTALL_SNAPSHOT][..],
          &[9, 0, 0, 0, 0, 0, 0, 0], // term
          &[1, 0, 0, 0, 0, 0, 0, 0], // leader
          &[7, 0, 0, 0, 0, 0, 0, 0], // last_included_index
          &[8, 0, 0, 0, 0, 0, 0, 0], // last_included_term
          &[1, 0, 0, 0],             // voter count
          &[1, 0, 0, 0, 0, 0, 0, 0],
          &[1],          // a joint set
          &[1, 0, 0, 0], // joint count
          &[2, 0, 0, 0, 0, 0, 0, 0],
          &[2, 0, 0, 0], // state length
          b"st",
        ]
        .concat(),
      ),
      (
        RaftMessage::InstallSnapshotReply(InstallSnapshotReply {
          follower: HostId(2),
          term: 9,
          match_index: 7,
        }),
        [
          &[TAG_INSTALL_SNAPSHOT_REPLY][..],
          &[2, 0, 0, 0, 0, 0, 0, 0],
          &[9, 0, 0, 0, 0, 0, 0, 0],
          &[7, 0, 0, 0, 0, 0, 0, 0],
        ]
        .concat(),
      ),
      (
        RaftMessage::FastPropose(FastPropose {
          term: 0x31,
          proposer: HostId(3),
          index: 0x32,
          command: b"fp".to_vec(),
        }),
        [
          &[TAG_FAST_PROPOSE][..],
          &[0x31, 0, 0, 0, 0, 0, 0, 0], // term
          &[3, 0, 0, 0, 0, 0, 0, 0],    // proposer
          &[0x32, 0, 0, 0, 0, 0, 0, 0], // index
          &[2, 0, 0, 0],                // command length
          b"fp",
        ]
        .concat(),
      ),
      (
        RaftMessage::FastVote(FastVote {
          term: 0x41,
          voter: HostId(4),
          index: 0x42,
          command: b"fv".to_vec(),
        }),
        [
          &[TAG_FAST_VOTE][..],
          &[0x41, 0, 0, 0, 0, 0, 0, 0], // term
          &[4, 0, 0, 0, 0, 0, 0, 0],    // voter
          &[0x42, 0, 0, 0, 0, 0, 0, 0], // index
          &[2, 0, 0, 0],                // command length
          b"fv",
        ]
        .concat(),
      ),
    ];
    for (message, bytes) in golden {
      assert_eq!(
        message.encode(),
        bytes,
        "{message:?} encodes to its golden bytes"
      );
      assert_eq!(
        RaftMessage::decode(&bytes),
        Ok(message),
        "the golden bytes decode"
      );
    }
  }

  /// A fast proposal every test of the fast track's wire uses.
  fn fast_propose() -> RaftMessage {
    RaftMessage::FastPropose(FastPropose {
      term: 6,
      proposer: A,
      index: 12,
      command: b"cmd".to_vec(),
    })
  }

  /// Hostile input on the fast track's messages and a vote's reports (research record §3.7, §4): every
  /// truncation of a valid encoding is refused `Truncated` or `LengthMismatch`, a trailing byte is
  /// `LengthMismatch`, a command length reaching past the bytes is `LengthMismatch` before any allocation, a
  /// report count past the bytes too, a report's fast flag that is neither zero nor one is `BadFlag`, and a
  /// session cannot propose or vote for another member.
  #[test]
  fn hostile_fast_track_messages_are_refused() {
    let vote_reply = RaftMessage::VoteReply(VoteReply {
      voter: B,
      term: 7,
      granted: true,
      reports: vec![SlotReport {
        index: 3,
        slot: WindowSlot {
          term: 7,
          fast: true,
          entry: LogEntry::command(7, b"x".to_vec()),
        },
      }],
    });
    for message in [fast_propose(), vote_reply.clone()] {
      let bytes = message.encode();
      for cut in 1..bytes.len() {
        assert!(
          matches!(
            RaftMessage::decode(&bytes[..cut]),
            Err(RaftWireError::Truncated | RaftWireError::LengthMismatch)
          ),
          "{message:?} cut at {cut}"
        );
      }
      let mut long = bytes.clone();
      long.push(0);
      assert_eq!(
        RaftMessage::decode(&long),
        Err(RaftWireError::LengthMismatch)
      );
      assert_eq!(
        RaftMessage::decode_from(&bytes, HostId(message.sender().0 ^ u64::MAX)),
        Err(RaftWireError::ForeignSender)
      );
    }
    // The command length is the last `u32` before the command's three bytes.
    let mut lying = fast_propose().encode();
    let length_at = lying.len() - 3 - size_of::<u32>();
    lying[length_at..length_at + size_of::<u32>()].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
      RaftMessage::decode(&lying),
      Err(RaftWireError::LengthMismatch)
    );
    // The report count follows the tag, the voter, the term and the granted flag.
    let count_at = 1 + 2 * size_of::<u64>() + 1;
    let mut counted = vote_reply.encode();
    counted[count_at..count_at + size_of::<u32>()].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
      RaftMessage::decode(&counted),
      Err(RaftWireError::LengthMismatch)
    );
    // The report's fast flag follows its index and term.
    let flag_at = count_at + size_of::<u32>() + 2 * size_of::<u64>();
    let mut flagged = vote_reply.encode();
    flagged[flag_at] = 2;
    assert_eq!(
      RaftMessage::decode(&flagged),
      Err(RaftWireError::BadFlag { flag: 2 })
    );
  }

  /// Hostile input on the leadership-transfer invitation: every truncation of a valid encoding is refused
  /// `Truncated`, a trailing byte `LengthMismatch`, and a session cannot invite on another leader's behalf.
  #[test]
  fn a_hostile_timeout_now_is_refused() {
    let bytes = RaftMessage::TimeoutNow(TimeoutNow { term: 3, leader: A }).encode();
    for cut in 1..bytes.len() {
      assert_eq!(
        RaftMessage::decode(&bytes[..cut]),
        Err(RaftWireError::Truncated),
        "cut at {cut}"
      );
    }
    let mut long = bytes.clone();
    long.push(0);
    assert_eq!(
      RaftMessage::decode(&long),
      Err(RaftWireError::LengthMismatch)
    );
    assert_eq!(
      RaftMessage::decode_from(&bytes, B),
      Err(RaftWireError::ForeignSender)
    );
  }

  /// Hostile input on the snapshot transfer (Raft §7): every truncation of a valid encoding is refused
  /// `Truncated` or `LengthMismatch` (a cut inside a declared count or the state), a trailing byte is
  /// `LengthMismatch`, a state length reaching past the bytes is `LengthMismatch` before any allocation, a
  /// joint flag that is neither zero nor one is `BadFlag`, and a session cannot ship a snapshot, or answer
  /// one, for another member.
  #[test]
  fn a_hostile_snapshot_is_refused() {
    let snapshot = snapshot_with_joint_config();
    let bytes = snapshot.encode();
    for cut in 1..bytes.len() {
      assert!(
        matches!(
          RaftMessage::decode(&bytes[..cut]),
          Err(RaftWireError::Truncated | RaftWireError::LengthMismatch)
        ),
        "cut at {cut}"
      );
    }
    let mut long = bytes.clone();
    long.push(0);
    assert_eq!(
      RaftMessage::decode(&long),
      Err(RaftWireError::LengthMismatch)
    );
    // The state's length is the four bytes before it: claim far more than follows.
    let state_length_at = bytes.len() - b"configuration".len() - size_of::<u32>();
    let mut lying = bytes.clone();
    lying[state_length_at..state_length_at + size_of::<u32>()]
      .copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
      RaftMessage::decode(&lying),
      Err(RaftWireError::LengthMismatch)
    );
    // The joint flag follows the tag, four u64 fields, the voter count and two voters.
    let joint_flag_at = 1 + 4 * size_of::<u64>() + size_of::<u32>() + 2 * size_of::<u64>();
    let mut bad_flag = bytes.clone();
    bad_flag[joint_flag_at] = 2;
    assert_eq!(
      RaftMessage::decode(&bad_flag),
      Err(RaftWireError::BadFlag { flag: 2 })
    );
    assert_eq!(
      RaftMessage::decode_from(&bytes, B),
      Err(RaftWireError::ForeignSender)
    );
    let reply = RaftMessage::InstallSnapshotReply(InstallSnapshotReply {
      follower: B,
      term: 9,
      match_index: 7,
    })
    .encode();
    assert_eq!(
      RaftMessage::decode_from(&reply, A),
      Err(RaftWireError::ForeignSender)
    );
  }

  /// Doc truth: [`LogEntry::encoded_len`] — the size the core's append budget counts — is exactly the bytes
  /// this codec writes for an entry, for a command entry, an empty one, and configuration entries with and
  /// without a joint set; and [`APPEND_HEADER_BYTES`] is exactly an empty append's encoding.
  #[test]
  fn an_entry_takes_the_bytes_the_core_counts() {
    let entries = [
      LogEntry::command(3, b"command".to_vec()),
      LogEntry::command(3, Vec::new()),
      LogEntry::configuration(
        4,
        VoterConfig {
          voters: vec![A, B, HostId(3)],
          joint: None,
        },
      ),
      LogEntry::configuration(
        5,
        VoterConfig {
          voters: vec![A],
          joint: Some(vec![B, HostId(3)]),
        },
      ),
    ];
    for entry in &entries {
      let mut out = Vec::new();
      encode_entry(&mut out, entry);
      assert_eq!(out.len(), entry.encoded_len(), "{entry:?}");
    }
    let empty = RaftMessage::AppendEntries(AppendEntries {
      read_context: 0,
      term: 0,
      leader: A,
      prev_log_index: 0,
      prev_log_term: 0,
      entries: Vec::new(),
      leader_commit: 0,
      priorities: Vec::new(),
      sync_index: 0,
      open_from: 0,
    });
    // The header, an empty priority table's count, then the trailer.
    assert_eq!(
      empty.encode().len(),
      APPEND_HEADER_BYTES + size_of::<u32>() + APPEND_TRAILER_BYTES
    );
  }

  /// The derived batch budget: a whole append — its header, a five-voter priority table, its trailer, the
  /// sender's envelope and a batch of exactly the budget — is the credit a fresh session grants before any
  /// window update, so it needs no update to arrive; and an envelope larger than that credit leaves no budget
  /// rather than wrapping.
  #[test]
  fn a_budgeted_append_fits_a_fresh_sessions_first_credit() {
    let frame_cap = slates_transport::endpoint::MAX_PACKET_PAYLOAD;
    let envelope = 36;
    let credit = usize::try_from(slates_transport::connection::initial_receive_window(
      frame_cap,
    ))
    .unwrap();
    let voters = 5;
    let table = size_of::<u32>() + voters * PRIORITY_ROW_BYTES;
    let budget = append_batch_bytes(frame_cap, envelope, voters);
    assert!(budget > 0);
    assert_eq!(
      budget + APPEND_HEADER_BYTES + table + APPEND_TRAILER_BYTES + envelope,
      credit
    );
    assert_eq!(append_batch_bytes(frame_cap, credit, voters), 0);
  }

  /// An empty input and an unknown tag are refused, not panicked.
  #[test]
  fn empty_and_foreign_bytes_are_refused() {
    assert_eq!(RaftMessage::decode(&[]), Err(RaftWireError::Truncated));
    assert_eq!(
      RaftMessage::decode(&[0xEE, 0, 0]),
      Err(RaftWireError::UnknownTag { tag: 0xEE })
    );
  }

  /// An append claiming a vast entry count with no entries to back it is refused before allocating.
  #[test]
  fn a_lying_entry_count_is_refused() {
    let mut bytes = vec![TAG_APPEND_ENTRIES];
    for _ in 0..6 {
      bytes.extend_from_slice(&0u64.to_le_bytes()); // term, leader, prev index/term, commit, read context
    }
    bytes.extend_from_slice(&u32::MAX.to_le_bytes()); // entry count = huge
    assert_eq!(
      RaftMessage::decode(&bytes),
      Err(RaftWireError::LengthMismatch),
      "a count beyond the bytes is refused"
    );
  }

  /// A trailing byte after a complete message is refused (a malformed datagram).
  #[test]
  fn trailing_bytes_are_refused() {
    let mut bytes = RaftMessage::VoteReply(VoteReply {
      voter: A,
      term: 1,
      granted: false,
      reports: Vec::new(),
    })
    .encode();
    bytes.push(0);
    assert_eq!(
      RaftMessage::decode(&bytes),
      Err(RaftWireError::LengthMismatch)
    );
  }

  /// A non-boolean flag byte (neither 0 nor 1) is refused.
  #[test]
  fn a_bad_flag_is_refused() {
    // A VoteReply whose granted byte is 2.
    let mut bytes = vec![TAG_VOTE_REPLY];
    bytes.extend_from_slice(&1u64.to_le_bytes()); // voter
    bytes.extend_from_slice(&1u64.to_le_bytes()); // term
    bytes.push(2); // granted flag — invalid
    assert_eq!(
      RaftMessage::decode(&bytes),
      Err(RaftWireError::BadFlag { flag: 2 })
    );
  }

  /// A regional configuration round-trips through encode/decode unchanged — members, per-owner
  /// neighbourhoods, epochs, domains, quorum, version and mirror flag — so a learner decodes exactly what a
  /// voter encoded.
  #[test]
  fn a_regional_configuration_round_trips() {
    use slates_db::register::RegionalConfiguration;
    let mut domains = std::collections::BTreeMap::new();
    domains.insert(A, 7);
    let mut config =
      RegionalConfiguration::formed(vec![A, B, HostId(3)], Quorum { f: 1 }, domains, 3, true);
    let bytes = encode_regional_configuration(&config);
    assert_eq!(
      decode_regional_configuration(&bytes),
      Ok(config.clone()),
      "round-trip is identity"
    );
    // A change in flight (an admission moved the neighbourhoods) and a retirement with a confirmation and an
    // unconfirmed lineage: every settled neighbourhood, domain and retirement field survives the round trip.
    assert!(config.admit(HostId(4), Some(9), 3));
    config.take_over(A, 3);
    let survivor = config
      .recovery_hosts(A)
      .into_iter()
      .find(|host| config.owes_confirmation(A, *host))
      .unwrap();
    assert!(config.confirm(A, survivor));
    config.take_over(HostId(4), 3);
    assert!(!config.retired.is_empty());
    let bytes = encode_regional_configuration(&config);
    assert_eq!(
      decode_regional_configuration(&bytes),
      Ok(config),
      "round-trip is identity with retirements and a change in flight"
    );
  }

  /// A retirement whose confirmed-host count lies — more ids than the bytes back — is refused before
  /// allocating, and a configuration cut short inside its retirements is refused as truncated.
  #[test]
  fn a_lying_retirement_count_is_refused() {
    use slates_db::register::RegionalConfiguration;
    let mut config = RegionalConfiguration::formed(
      vec![A, B, HostId(3)],
      Quorum { f: 1 },
      std::collections::BTreeMap::new(),
      3,
      false,
    );
    config.take_over(A, 3);
    let bytes = encode_regional_configuration(&config);
    // The last retirement ends with its confirmed ids (a count, none here) and its unconfirmed ids (a count,
    // none here): make the confirmed count claim every id there is.
    let confirmed_count_at = bytes.len() - 2 * size_of::<u32>();
    let mut lying = bytes.clone();
    lying[confirmed_count_at..confirmed_count_at + size_of::<u32>()]
      .copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
      decode_regional_configuration(&lying),
      Err(RaftWireError::LengthMismatch),
      "a count beyond the bytes is refused"
    );
    for cut in 1..=2 * size_of::<u32>() {
      assert!(
        decode_regional_configuration(&bytes[..bytes.len() - cut]).is_err(),
        "a configuration cut {cut} bytes short is refused"
      );
    }
  }

  /// A regional configuration whose leading (members) count lies — more entries than the bytes back — is
  /// refused before allocating, not panicked.
  #[test]
  fn a_lying_regional_configuration_count_is_refused() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&0u64.to_le_bytes()); // version
    bytes.extend_from_slice(&1u32.to_le_bytes()); // quorum.f
    bytes.push(0); // has_mirror
    bytes.extend_from_slice(&u32::MAX.to_le_bytes()); // members count = huge
    assert_eq!(
      decode_regional_configuration(&bytes),
      Err(RaftWireError::LengthMismatch),
      "a count beyond the bytes is refused"
    );
  }

  /// A root configuration round-trips through encode/decode unchanged — regions, moved homes and promotions —
  /// so a region decodes exactly what the root group encoded.
  #[test]
  fn a_root_configuration_round_trips() {
    let mut config = RootConfiguration::formed(vec![RegionId(0), RegionId(1), RegionId(2)]);
    assert!(config.move_home(ObjectId::new(HostId(1), 7), RegionId(1)));
    assert!(config.promote_region(RegionId(2), RegionId(0)));
    let bytes = encode_root_configuration(&config);
    assert_eq!(
      decode_root_configuration(&bytes),
      Ok(config),
      "round-trip is identity"
    );
  }

  /// A root configuration whose leading (regions) count lies — more entries than the bytes back — is refused
  /// before allocating, not panicked.
  #[test]
  fn a_lying_root_configuration_count_is_refused() {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&0u64.to_le_bytes()); // version
    bytes.extend_from_slice(&u32::MAX.to_le_bytes()); // regions count = huge
    assert_eq!(
      decode_root_configuration(&bytes),
      Err(RaftWireError::LengthMismatch),
      "a count beyond the bytes is refused"
    );
  }
}
