//! The session log as the client reads it (`slates log`, A-773): the daemon's lines walked a page at a time, each
//! green's head joined once per green named, and the decisions that need the human picked out ([`SessionLog::needs_you`]).
//! The CLI, the MCP server and the SDKs read the one view through here, so they agree on what needs the human.
//!
//! **What needs the human, and only that** (Sapling's Smartlog keeps what decides the next action and hides the rest):
//! a work whose last submission collided, since no rebase or retry resolves a collision the same span made (D-27: the
//! verdict is accept, identical or conflict, never an inferred merge); and a landing presented and awaiting its grant,
//! which only a person grants (R10). An accepted or an open work, however far its green moved past it, is shown and
//! not asked about: its agent rebases or submits on its own.
//!
//! **Reads sized by what it shows.** The walk costs the lines the daemon holds (works and waiting landings, never the
//! landing history), and the heads cost one `versions` a green the lines name.

use std::collections::BTreeMap;

use slates_ipc::protocol::{LogCursor, LogLine, ReplyBody, RequestBody, VolumeId, WorkMark};

use crate::client::{Client, Page};
use crate::error::ClientError;

/// A work as the session log shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogWork {
  /// The work.
  pub id: VolumeId,
  /// Its name.
  pub name: String,
  /// The green it is over.
  pub green: VolumeId,
  /// The green version it is based on.
  pub base: u64,
  /// How its last submission ended.
  pub mark: WorkMark,
}

/// A landing presented and awaiting its grant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogLanding {
  /// The landing.
  pub landing: u64,
  /// The volume it lands.
  pub volume: VolumeId,
  /// The host directory it would write.
  pub target: String,
  /// The manifest a grant must bind.
  pub manifest: [u8; 32],
}

/// The session log: every work the caller may read, the landings awaiting a grant, and the heads of the greens the
/// works are over.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionLog {
  /// The works, in id order.
  pub works: Vec<LogWork>,
  /// The landings awaiting a grant, by volume then landing.
  pub landings: Vec<LogLanding>,
  /// Each green's head, for the greens the works are over and the caller may read.
  pub heads: BTreeMap<[u8; 16], u64>,
}

/// One decision that needs the human.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NeedsYou {
  /// A work's last submission collided.
  Collided {
    /// The work.
    work: VolumeId,
    /// Its name.
    name: String,
    /// The green.
    green: VolumeId,
    /// The first collided path.
    path: String,
    /// How many windows collided.
    windows: u32,
    /// How many versions the green moved past the work's base (0 when its head is unknown).
    behind: u64,
  },
  /// A landing awaits its grant.
  AwaitingGrant {
    /// The landing.
    landing: u64,
    /// The volume it lands.
    volume: VolumeId,
    /// The host directory it would write.
    target: String,
    /// The manifest a grant must bind.
    manifest: [u8; 32],
  },
}

impl SessionLog {
  /// `green`'s head, when the log read it.
  pub fn head_of(&self, green: VolumeId) -> Option<u64> {
    self.heads.get(&green.bytes).copied()
  }

  /// How many versions `work`'s green moved past its base: what advanced since it started (0 when the head is
  /// unknown).
  pub fn behind(&self, work: &LogWork) -> u64 {
    self
      .head_of(work.green)
      .map_or(0, |head| head.saturating_sub(work.base))
  }

  /// The decisions that need the human, collisions first, each once.
  pub fn needs_you(&self) -> Vec<NeedsYou> {
    let collided = self.works.iter().filter_map(|work| match &work.mark {
      WorkMark::Collided { windows, first, .. } => Some(NeedsYou::Collided {
        work: work.id,
        name: work.name.clone(),
        green: work.green,
        path: first
          .as_ref()
          .map(|window| window.path.clone())
          .unwrap_or_default(),
        windows: *windows,
        behind: self.behind(work),
      }),
      WorkMark::Open | WorkMark::Accepted { .. } | WorkMark::Identical { .. } => None,
    });
    let waiting = self.landings.iter().map(|landing| NeedsYou::AwaitingGrant {
      landing: landing.landing,
      volume: landing.volume,
      target: landing.target.clone(),
      manifest: landing.manifest,
    });
    collided.chain(waiting).collect()
  }
}

