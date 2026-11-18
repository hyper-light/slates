//! The landing manifest (§4.15 "Plan"): every diverged entry of the snapshot with its action,
//! its witnessed base and its overlay identity, in a canonical order with a canonical
//! encoding, so the BLAKE3 of the encoding names exactly what a grant allows to be written.
//! Planning walks the overlay's loaded nodes only (the diverged set), never the base.
//!
//! A clone of a captured base (§4.15 `capture_base`, A-291) is planned another way
//! ([`PlanWalk::against_capture`]): its source tree against the captured snapshot's, over the paths where the two part
//! (`slates_vfs::captured`), every entry judged by content alone ([`Judged::Content`]). Its files hold no witness of
//! the target (the capture made every body the volume's own, and the target is another folder than the one the
//! capture read), so a witness there is the BLAKE3 of the bytes the capture held and nothing else.

use std::collections::BTreeMap;

use slates_vfs::base::{Diverged, Divergence};
use slates_vfs::captured::{ChangedPaths, FrozenTree, Held};
use slates_vfs::dir::Child;
use slates_vfs::error::VfsError;
use slates_vfs::host::HostFs;
use slates_vfs::ids::InodeNo;
use slates_vfs::inode::{Fingerprint, Kind, WindowsAttributes, Witness};
use slates_vfs::names::NameEquivalence;
use slates_vfs::volume::{Store, Volume};

use crate::source::Source;

/// Whether `path` is `prefix` or lies beneath it.
fn under(path: &str, prefix: &str) -> bool {
  path == prefix || path.starts_with(&format!("{}/", prefix.trim_end_matches('/')))
}

/// Format: the manifest encoding's magic and version.
const MAGIC: &[u8; 4] = b"SLMF";
/// Format: the encoding version; bumped with any change to the layout. 3 since A-451, each entry closed by the volume's
/// Windows attribute bits (1 had none, and 2 is [`VERSION_CONTENT`]'s old number): a grant binds the bits it lands.
const VERSION: u16 = 3;
/// Format: the encoding version of a manifest judged by content ([`Judged::Content`], A-291): the layout of
/// [`VERSION`] with each witness tagged [`WITNESS_CONTENT`], so no grant of one binds a manifest of the other. 4 since
/// A-451, with the bits, as [`VERSION`].
const VERSION_CONTENT: u16 = 4;
/// Format: a witness tag: none.
const WITNESS_NONE: u8 = 0;
/// Format: a witness tag: a fingerprint and an identity ([`Judged::Witness`]).
const WITNESS_FINGERPRINT: u8 = 1;
/// Format: a witness tag: an identity alone ([`Judged::Content`]).
const WITNESS_CONTENT: u8 = 2;
/// Format: the action tags of the encoding, in the order the design lists the actions.
const TAG_CREATE: u8 = 0;
/// Format: see `TAG_CREATE`.
const TAG_REPLACE: u8 = 1;
/// Format: see `TAG_CREATE`.
const TAG_DELETE: u8 = 2;
/// Format: see `TAG_CREATE`.
const TAG_RENAME: u8 = 3;
/// Format: see `TAG_CREATE`.
const TAG_MKDIR: u8 = 4;
/// Format: see `TAG_CREATE`.
const TAG_RMDIR: u8 = 5;
/// Format: see `TAG_CREATE`.
const TAG_SYMLINK: u8 = 6;
/// Format: see `TAG_CREATE`.
const TAG_CLEAR: u8 = 7;
/// Shape: the landing order of §4.15 step 5, lowest first: directories top-down, then creates,
/// then replacements, then directory renames, then file deletes, then directory removals
/// bottom-up, so a tool reading the tree mid-landing never sees a referenced entry missing.
const CLASS_MKDIR: u8 = 0;
/// Shape: see `CLASS_MKDIR`.
const CLASS_CREATE: u8 = 1;
/// Shape: see `CLASS_MKDIR`.
const CLASS_REPLACE: u8 = 2;
/// Shape: see `CLASS_MKDIR`.
const CLASS_RENAME: u8 = 3;
/// Shape: see `CLASS_MKDIR`.
const CLASS_DELETE: u8 = 4;
/// Shape: see `CLASS_MKDIR`.
const CLASS_RMDIR: u8 = 5;

