//! A git record's replay a turn at a time (A-722 part 4; main's ruling of 2026-11-20: a large pack record's replay ran
//! in one turn, held its shard past the watchdog's window and crash-looped the daemon).
//!
//! **What.** A version-2 record's pack (A-723) is read a chunk at a time and its objects admitted one at a time, each
//! turn held to a deadline on the shard's clock and taking at least one unit, so a record of any size is replayed over
//! as many turns as it needs. The record is decoded once, as its replay begins: its names and moves are kept, and its
//! pack is read again from the register's bytes at each turn by its place in them, never copied. Once the pack's
//! objects are in, the names the admission would record are compared with the record's; the push is sealed and staged
//! a unit at a time as a live push is (`slates_git::store::Staging`); then the moves and the commit follow, as
//! [`super::replay_push`] runs them. A pack's waiting deltas resolve a delta a unit (`Reader::resolve_turn_with`, w-balance's
//! resolution in turns), and the record's moves are admitted an object a unit (A-722 part 4g). Owed its own slices: the
//! landing's whole passes (`Repository::commit`).
//!
//! **Not stepped.** An owed commit's record, an LFS record and a record carrying no pack replay whole in their turn,
//! as before: none holds a pack's objects.
//!
//! **Given back.** A replay refused, or ended by its rebuild's end, gives back what its push held ([`GitReplay::abandon`]).

use slates_git::Admission;
use slates_git::object_name::{Kind, ObjectFormat};
use slates_git::store::{Admitting, Incoming, Move, Push, PushRefused};
use slates_git::unpack::{NoIndex, Object, Reader};
use slates_vcs::{ContentId, ContentStore};

use super::record::{Record, RecordWriter, RecordedMove};
use super::{
  GitRefused, GitStore, NO_INDEX, ReplayRefused, StoreBases, admit_object, commit_moves,
  names_alike,
};

/// A clock read as no time at all: a staging turn under it takes one unit (`Repository::stage_turn` with a quantum of
/// none), the replay's own deadline bounding its turn.
struct Still;

impl slates_git::store::learn::Clock for Still {
  fn now_ns(&mut self) -> u64 {
    0
  }
}

/// A git record's replay under way (module docs).
pub(crate) struct GitReplay {
  /// The push the record's objects are admitted into.
  push: Push,
  /// The record the admission would write again, compared with the record's names at the end.
  remade: RecordWriter,
  /// The record's names: each object it brought by type, content identity and length, in admission order (a version-3
  /// record's read from their content as its replay begins, A-724 piece 8c).
  names: Vec<(Kind, ContentId, u64)>,
  /// A version-3 record's names' content, to hold for the record at its replay's first turn where nothing holds it (a3:
  /// a window restored from the shard image is held for no one until a record holds it again).
  named: Option<ContentId>,
  /// The record's moves.
  moves: Vec<RecordedMove>,
  /// Where the record's pack begins in its bytes.
  pack_at: usize,
  /// What comes next.
  stage: Stage,
}

impl core::fmt::Debug for GitReplay {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    let stage = match &self.stage {
      Stage::Reading { read, .. } => format!("reading at {read}"),
      Stage::Admitting { objects, next } => format!("admitting {next} of {}", objects.len()),
      Stage::Sealing { .. } => "sealing".to_owned(),
      Stage::Resolving { .. } => "resolving".to_owned(),
      Stage::Moving { admitted, .. } => {
        format!("moving {} of {}", admitted.len(), self.moves.len())
      }
    };
    f.debug_struct("GitReplay")
      .field("names", &self.names.len())
      .field("moves", &self.moves.len())
      .field("stage", &stage)
      .finish_non_exhaustive()
  }
}

