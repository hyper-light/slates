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

use slates_db::register::{
  DomainId, HostEpoch, HostId, Neighbourhood, OBJECT_BYTES, ObjectId, Quorum, RegionId,
  RegionalConfiguration, RootConfiguration,
};
use slates_transport::endpoint::{Endpoint, EndpointError};

use crate::raft::{
  AppendEntries, AppendReply, LogEntry, PreVote, PreVoteReply, RaftNode, RequestVote, VoteReply,
  VoterConfig,
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
/// Format: the pre-election tag bytes, continuing the leading-tag sequence.
const TAG_PRE_VOTE: u8 = 5;
const TAG_PRE_VOTE_REPLY: u8 = 6;

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
    .serve_once(|_, request| match RaftMessage::decode(&request) {
      Ok(RaftMessage::PreVote(pre)) => RaftMessage::PreVoteReply(node.on_pre_vote(pre)).encode(),
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
  put_u32(
    &mut out,
    u32::try_from(config.domains.len()).unwrap_or(u32::MAX),
  );
  for (host, domain) in &config.domains {
    put_u64(&mut out, host.0);
    put_u64(&mut out, *domain);
  }
  out
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
  expect_end(rest)?;
  Ok(RegionalConfiguration {
    version,
    members,
    neighbourhoods,
    epochs,
    domains,
    quorum: Quorum { f },
    has_mirror,
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

  /// A regional configuration round-trips through encode/decode unchanged — members, per-owner
  /// neighbourhoods, epochs, domains, quorum, version and mirror flag — so a learner decodes exactly what a
  /// voter encoded.
  #[test]
  fn a_regional_configuration_round_trips() {
    use slates_db::register::RegionalConfiguration;
    let mut domains = std::collections::BTreeMap::new();
    domains.insert(A, 7);
    let config =
      RegionalConfiguration::formed(vec![A, B, HostId(3)], Quorum { f: 1 }, domains, 3, true);
    let bytes = encode_regional_configuration(&config);
    assert_eq!(
      decode_regional_configuration(&bytes),
      Ok(config),
      "round-trip is identity"
    );
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