/// What the landing does to one path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
  /// A file the base did not have.
  Create,
  /// A file the base had, replaced.
  Replace,
  /// A file the base had, removed.
  Delete,
  /// A directory renamed on the base, as one rename from its origin.
  Rename {
    /// The origin path on the base.
    from: Box<str>,
  },
  /// A directory created.
  Mkdir,
  /// A directory removed with everything beneath it (a whiteout over a base directory).
  Rmdir,
  /// A symlink created or replaced.
  Symlink {
    /// The target.
    target: Box<str>,
  },
  /// A base directory the overlay removed and recreated opaque: everything beneath it on the
  /// disk goes, the directory itself stays (T-1.12's "one recursive removal").
  Clear,
}

/// What the overlay holds for a file: the identity of its bytes, its size, mode and mtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OverlayIdentity {
  /// BLAKE3 of the bytes.
  pub hash: [u8; 32],
  /// Size in bytes.
  pub size: u64,
  /// Mode bits.
  pub mode: u32,
  /// Modification time, nanoseconds.
  pub mtime_ns: i64,
}

/// How an entry's verdict judges the disk (§4.15 step 4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Judged {
  /// Against the witness the volume took of this host's own base: its fingerprint first, its bytes when that moved
  /// (an overlay over the directory its base reads; a scratch volume, which has none).
  #[default]
  Witness,
  /// By content alone, against what a captured base held (§4.15 `capture_base`, A-291's landing): the witness's
  /// identity is the BLAKE3 of the captured bytes and its fingerprint means nothing, since the target is another
  /// folder than the one the capture read; the disk is hashed for every entry with a witness
  /// ([`crate::verdict::content_verdict`]).
  Content,
}

/// One entry of the manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandingEntry {
  /// The path in the target, absolute from the target root.
  pub path: Box<str>,
  /// The action.
  pub action: Action,
  /// The witnessed base the volume's copy was based on (a fingerprint for deletes and renames
  /// too), or none for a created entry.
  pub witnessed: Option<Witness>,
  /// The overlay's identity, for files.
  pub overlay: Option<OverlayIdentity>,
  /// How its verdict judges the disk.
  pub judged: Judged,
  /// The Windows attribute bits the volume keeps for what it writes (A-262, at Windows' own values): hidden, system,
  /// archive, offline, not indexed, a directory's read-only, and a link's directory kind. A Windows landing applies them
  /// (A-451); Linux and macOS hold none. None for an entry that writes nothing (a delete, a rename, a removal).
  pub windows: WindowsAttributes,
}

impl LandingEntry {
  /// The class the landing orders by (§4.15 step 5), lower first.
  pub fn class(&self) -> u8 {
    match self.action {
      Action::Mkdir | Action::Clear => CLASS_MKDIR,
      Action::Create | Action::Symlink { .. } => CLASS_CREATE,
      Action::Replace => CLASS_REPLACE,
      Action::Rename { .. } => CLASS_RENAME,
      Action::Delete => CLASS_DELETE,
      Action::Rmdir => CLASS_RMDIR,
    }
  }
}

/// Counts by action and by top-level directory, and the bytes to write.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
  /// Entries per action name.
  pub by_action: BTreeMap<Box<str>, usize>,
  /// Entries per top-level directory of the target.
  pub by_top_level: BTreeMap<Box<str>, usize>,
  /// Bytes the landing writes.
  pub bytes: u64,
  /// Entries the filter left out.
  pub filtered_out: usize,
}

/// The manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
  /// The entries, sorted by class then path (the writing order within a class is the path's).
  pub entries: Vec<LandingEntry>,
  /// The summary a human sees.
  pub summary: Summary,
  /// BLAKE3 of the canonical encoding.
  pub hash: [u8; 32],
  /// How its entries are judged: a manifest judged by content is a captured clone's, whose landing leaves the volume
  /// as it was (no base is taken from the target, A-291), even when it lands nothing.
  pub basis: Judged,
}