/// A replay's stage.
enum Stage {
  /// The record's moves admitted an object a unit (A-722 part 4g: a replay's end admitted them whole, a walk of every
  /// object a push brought in one turn, and a restarted daemon recovering git/git's v2.0.0 spun past the watchdog).
  Moving {
    /// The moves admitted so far, in the record's order.
    admitted: Vec<Move>,
    /// The admission of the next under way.
    admitting: Option<Box<Admitting>>,
  },
  /// The pack whole, its waiting deltas resolved a delta a unit (`Reader::resolve_turn_with`).
  Resolving {
    /// Git's reader of the pack (boxed, as reading's is).
    reader: Box<Reader<'static, NoIndex>>,
    /// What the reading holds.
    admission: Admission,
  },
  /// The push sealed and staged a unit at a time, once every object is in and the names compared.
  Sealing {
    /// The seal and stage under way (boxed: it is most of the stage's size).
    staging: Box<slates_git::store::Staging>,
  },
  /// The pack read a chunk at a time.
  Reading {
    /// Git's reader of the pack, holding what it resolved (boxed: it is most of the stage's size).
    reader: Box<Reader<'static, NoIndex>>,
    /// What the reading holds, charged within the room the content store had as the replay began.
    admission: Admission,
    /// The pack's bytes read so far.
    read: usize,
  },
  /// The pack's objects admitted one at a time.
  Admitting {
    /// The pack's objects, in pack order.
    objects: Vec<Object>,
    /// The next to admit.
    next: usize,
  },
}

/// What a turn of a git record's replay left.
pub(crate) enum Stepped {
  /// More turns remain.
  More(Box<GitReplay>),
  /// The record is replayed: the derived values not kept, as [`super::commit_moves`] answers them.
  Done(u64),
}

/// How much a turn may take: its deadline on the shard's clock, and the pack's bytes one read takes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Turn {
  /// The shard's monotonic nanosecond past which the turn ends, once it has taken a unit.
  pub(crate) deadline_ns: u64,
  /// The pack's bytes a read takes at most.
  pub(crate) chunk: usize,
}

/// Whether a replay of the record `bytes` is stepped (module docs): a version-2 record carrying a pack.
pub(crate) fn stepped(bytes: &[u8], format: ObjectFormat) -> bool {
  !super::owed::is_owed(bytes)
    && !super::lfs_record::is_lfs(bytes)
    && Record::decode(bytes, format).is_ok_and(|record| !record.pack.is_empty())
}

impl GitReplay {
  /// The replay of the record `bytes` begun (module docs): decoded once, its deepest chain carried, its pack's reader made
  /// with the room the content store has now.
  pub(crate) fn begin(
    content: &ContentStore,
    format: ObjectFormat,
    bytes: &[u8],
  ) -> Result<Box<Self>, ReplayRefused> {
    let record = Record::decode(bytes, format).map_err(ReplayRefused::Record)?;
    let (names, named) = match record.named {
      Some((identity, count)) => (
        super::record_names::names_from_content(content, &identity, count)
          .map_err(ReplayRefused::Names)?,
        Some(identity),
      ),
      None => (record.names, None),
    };
    let mut push = Push::new(format);
    push.carried(record.deepest);
    // The pack is the record's last field (`Record::decode` refuses trailing bytes), so it ends where the bytes do.
    let pack_at = bytes.len().saturating_sub(record.pack.len());
    Ok(Box::new(Self {
      push,
      remade: RecordWriter::default(),
      names,
      named,
      moves: record.moves,
      pack_at,
      stage: Stage::Reading {
        reader: Box::new(Reader::new(format, &NO_INDEX)),
        admission: Admission::new(content.room()),
        read: 0,
      },
    }))
  }

  /// One turn of the replay over the record `bytes` (module docs): a unit at least, then units until `turn`'s deadline.
  /// Refused, what the push held is given back.
  pub(crate) fn step(
    mut self: Box<Self>,
    (content, store): (&mut ContentStore, &mut GitStore),
    bytes: &[u8],
    (turn, now): (Turn, &mut dyn FnMut() -> u64),
  ) -> Result<Stepped, ReplayRefused> {
    if let Some(identity) = self.named.take()
      && content.holders(&identity) == 0
      && let Err(refused) = content.hold(&identity)
    {
      return Err(ReplayRefused::Git(GitRefused::Object(
        PushRefused::Content(refused),
      )));
    }
    loop {
      match self.unit((content, store), bytes, turn.chunk) {
        Ok(None) => {}
        Ok(Some(moves)) => return self.finish(content, store, moves).map(Stepped::Done),
        Err(refused) => return Err(self.abandon(content).err().unwrap_or(refused)),
      }
      if now() >= turn.deadline_ns {
        return Ok(Stepped::More(self));
      }
    }
  }