/// The key a cursor or a line orders by.
fn cursor_key(cursor: &LogCursor) -> ([u8; 16], Option<u64>) {
  (cursor.volume.bytes, cursor.landing)
}

impl Client {
  /// The session log (A-773): every page walked ([`Client::session_log_page`]), then each named green's head read
  /// once. A green the caller may not read leaves its head unknown rather than refusing the log.
  pub fn session_log(&mut self) -> Result<SessionLog, ClientError> {
    let mut log = SessionLog::default();
    let mut after = None;
    loop {
      let page = self.session_log_page(after)?;
      for line in page.items {
        match line {
          LogLine::Work {
            id,
            name,
            green,
            base,
            mark,
          } => log.works.push(LogWork {
            id,
            name,
            green,
            base,
            mark,
          }),
          LogLine::AwaitingGrant {
            landing,
            volume,
            target,
            manifest,
          } => log.landings.push(LogLanding {
            landing,
            volume,
            target,
            manifest,
          }),
        }
      }
      match page.next {
        None => break,
        next => after = next,
      }
    }
    let greens: std::collections::BTreeSet<[u8; 16]> =
      log.works.iter().map(|work| work.green.bytes).collect();
    for bytes in greens {
      match self.versions(VolumeId { bytes }) {
        Ok(head) => {
          log.heads.insert(bytes, head);
        }
        Err(ClientError::Refused(_)) => {}
        Err(other) => return Err(other),
      }
    }
    Ok(log)
  }

  /// One page of the session log past `after` (`None` begins), and the cursor the next page is asked after, `None`
  /// when this page ends it. A page whose cursor does not move past `after` is refused
  /// [`ClientError::UnexpectedReply`], so a walk always ends.
  pub fn session_log_page(
    &mut self,
    after: Option<LogCursor>,
  ) -> Result<Page<LogLine, LogCursor>, ClientError> {
    let ReplyBody::SessionLog { lines, next } = self.call(&RequestBody::SessionLog { after })?
    else {
      return Err(ClientError::UnexpectedReply {
        verb: "session_log",
      });
    };
    if let (Some(next), Some(after)) = (&next, &after)
      && cursor_key(next) <= cursor_key(after)
    {
      return Err(ClientError::UnexpectedReply {
        verb: "session_log",
      });
    }
    Ok(Page { items: lines, next })
  }
}

#[cfg(test)]
mod tests {
  use slates_ipc::protocol::MergeWindow;

  use super::*;

  fn id(n: u8) -> VolumeId {
    VolumeId { bytes: [n; 16] }
  }

  fn work(n: u8, base: u64, mark: WorkMark) -> LogWork {
    LogWork {
      id: id(n),
      name: format!("agent-{n}"),
      green: id(0),
      base,
      mark,
    }
  }

  /// A-773. Do: a log of eight accepted works, one open work far behind its green, one collided work and one landing
  /// awaiting its grant. Expect: exactly the collision (its path, how far its green moved) and the landing need the
  /// human; the open work behind its green does not.
  #[test]
  fn only_a_collision_and_a_waiting_landing_need_the_human() {
    let mut works: Vec<LogWork> = (1..=8)
      .map(|n| {
        work(
          n,
          u64::from(n) + 1,
          WorkMark::Accepted {
            version: u64::from(n) + 1,
          },
        )
      })
      .collect();
    works.push(work(9, 1, WorkMark::Open));
    works.push(work(
      10,
      1,
      WorkMark::Collided {
        verdict: 7,
        windows: 2,
        first: Some(MergeWindow {
          path: "/shared.txt".to_owned(),
          at: 0,
          len: 4,
          class: 0,
        }),
      },
    ));
    let log = SessionLog {
      works,
      landings: vec![LogLanding {
        landing: 3,
        volume: id(20),
        target: "/srv/out".to_owned(),
        manifest: [9; 32],
      }],
      heads: [([0; 16], 10)].into_iter().collect(),
    };
    assert_eq!(
      log.needs_you(),
      vec![
        NeedsYou::Collided {
          work: id(10),
          name: "agent-10".to_owned(),
          green: id(0),
          path: "/shared.txt".to_owned(),
          windows: 2,
          behind: 9,
        },
        NeedsYou::AwaitingGrant {
          landing: 3,
          volume: id(20),
          target: "/srv/out".to_owned(),
          manifest: [9; 32],
        },
      ]
    );
  }
}