/// The caller's filter: include and exclude prefixes; an entry is kept when it matches an
/// include (or there are none) and no exclude.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filter {
  /// Path prefixes to keep.
  pub include: Vec<Box<str>>,
  /// Path prefixes to leave out.
  pub exclude: Vec<Box<str>>,
}

impl Filter {
  fn keeps(&self, path: &str) -> bool {
    (self.include.is_empty() || self.include.iter().any(|p| under(path, p)))
      && !self.exclude.iter().any(|p| under(path, p))
  }
}

impl Manifest {
  /// The canonical encoding: little-endian, length-prefixed strings, entries in order. A manifest judged by content
  /// carries its own version and tags each witness as an identity alone.
  pub fn encode(entries: &[LandingEntry], basis: Judged) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    let version = match basis {
      Judged::Witness => VERSION,
      Judged::Content => VERSION_CONTENT,
    };
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(
      &u32::try_from(entries.len())
        .unwrap_or(u32::MAX)
        .to_le_bytes(),
    );
    for e in entries {
      put_str(&mut out, &e.path);
      match &e.action {
        Action::Create => out.push(TAG_CREATE),
        Action::Replace => out.push(TAG_REPLACE),
        Action::Delete => out.push(TAG_DELETE),
        Action::Rename { from } => {
          out.push(TAG_RENAME);
          put_str(&mut out, from);
        }
        Action::Mkdir => out.push(TAG_MKDIR),
        Action::Rmdir => out.push(TAG_RMDIR),
        Action::Symlink { target } => {
          out.push(TAG_SYMLINK);
          put_str(&mut out, target);
        }
        Action::Clear => out.push(TAG_CLEAR),
      }
      match (&e.witnessed, e.judged) {
        (Some(w), Judged::Witness) => {
          out.push(WITNESS_FINGERPRINT);
          put_fingerprint(&mut out, &w.fingerprint);
          out.extend_from_slice(&w.identity);
        }
        (Some(w), Judged::Content) => {
          out.push(WITNESS_CONTENT);
          out.extend_from_slice(&w.identity);
        }
        (None, _) => out.push(WITNESS_NONE),
      }
      match &e.overlay {
        Some(o) => {
          out.push(1);
          out.extend_from_slice(&o.hash);
          out.extend_from_slice(&o.size.to_le_bytes());
          out.extend_from_slice(&o.mode.to_le_bytes());
          out.extend_from_slice(&o.mtime_ns.to_le_bytes());
        }
        None => out.push(0),
      }
      out.extend_from_slice(&e.windows.bits().to_le_bytes());
    }
    out
  }

  /// Builds a manifest from entries: sorts them into landing order, summarizes, hashes.
  pub fn from_entries(entries: Vec<LandingEntry>, filtered_out: usize) -> Self {
    Self::from_judged(entries, filtered_out, Judged::Witness)
  }

  /// [`Manifest::from_entries`] for a manifest whose entries are judged as `basis` says.
  pub fn from_judged(mut entries: Vec<LandingEntry>, filtered_out: usize, basis: Judged) -> Self {
    entries.sort_by(|a, b| a.class().cmp(&b.class()).then_with(|| a.path.cmp(&b.path)));
    // Removals go bottom-up: deeper paths first within their class.
    let (mut keep, mut removals): (Vec<LandingEntry>, Vec<LandingEntry>) = entries
      .into_iter()
      .partition(|e| !matches!(e.action, Action::Delete | Action::Rmdir));
    removals.sort_by(|a, b| {
      a.class()
        .cmp(&b.class())
        .then_with(|| {
          b.path
            .matches('/')
            .count()
            .cmp(&a.path.matches('/').count())
        })
        .then_with(|| a.path.cmp(&b.path))
    });
    keep.append(&mut removals);
    let entries = keep;
    let mut summary = Summary {
      filtered_out,
      ..Summary::default()
    };
    for e in &entries {
      let action = match &e.action {
        Action::Create => "create",
        Action::Replace => "replace",
        Action::Delete => "delete",
        Action::Rename { .. } => "rename",
        Action::Mkdir => "mkdir",
        Action::Rmdir => "rmdir",
        Action::Symlink { .. } => "symlink",
        Action::Clear => "clear",
      };
      let actions = summary.by_action.entry(action.into()).or_insert(0);
      *actions = actions.saturating_add(1);
      let top = e
        .path
        .trim_start_matches('/')
        .split('/')
        .next()
        .unwrap_or("");
      let tops = summary.by_top_level.entry(top.into()).or_insert(0);
      *tops = tops.saturating_add(1);
      summary.bytes = summary
        .bytes
        .saturating_add(e.overlay.map_or(0, |o| o.size));
    }
    let hash = *blake3::hash(&Self::encode(&entries, basis)).as_bytes();
    Self {
      entries,
      summary,
      hash,
      basis,
    }
  }

  /// The hash as hex.
  pub fn hash_hex(&self) -> String {
    self.hash.iter().map(|b| format!("{b:02x}")).collect()
  }
}