  /// One unit: a chunk of the pack read (the pack ended into its objects once whole), one object admitted (the names
  /// compared and the seal begun after the last), or one unit of the seal. Whether the push is sealed and staged.
  fn unit(
    &mut self,
    (content, store): (&mut ContentStore, &mut GitStore),
    bytes: &[u8],
    chunk: usize,
  ) -> Result<Option<Vec<Move>>, ReplayRefused> {
    let stage = core::mem::replace(
      &mut self.stage,
      Stage::Admitting {
        objects: Vec::new(),
        next: 0,
      },
    );
    let (stage, done) = match stage {
      Stage::Reading {
        mut reader,
        mut admission,
        read,
      } => {
        let pack = bytes.get(self.pack_at..).unwrap_or_default();
        let rest = pack.get(read..).unwrap_or_default();
        let piece = rest.get(..chunk.max(1)).unwrap_or(rest);
        let took = reader
          .read(piece, &mut admission)
          .map_err(ReplayRefused::Pack)?;
        if reader.is_whole() {
          (Stage::Resolving { reader, admission }, None)
        } else if piece.is_empty() || took == 0 {
          // No more to give it: ended, a pack cut short refused by its end.
          let bases = StoreBases(slates_git::store::views::IndexedObjects::new(
            &store.repository,
            content,
            content.room(),
          ));
          let unpacked = reader
            .finish_with(&bases, &mut admission)
            .map_err(ReplayRefused::Pack)?;
          (
            Stage::Admitting {
              objects: unpacked.objects,
              next: 0,
            },
            None,
          )
        } else {
          let read = read.saturating_add(took);
          (
            Stage::Reading {
              reader,
              admission,
              read,
            },
            None,
          )
        }
      }
      Stage::Admitting { objects, next } => match objects.get(next) {
        Some(object) => {
          let incoming = Incoming {
            kind: object.kind,
            claimed: Some(object.id),
            content: &object.content,
            depth: object.chain.as_ref().map_or(0, |chain| chain.depth),
          };
          admit_object(
            content,
            store,
            (&mut self.push, &mut self.remade),
            (incoming, None),
          )
          .map_err(ReplayRefused::Git)?;
          (
            Stage::Admitting {
              objects,
              next: next.saturating_add(1),
            },
            None,
          )
        }
        None => {
          if !names_alike(content, store, &self.remade, &self.names) {
            return Err(ReplayRefused::Diverged);
          }
          let staging = store
            .repository
            .sealing(content, &mut self.push)
            .map_err(|refused| ReplayRefused::Git(GitRefused::Store(refused)))?;
          (
            Stage::Sealing {
              staging: Box::new(staging),
            },
            None,
          )
        }
      },
      Stage::Resolving {
        mut reader,
        mut admission,
      } => {
        let bases = StoreBases(slates_git::store::views::IndexedObjects::new(
          &store.repository,
          content,
          content.room(),
        ));
        let mut still = Still;
        let quantum = slates_git::store::hook_view::Quantum {
          clock: &mut still,
          ns: 0,
        };
        let resolved = reader
          .resolve_turn_with(&bases, &mut admission, quantum)
          .map_err(ReplayRefused::Pack)?;
        if !resolved {
          return {
            self.stage = Stage::Resolving { reader, admission };
            Ok(None)
          };
        }
        let unpacked = reader
          .finish_with(&bases, &mut admission)
          .map_err(ReplayRefused::Pack)?;
        (
          Stage::Admitting {
            objects: unpacked.objects,
            next: 0,
          },
          None,
        )
      }
      Stage::Sealing { mut staging } => {
        let mut still = Still;
        let quantum = slates_git::store::hook_view::Quantum {
          clock: &mut still,
          ns: 0,
        };
        let staged = store
          .repository
          .stage_turn(content, &mut self.push, &mut staging, quantum)
          .map_err(|refused| ReplayRefused::Git(GitRefused::Store(refused)))?;
        if staged {
          (
            Stage::Moving {
              admitted: Vec::new(),
              admitting: None,
            },
            None,
          )
        } else {
          (Stage::Sealing { staging }, None)
        }
      }
      Stage::Moving {
        mut admitted,
        admitting,
      } => {
        let admitting = self.move_unit(&*content, &*store, &mut admitted, admitting)?;
        if admitting.is_none() && admitted.len() >= self.moves.len() {
          (
            Stage::Admitting {
              objects: Vec::new(),
              next: 0,
            },
            Some(admitted),
          )
        } else {
          (
            Stage::Moving {
              admitted,
              admitting,
            },
            None,
          )
        }
      }
    };
    self.stage = stage;
    Ok(done)
  }

