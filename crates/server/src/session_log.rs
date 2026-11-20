//! The session log (`slates log`, A-773): what a person supervising many agents must see to decide the next action,
//! and nothing else, in the spirit of Sapling's Smartlog. Each line is a work, with the green version it started from
//! (or its last accepted submission moved it to) and how its last submission ended, or a landing presented and
//! awaiting its grant (§4.15). The client joins each green's head and names what needs the human: a collided work and
//! a landing awaiting a grant.
//!
//! **Reads sized by what it shows (Ada's rule).** A shard's part walks its works from the cursor (the works it holds,
//! changes in RAM) and the presentations it holds awaiting a grant (bounded by `landings_awaiting_per_shard`), never
//! the landing history, and stops at one page. The log is a read: it shows a
//! pending grant and never makes one (R10); `grant` stays the CLI's and the launcher's.
//!
//! **One order across shards.** A line's key is its volume and, for a landing, its id (`None` for a work's line, which
//! sorts first): the same order `verbs::merged_page` folds the shards' parts in, so a page's last line is the cursor
//! every shard continues from.

use slates_db::catalog::{Principal, VolumeState};
use slates_ipc::protocol::{LogCursor, LogLine, ReplyBody, VolumeId, WorkMark};

use crate::state::{ShardState, WorkState};
use crate::verbs::{PagePart, rights_of, to_db_volume, to_wire_volume};

/// A line's key: its volume's bytes, and its landing for a landing's line.
type Key = ([u8; 16], Option<u64>);

/// The key of `line`.
pub(crate) fn key_of(line: &LogLine) -> Key {
  match line {
    LogLine::Work { id, .. } => (id.bytes, None),
    LogLine::AwaitingGrant {
      volume, landing, ..
    } => (volume.bytes, Some(*landing)),
  }
}

/// The cursor naming `line`.
fn cursor_of(line: &LogLine) -> LogCursor {
  let (bytes, landing) = key_of(line);
  LogCursor {
    volume: VolumeId { bytes },
    landing,
  }
}

/// How `work`'s last submission ended: collided while its conflicting verdict stands, accepted while it stands on the
/// version its last accepted submission made, and open otherwise (made, or rebased since).
fn mark_of(work: &WorkState) -> WorkMark {
  if let Some(kept) = &work.verdict {
    return WorkMark::Collided {
      verdict: kept.id,
      windows: u32::try_from(kept.windows.len()).unwrap_or(u32::MAX),
      first: kept.windows.first().cloned(),
    };
  }
  match work.accepted {
    Some(version) if version == work.base_version => WorkMark::Accepted { version },
    _ => WorkMark::Open,
  }
}

/// This shard's works past `after`, as lines, in id order, those the caller may read.
fn work_lines<'a>(
  state: &'a ShardState,
  principal: &'a Principal,
  after: Option<LogCursor>,
) -> impl Iterator<Item = LogLine> + 'a {
  // A work's line precedes its volume's landings, so a cursor on the volume (a work's line or a landing's) resumes
  // past that work.
  let from = match after {
    Some(cursor) => std::ops::Bound::Excluded(to_db_volume(cursor.volume)),
    None => std::ops::Bound::Unbounded,
  };
  state
    .works
    .range((from, std::ops::Bound::Unbounded))
    .filter_map(move |(id, work)| {
      let record = state.db.partition().volume(*id)?;
      if !rights_of(record, principal).read || record.state == VolumeState::Destroyed {
        return None;
      }
      Some(LogLine::Work {
        id: to_wire_volume(*id),
        name: record.name.clone(),
        green: to_wire_volume(work.green),
        base: work.base_version,
        mark: mark_of(work),
      })
    })
}

/// This shard's landings awaiting a grant past `after`, as lines, in (volume, landing) order, those whose volume the
/// caller may read. Read from the presentations the shard holds now (`state.landing.awaiting`), which are exactly the
/// grantable ones: a presentation its client abandoned (AUD-29-07) or a newer one superseded (A-779) is not there,
/// though its durable record may still read `AwaitingGrant` (abandoned). Bounded by `landings_awaiting_per_shard`, so
/// sorting them costs that bound, never the landing history.
fn waiting_lines<'a>(
  state: &'a ShardState,
  principal: &'a Principal,
  after: Option<LogCursor>,
) -> impl Iterator<Item = LogLine> + 'a {
  let partition = state.db.partition();
  let mut lines: Vec<LogLine> = state
    .landing
    .awaiting
    .iter()
    .filter_map(|(id, awaiting)| {
      let record = partition.volume(awaiting.volume)?;
      if !rights_of(record, principal).read {
        return None;
      }
      let line = LogLine::AwaitingGrant {
        landing: *id,
        volume: to_wire_volume(awaiting.volume),
        target: awaiting.target.clone(),
        manifest: awaiting.manifest,
      };
      let past = after.is_none_or(|cursor| key_of(&line) > (cursor.volume.bytes, cursor.landing));
      past.then_some(line)
    })
    .collect();
  lines.sort_by_key(key_of);
  lines.into_iter()
}

/// Two key-ordered line streams as one, in key order.
struct Merged<A: Iterator<Item = LogLine>, B: Iterator<Item = LogLine>> {
  works: std::iter::Peekable<A>,
  waiting: std::iter::Peekable<B>,
}

impl<A: Iterator<Item = LogLine>, B: Iterator<Item = LogLine>> Iterator for Merged<A, B> {
  type Item = LogLine;

  fn next(&mut self) -> Option<LogLine> {
    let work_first = match (self.works.peek(), self.waiting.peek()) {
      (Some(work), Some(waiting)) => key_of(work) <= key_of(waiting),
      (Some(_), None) => true,
      (None, _) => false,
    };
    if work_first {
      self.works.next()
    } else {
      self.waiting.next()
    }
  }
}

/// This shard's part of the session log page past `after`: its lines in key order, as many as one page holds, and
/// whether more follow here.
pub(crate) fn part(
  state: &ShardState,
  principal: &Principal,
  after: Option<LogCursor>,
) -> PagePart<LogLine> {
  let lines = Merged {
    works: work_lines(state, principal, after).peekable(),
    waiting: waiting_lines(state, principal, after).peekable(),
  };
  let (items, more) = crate::verbs::fitting(lines, page_room());
  PagePart { items, more }
}

/// The session log page the shards' `parts` make: their lines in key order, as many as one reply chunk holds, and the
/// cursor of its last line when more follow.
pub(crate) fn page(parts: Vec<PagePart<LogLine>>) -> ReplyBody {
  let (lines, more) = crate::verbs::merged_page(parts, page_room(), key_of);
  let next = more.then(|| lines.last().map(cursor_of)).flatten();
  ReplyBody::SessionLog { lines, next }
}

/// The bytes a session log page's lines may take: one reply chunk less the page's own framing, its cursor included.
fn page_room() -> usize {
  let chunk = usize::try_from(crate::config::bulk_chunk_bytes()).unwrap_or(usize::MAX);
  let empty = ReplyBody::SessionLog {
    lines: Vec::new(),
    next: Some(LogCursor {
      volume: VolumeId {
        bytes: [u8::MAX; 16],
      },
      landing: Some(u64::MAX),
    }),
  };
  chunk.saturating_sub(slates_ipc::protocol::framed_len(&empty))
}