fn put_str(out: &mut Vec<u8>, s: &str) {
  out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(u16::MAX).to_le_bytes());
  out.extend_from_slice(s.as_bytes());
}

fn put_fingerprint(out: &mut Vec<u8>, fp: &Fingerprint) {
  out.extend_from_slice(&fp.dev.to_le_bytes());
  out.extend_from_slice(&fp.ino.to_le_bytes());
  out.extend_from_slice(&fp.size.to_le_bytes());
  out.extend_from_slice(&fp.mtime_ns.to_le_bytes());
  out.extend_from_slice(&fp.ctime_ns.to_le_bytes());
  out.extend_from_slice(&fp.mode.to_le_bytes());
}

/// Plans the manifest of `source` — the volume's head or one of its snapshots (A-49): the diverged entries
/// with their witnesses and the overlay's identities (the bytes are hashed here, once, proportional to the
/// delta), every one of them as the source holds it. [`PlanWalk`] run to its end, in one call.
pub fn plan(
  vol: &mut Volume,
  store: &mut Store,
  host: &mut dyn HostFs,
  filter: &Filter,
  source: Source,
) -> Result<Manifest, VfsError> {
  let mut walk = PlanWalk::new(vol, source)?;
  loop {
    if let Some(manifest) = walk.unit(vol, store, host, filter)? {
      return Ok(manifest);
    }
  }
}

/// A landing's plan in progress (§4.15 step 1; §4.3 "Bounded work everywhere"): the diverged set walked a directory a
/// unit, then each diverged entry planned a unit (a file read and its identity hashed), then the manifest built, so a
/// large landing is planned over many of a shard's steps instead of one poll (a presentation of 1,000 files of 16 KiB
/// held its shard for one 6.2 s plan, 2026-11-08). It holds no borrow between units; what it walks must not move under
/// it, so a sliced plan walks a snapshot (the head's, taken as the landing begins).
#[derive(Debug)]
pub struct PlanWalk {
  source: Source,
  stage: PlanStage,
}

#[derive(Debug)]
enum PlanStage {
  /// The diverged set being walked.
  Diverged(slates_vfs::base::DivergedWalk),
  /// Each diverged entry being planned, in path order.
  Entries {
    diverged: Vec<Diverged>,
    /// The origins of the renamed base directories: a whiteout at one is implied by its rename.
    origins: Vec<Box<str>>,
    next: usize,
    entries: Vec<LandingEntry>,
    filtered_out: usize,
  },
  /// A clone of a captured base: the paths where its source parts from the capture, being found.
  CaptureChanges {
    /// The captured snapshot's tree.
    base: FrozenTree,
    /// The source's tree (a snapshot's, for a sliced plan).
    source: FrozenTree,
    /// The search.
    changed: ChangedPaths,
  },
  /// Each candidate judged against the capture, in path order.
  CaptureEntries {
    base: FrozenTree,
    source: FrozenTree,
    paths: Vec<String>,
    next: usize,
    entries: Vec<LandingEntry>,
    filtered_out: usize,
  },
  /// Ended: the manifest was given out.
  Ended,
}