  /// The replay's end once the push is sealed and staged: the moves and the commit, as [`super::replay_push`] ends;
  /// what the push held given back after.
  fn finish(
    mut self: Box<Self>,
    content: &mut ContentStore,
    store: &mut GitStore,
    moves: Vec<Move>,
  ) -> Result<u64, ReplayRefused> {
    let ended = self.end(content, store, &moves);
    // A committed push holds nothing, so this gives back only what a refused end held.
    let given = self.abandon(content);
    let unkept = ended?;
    given?;
    Ok(unkept)
  }

  /// One unit of the moves' admission: a unit of the admission under way, the move it admitted kept; or the next
  /// recorded move's admission begun (a delete admitted at once). The admission under way after it.
  fn move_unit(
    &self,
    content: &ContentStore,
    store: &GitStore,
    admitted: &mut Vec<Move>,
    admitting: Option<Box<Admitting>>,
  ) -> Result<Option<Box<Admitting>>, ReplayRefused> {
    let refused = |refused| ReplayRefused::Git(GitRefused::Store(refused));
    if let Some(mut under_way) = admitting {
      let mut still = Still;
      let quantum = slates_git::store::hook_view::Quantum {
        clock: &mut still,
        ns: 0,
      };
      if !store
        .repository
        .admit_turn(content, &self.push, &mut under_way, quantum)
        .map_err(refused)?
      {
        return Ok(Some(under_way));
      }
      if let Some(admission) = under_way.admitted() {
        admitted.push(admission);
      }
      return Ok(None);
    }
    let Some(&(green, _, head)) = self.moves.get(admitted.len()) else {
      return Ok(None);
    };
    match head {
      Some(to) => store
        .repository
        .admitting(content, &self.push, green, to)
        .map(|begun| Some(Box::new(begun)))
        .map_err(refused),
      None => {
        let deleted = store
          .repository
          .admit(content, &self.push, green, None)
          .map_err(refused)?;
        admitted.push(deleted);
        Ok(None)
      }
    }
  }

  /// [`GitReplay::finish`]'s steps: the moves admitted ([`Stage::Moving`]) committed.
  fn end(
    &mut self,
    content: &mut ContentStore,
    store: &mut GitStore,
    moves: &[Move],
  ) -> Result<u64, ReplayRefused> {
    commit_moves(content, store, &mut self.push, (moves, &self.moves)).map_err(ReplayRefused::Git)
  }

  /// Gives back what the replay's push holds.
  pub(crate) fn abandon(
    mut self: Box<Self>,
    content: &mut ContentStore,
  ) -> Result<(), ReplayRefused> {
    let stage = core::mem::replace(
      &mut self.stage,
      Stage::Admitting {
        objects: Vec::new(),
        next: 0,
      },
    );
    let staged = match stage {
      Stage::Sealing { staging } => staging
        .abandon(content)
        .map_err(|refused| ReplayRefused::Git(GitRefused::Store(refused))),
      _ => Ok(()),
    };
    let given = self
      .push
      .abandon(content)
      .map_err(|refused| ReplayRefused::Git(GitRefused::Object(PushRefused::Content(refused))));
    staged.and(given)
  }
}
