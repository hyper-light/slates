//! The Raft wire (§4.8, §4.10a) — the on-the-wire encoding of the configuration group's Raft messages
//! ([`RequestVote`], [`VoteReply`], [`AppendEntries`], [`AppendReply`]) so the dialect can be driven over
//! the fleet transport. The state machine ([`crate::raft`]) is sans-io and message-passing; this module
//! is the pure codec that turns those messages into bytes and back.
//!
//! Every decode is a parser of external bytes: it checks each length against the bytes that actually
//! arrived before allocating, so a hostile datagram claiming a huge entry count, command length or voter
//! count is a typed [`RaftWireError`], never a panic or an over-allocation. Little-endian throughout, so
//! two hosts encode a message identically (the determinism the golden vectors pin).

use std::mem::size_of;

use slates_db::register::HostId;
use slates_transport::endpoint::{Endpoint, EndpointError};

use crate::raft::{
  AppendEntries, AppendReply, LogEntry, RaftNode, RequestVote, VoteReply, VoterConfig,
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
}

/// A refusal to decode a Raft message from received bytes (the closed hostile-input taxonomy).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RaftWireError {
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

/// Shape: the largest entry count, command length or voter count a decoder accepts before allocating —
/// a hostile datagram cannot force an unbounded allocation. Far above any real Raft batch or fleet size.
const MAX_ITEMS: usize = 1 << 20;

impl RaftMessage {
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
      }
      RaftMessage::AppendEntries(append) => {
        out.push(TAG_APPEND_ENTRIES);
        put_u64(&mut out, append.term);
        put_u64(&mut out, append.leader.0);
        put_u64(&mut out, append.prev_log_index);
        put_u64(&mut out, append.prev_log_term);
        put_u64(&mut out, append.leader_commit);
        put_u32(
          &mut out,
          u32::try_from(append.entries.len()).unwrap_or(u32::MAX),
        );
        for entry in &append.entries {
          encode_entry(&mut out, entry);
        }
      }
      RaftMessage::AppendReply(reply) => {
        out.push(TAG_APPEND_REPLY);
        put_u64(&mut out, reply.follower.0);
        put_u64(&mut out, reply.term);
        out.push(u8::from(reply.success));
        put_u64(&mut out, reply.match_index);
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
        expect_end(rest)?;
        Ok(RaftMessage::VoteReply(VoteReply {
          voter: HostId(voter),
          term,
          granted,
        }))
      }
      TAG_APPEND_ENTRIES => {
        let (term, rest) = take_u64(rest)?;
        let (leader, rest) = take_u64(rest)?;
        let (prev_log_index, rest) = take_u64(rest)?;
        let (prev_log_term, rest) = take_u64(rest)?;
        let (leader_commit, rest) = take_u64(rest)?;
        let (count, mut rest) = take_count(rest)?;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
          let (entry, tail) = decode_entry(rest)?;
          entries.push(entry);
          rest = tail;
        }
        expect_end(rest)?;
        Ok(RaftMessage::AppendEntries(AppendEntries {
          term,
          leader: HostId(leader),
          prev_log_index,
          prev_log_term,
          entries,
          leader_commit,
        }))
      }
      TAG_APPEND_REPLY => {
        let (follower, rest) = take_u64(rest)?;
        let (term, rest) = take_u64(rest)?;
        let (success, rest) = take_bool(rest)?;
        let (match_index, rest) = take_u64(rest)?;
        expect_end(rest)?;
        Ok(RaftMessage::AppendReply(AppendReply {
          follower: HostId(follower),
          term,
          success,
          match_index,
        }))
      }
      other => Err(RaftWireError::UnknownTag { tag: other }),
    }
  }
}

/// Appends a log entry: its term, its command (length-prefixed), then its optional voter configuration.
fn encode_entry(out: &mut Vec<u8>, entry: &LogEntry) {
  put_u64(out, entry.term);
  put_u32(out, u32::try_from(entry.command.len()).unwrap_or(u32::MAX));
  out.extend_from_slice(&entry.command);
  match &entry.config {
    None => out.push(0),
    Some(config) => {
      out.push(1);
      encode_hosts(out, &config.voters);
      match &config.joint {
        None => out.push(0),
        Some(joint) => {
          out.push(1);
          encode_hosts(out, joint);
        }
      }
    }
  }
}

/// Decodes a log entry from the front of `bytes`, returning it and the remainder.
fn decode_entry(bytes: &[u8]) -> Result<(LogEntry, &[u8]), RaftWireError> {
  let (term, rest) = take_u64(bytes)?;
  let (command, rest) = take_bytes(rest)?;
  let (has_config, rest) = take_bool(rest)?;
  if !has_config {
    return Ok((LogEntry::command(term, command), rest));
  }
  let (voters, rest) = decode_hosts(rest)?;
  let (has_joint, rest) = take_bool(rest)?;
  let (joint, rest) = if has_joint {
    let (joint, rest) = decode_hosts(rest)?;
    (Some(joint), rest)
  } else {
    (None, rest)
  };
  Ok((
    LogEntry {
      term,
      command,
      config: Some(VoterConfig { voters, joint }),
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

/// Format: a Raft RPC rides one stream per peer connection; the server's `serve_once` accepts whichever
/// stream arrives, so the exact id is a fixed label, not a tunable.
const RAFT_STREAM: u64 = 1;

/// Serves one incoming Raft request on `node` over `endpoint` (§4.8): a received vote request or append
/// is run through the node's handler and the reply is sent back. A reply-typed or malformed request is
/// answered with nothing (the requester counts it as no reply). The caller loops this to keep serving.
pub async fn serve_raft_once(
  endpoint: &mut Endpoint,
  node: &mut RaftNode,
) -> Result<(), EndpointError> {
  endpoint
    .serve_once(|request| match RaftMessage::decode(&request) {
      Ok(RaftMessage::RequestVote(vote)) => {
        RaftMessage::VoteReply(node.on_request_vote(vote)).encode()
      }
      Ok(RaftMessage::AppendEntries(append)) => {
        RaftMessage::AppendReply(node.on_append_entries(append)).encode()
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
  let reply = endpoint.request(RAFT_STREAM, &message.encode()).await?;
  Ok(RaftMessage::decode(&reply).ok())
}

#[cfg(test)]
mod tests {
  use super::*;

  const A: HostId = HostId(1);
  const B: HostId = HostId(2);

  fn append_with_entries() -> RaftMessage {
    RaftMessage::AppendEntries(AppendEntries {
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
    })
  }

  /// Every message kind round-trips through encode/decode unchanged — including an append carrying a
  /// command entry and a configuration entry with a joint voter set.
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
      }),
      append_with_entries(),
      RaftMessage::AppendReply(AppendReply {
        follower: B,
        term: 5,
        success: true,
        match_index: 4,
      }),
    ];
    for message in messages {
      let bytes = message.encode();
      assert_eq!(
        RaftMessage::decode(&bytes),
        Ok(message),
        "round-trip is identity"
      );
    }
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
    for _ in 0..5 {
      bytes.extend_from_slice(&0u64.to_le_bytes()); // term, leader, prev index/term, leader_commit
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
}