impl PlanWalk {
  /// A plan of `source` from its start.
  pub fn new(vol: &Volume, source: Source) -> Result<PlanWalk, VfsError> {
    let walk = match source {
      Source::Head => vol.diverged_walk(),
      Source::Snapshot(id) => vol.diverged_walk_in(id)?,
    };
    Ok(PlanWalk {
      source,
      stage: PlanStage::Diverged(walk),
    })
  }

  /// A plan of `source` judged against the captured base `base` (§4.15 `capture_base`, A-291's landing): the paths
  /// where `source` parts from the capture, each an entry judged by content ([`Judged::Content`]). For a clone of a
  /// captured snapshot, which shares that snapshot's nodes; `base` must stay frozen while the plan runs (a capture is
  /// pinned while a clone of it lives).
  pub fn against_capture(
    vol: &Volume,
    source: Source,
    base: FrozenTree,
  ) -> Result<PlanWalk, VfsError> {
    let tree = match source {
      Source::Head => vol.head_tree(),
      Source::Snapshot(id) => vol.frozen_tree(id)?,
    };
    Ok(PlanWalk {
      source,
      stage: PlanStage::CaptureChanges {
        base,
        source: tree,
        changed: ChangedPaths::new(base, tree),
      },
    })
  }

  /// One unit: a directory of the diverged walk, one entry planned, or the manifest built; the manifest once built.
  pub fn unit(
    &mut self,
    vol: &mut Volume,
    store: &mut Store,
    host: &mut dyn HostFs,
    filter: &Filter,
  ) -> Result<Option<Manifest>, VfsError> {
    let source = self.source;
    match std::mem::replace(&mut self.stage, PlanStage::Ended) {
      PlanStage::Diverged(mut walk) => {
        self.stage = match vol.diverged_unit(store, &mut walk) {
          Some(diverged) => {
            let origins: Vec<Box<str>> = diverged
              .iter()
              .filter(|d| d.kind == Divergence::Redirect)
              .filter_map(|d| origin_of(vol, store, source, &d.path))
              .collect();
            PlanStage::Entries {
              diverged,
              origins,
              next: 0,
              entries: Vec::new(),
              filtered_out: 0,
            }
          }
          None => PlanStage::Diverged(walk),
        };
        Ok(None)
      }
      PlanStage::Entries {
        diverged,
        origins,
        next,
        mut entries,
        mut filtered_out,
      } => {
        let Some(d) = diverged.get(next) else {
          return Ok(Some(Manifest::from_entries(entries, filtered_out)));
        };
        if !filter.keeps(&d.path) {
          filtered_out = filtered_out.saturating_add(1);
        } else if d.kind == Divergence::Whiteout
          && origins.iter().any(|o| o.as_ref() == d.path.as_str())
        {
          // A renamed base directory implies the whiteout at its origin; that whiteout is not a separate removal.
        } else if let Some(entry) = entry_for(vol, store, host, source, d)? {
          entries.push(entry);
        }
        self.stage = PlanStage::Entries {
          diverged,
          origins,
          next: next.saturating_add(1),
          entries,
          filtered_out,
        };
        Ok(None)
      }
      PlanStage::CaptureChanges {
        base,
        source,
        mut changed,
      } => {
        self.stage = match changed.unit(store)? {
          Some(paths) => PlanStage::CaptureEntries {
            base,
            source,
            paths,
            next: 0,
            entries: Vec::new(),
            filtered_out: 0,
          },
          None => PlanStage::CaptureChanges {
            base,
            source,
            changed,
          },
        };
        Ok(None)
      }
      PlanStage::CaptureEntries {
        base,
        source,
        paths,
        next,
        mut entries,
        mut filtered_out,
      } => {
        let Some(path) = paths.get(next) else {
          return Ok(Some(Manifest::from_judged(
            entries,
            filtered_out,
            Judged::Content,
          )));
        };
        if filter.keeps(path) {
          let trees = (base, source);
          entries.extend(capture_entries(store, vol.policy(), trees, path)?);
        } else {
          filtered_out = filtered_out.saturating_add(1);
        }
        self.stage = PlanStage::CaptureEntries {
          base,
          source,
          paths,
          next: next.saturating_add(1),
          entries,
          filtered_out,
        };
        Ok(None)
      }
      PlanStage::Ended => Err(VfsError::Invalid),
    }
  }
}

/// A witness judged by content: the identity of what a capture held, and no fingerprint ([`Judged::Content`]).
fn content_witness(identity: [u8; 32]) -> Witness {
  Witness {
    fingerprint: Fingerprint::default(),
    identity,
    witnessed_at: 0,
    racy: false,
  }
}

/// The BLAKE3 of file `no`'s bytes in `tree`, read whole as the overlay's plan reads a file.
fn tree_identity(
  store: &Store,
  tree: FrozenTree,
  no: InodeNo,
  size: u64,
) -> Result<[u8; 32], VfsError> {
  let mut bytes = vec![0u8; usize::try_from(size).map_err(|_| VfsError::FileTooLarge)?];
  let read = slates_vfs::captured::read_file(store, tree, no, 0, &mut bytes)?;
  bytes.truncate(read);
  Ok(*blake3::hash(&bytes).as_bytes())
}

/// What the capture held at a path, as a witness judged by content: a file's bytes' identity, a symlink's target's.
fn captured_identity(store: &Store, base: FrozenTree, held: &Held) -> Result<[u8; 32], VfsError> {
  match held {
    Held::File { no, size, .. } => tree_identity(store, base, *no, *size),
    Held::Symlink { target, .. } => Ok(*blake3::hash(target.as_bytes()).as_bytes()),
    Held::Absent | Held::Dir { .. } | Held::Special => Ok([0; 32]),
  }
}

/// The entries one candidate path makes (§4.15 step 1 for a captured clone, A-291): none where the capture and the
/// source hold the same, else what the source holds there written over what the capture held. A file or symlink of
/// the capture the source no longer holds is removed (`Delete`), a directory of it removed once emptied (`Rmdir`); a
/// name whose kind changed is both a removal and a making, which the landing's class order meets as a type conflict
/// (owed: a kind change lands as one entry). A symlink pointed elsewhere is one `Symlink`, as an overlay's is.
fn capture_entries(
  store: &Store,
  policy: NameEquivalence,
  (base, source): (FrozenTree, FrozenTree),
  path: &str,
) -> Result<Vec<LandingEntry>, VfsError> {
  let was = slates_vfs::captured::held_at(store, base, policy, path)?;
  let now = slates_vfs::captured::held_at(store, source, policy, path)?;
  let entry = |action: Action,
               witnessed: Option<Witness>,
               overlay: Option<OverlayIdentity>,
               windows: WindowsAttributes| LandingEntry {
    path: path.into(),
    action,
    witnessed,
    overlay,
    judged: Judged::Content,
    windows,
  };
  match (&was, &now) {
    (Held::Absent, Held::Absent) | (Held::Dir { .. }, Held::Dir { .. }) => Ok(Vec::new()),
    (
      Held::File { .. },
      Held::File {
        no,
        size,
        mode,
        mtime_ns,
        windows,
      },
    ) => {
      let captured = captured_identity(store, base, &was)?;
      let hash = tree_identity(store, source, *no, *size)?;
      Ok(if hash == captured {
        Vec::new()
      } else {
        vec![entry(
          Action::Replace,
          Some(content_witness(captured)),
          Some(OverlayIdentity {
            hash,
            size: *size,
            mode: *mode,
            mtime_ns: *mtime_ns,
          }),
          *windows,
        )]
      })
    }
    (
      Held::Symlink { target: before, .. },
      Held::Symlink { target, windows },
    ) => Ok(if before == target {
      Vec::new()
    } else {
      vec![entry(
        Action::Symlink {
          target: target.clone(),
        },
        None,
        None,
        *windows,
      )]
    }),
    _ => {
      let mut out = Vec::new();
      if let Some((action, witness)) = removal_of(store, base, &was)? {
        out.push(entry(action, Some(witness), None, WindowsAttributes::default()));
      }
      if let Some((action, overlay, windows)) = made_entry(store, source, &now)? {
        out.push(entry(action, None, overlay, windows));
      }
      Ok(out)
    }
  }
}

/// What removing what the capture held at a path does: a file, symlink or special name deleted while it is still what
/// the capture held, a directory removed once emptied; nothing for a name the capture did not hold.
fn removal_of(
  store: &Store,
  base: FrozenTree,
  was: &Held,
) -> Result<Option<(Action, Witness)>, VfsError> {
  Ok(match was {
    Held::Absent => None,
    Held::Dir { .. } => Some((Action::Rmdir, content_witness([0; 32]))),
    Held::File { .. } | Held::Symlink { .. } | Held::Special => Some((
      Action::Delete,
      content_witness(captured_identity(store, base, was)?),
    )),
  })
}

/// What making `held` writes: its action, for a file or directory its identity, and its Windows bits (A-451).
fn made_entry(
  store: &Store,
  source: FrozenTree,
  held: &Held,
) -> Result<Option<(Action, Option<OverlayIdentity>, WindowsAttributes)>, VfsError> {
  Ok(match held {
    Held::Absent => None,
    Held::File {
      no,
      size,
      mode,
      mtime_ns,
      windows,
    } => Some((
      Action::Create,
      Some(OverlayIdentity {
        hash: tree_identity(store, source, *no, *size)?,
        size: *size,
        mode: *mode,
        mtime_ns: *mtime_ns,
      }),
      *windows,
    )),
    Held::Dir {
      mode,
      mtime_ns,
      windows,
    } => Some((
      Action::Mkdir,
      Some(OverlayIdentity {
        hash: [0; 32],
        size: 0,
        mode: *mode,
        mtime_ns: *mtime_ns,
      }),
      *windows,
    )),
    Held::Symlink { target, windows } => Some((
      Action::Symlink {
        target: target.clone(),
      },
      None,
      *windows,
    )),
    Held::Special => return Err(VfsError::SpecialFileOperation),
  })
}

fn origin_of(vol: &Volume, store: &Store, source: Source, path: &str) -> Option<Box<str>> {
  match source.resolve(vol, store, path).ok()?.child {
    Child::Dir(h) => store.dirs.get(h).ok()?.origin.clone(),
    _ => None,
  }
}

/// The manifest entry for one diverged path.
fn entry_for(
  vol: &mut Volume,
  store: &mut Store,
  host: &mut dyn HostFs,
  source: Source,
  d: &Diverged,
) -> Result<Option<LandingEntry>, VfsError> {
  let path: Box<str> = d.path.as_str().into();
  match d.kind {
    Divergence::Whiteout => {
      let (dir, name) = split(&d.path);
      let witnessed = source
        .whiteout_witness(vol, store, dir, name)
        .map(|fp| Witness {
          fingerprint: fp,
          identity: [0; 32],
          witnessed_at: 0,
          racy: false,
        });
      let action = if witnessed.is_some_and(|w| kind_is_dir(w.fingerprint.mode)) {
        Action::Rmdir
      } else {
        Action::Delete
      };
      Ok(Some(LandingEntry {
        path,
        action,
        witnessed,
        overlay: None,
        judged: Judged::Witness,
        windows: WindowsAttributes::default(),
      }))
    }
    Divergence::Redirect => {
      let from = origin_of(vol, store, source, &d.path).ok_or(VfsError::Invalid)?;
      let witnessed = source
        .redirect_witness(vol, store, &d.path)
        .map(|fp| Witness {
          fingerprint: fp,
          identity: [0; 32],
          witnessed_at: 0,
          racy: false,
        });
      Ok(Some(LandingEntry {
        path,
        action: Action::Rename { from },
        witnessed,
        overlay: None,
        judged: Judged::Witness,
        windows: WindowsAttributes::default(),
      }))
    }
    Divergence::Created | Divergence::Witnessed => {
      let located = source.resolve(vol, store, &d.path)?;
      match located.child {
        Child::Dir(_) => {
          let attrs = source.stat(vol, store, located.inode)?;
          // An opaque directory over a whiteout of a base directory clears the disk's copy.
          let (dir, name) = split(&d.path);
          let cleared = source
            .whiteout_witness(vol, store, dir, name)
            .map(|fp| Witness {
              fingerprint: fp,
              identity: [0; 32],
              witnessed_at: 0,
              racy: false,
            });
          Ok(Some(LandingEntry {
            path,
            action: if cleared.is_some() {
              Action::Clear
            } else {
              Action::Mkdir
            },
            witnessed: cleared,
            overlay: Some(OverlayIdentity {
              hash: [0; 32],
              size: 0,
              mode: attrs.mode,
              mtime_ns: attrs.mtime,
            }),
            judged: Judged::Witness,
            windows: source.windows(vol, store, located.inode)?,
          }))
        }
        Child::Symlink(no) => Ok(Some(LandingEntry {
          path,
          action: Action::Symlink {
            target: source.readlink(vol, store, no)?,
          },
          witnessed: None,
          overlay: None,
          judged: Judged::Witness,
          windows: source.windows(vol, store, no)?,
        })),
        Child::Fifo(_) | Child::Socket(_) | Child::BlockDevice(_) | Child::CharDevice(_) => {
          Err(VfsError::SpecialFileOperation)
        }
        Child::File(no) => {
          let attrs = source.stat(vol, store, no)?;
          let mut bytes =
            vec![0u8; usize::try_from(attrs.size).map_err(|_| VfsError::FileTooLarge)?];
          let n = source.read(vol, store, host, no, &mut bytes)?;
          bytes.truncate(n);
          let witnessed = source.witness(vol, no);
          Ok(Some(LandingEntry {
            path,
            action: if witnessed.is_some() {
              Action::Replace
            } else {
              Action::Create
            },
            witnessed,
            overlay: Some(OverlayIdentity {
              hash: *blake3::hash(&bytes).as_bytes(),
              size: attrs.size,
              mode: attrs.mode,
              mtime_ns: attrs.mtime,
            }),
            judged: Judged::Witness,
            windows: source.windows(vol, store, no)?,
          }))
        }
        Child::Whiteout => Ok(None),
      }
    }
  }
}

fn split(path: &str) -> (&str, &str) {
  match path.rsplit_once('/') {
    Some(("", name)) => ("/", name),
    Some((dir, name)) => (dir, name),
    None => ("/", path),
  }
}

/// Format: the directory bit of a POSIX mode.
const S_IFMT: u32 = 0o170_000;
/// Format: the directory type in a POSIX mode.
const S_IFDIR: u32 = 0o040_000;

pub(crate) fn kind_is_dir(mode: u32) -> bool {
  mode & S_IFMT == S_IFDIR
}

/// The kind a fingerprint's mode names, for the verdict.
pub fn kind_of_mode(mode: u32) -> Kind {
  if kind_is_dir(mode) {
    Kind::Dir
  } else {
    Kind::File
  }
}

#[cfg(test)]
mod tests {
  use super::under;

  /// CVE-2025-53110 ("EscapeRoute", Anthropic's Filesystem MCP) replicated against the landing filter's prefix
  /// check. There a naive `path.starts_with(root)` let a sibling like `/root-evil` match the boundary `/root`.
  /// slates' `under` compares against the exact prefix or `prefix + "/"`, so a sibling whose name merely extends
  /// the prefix is NOT under it — `/root-evil`, `/rootx` and `/roo` are not under `/root` — while the prefix
  /// itself and its genuine descendants are, with or without a trailing slash on the prefix.
  #[test]
  fn a_sibling_sharing_the_prefix_is_not_under_it() {
    // The exact bypass the CVE exploited: a sibling extending the prefix.
    assert!(!under("/root-evil", "/root"));
    assert!(!under("/rootx", "/root"));
    assert!(!under("/roo", "/root"));
    assert!(!under("/other", "/root"));
    // The prefix itself and its genuine descendants are under it.
    assert!(under("/root", "/root"));
    assert!(under("/root/file", "/root"));
    assert!(under("/root/a/b", "/root/"));
  }
}
