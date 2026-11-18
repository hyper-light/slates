//! An in-memory stand-in for an instance behind the [`Volumes`] seam (§4.12 "The seam", A-310): what a host's own
//! simulated cluster drives where no daemon runs (Hex's `hex-sim`). It answers each verb with the outcome or the typed
//! [`Error`] an [`crate::Instance`] answers for the same request, for every case the conformance suite exercises
//! (`crates/api/tests/seam.rs`, which runs the one suite over both), and keeps nothing on disk and starts no daemon.
//!
//! **What it models.** Volumes by name and id, each with its kind (plain, overlay, green, work) and its snapshots;
//! a clone pins the snapshot it was made from, as the volume core's clone pin does (§4.4), so that snapshot's destroy is
//! refused while the clone lives; a green over a snapshot holds that snapshot's volume, whose destroy is refused while
//! the green lives (A-217). Names are unique among the volumes, as they are among a caller's.
//!
//! **What it does not.** It serves no file system. A stream [`SimVolumes::serve_shards`] hands out is one end of a
//! real connected pair (a socket pair on Unix, a duplex pipe on Windows) whose other end the simulator holds and never
//! answers, until a detach or the volume's destroy closes it, when its peer reads the end. A mount is a record of one:
//! [`SimVolumes::mount`] answers the path as given and touches nothing at it. A green's version is its first (no merge
//! is simulated), and every volume's bytes are none. A host test that needs a guest's bytes runs a real instance.
//!
//! **Bounds.** Each table (volumes, attachments) holds at most what its constructor was given, the caller's own bound,
//! and refuses past it [`Exhausted::Table`]; ids come from one counter, refused the same way at its last value.

use std::collections::BTreeMap;

use slates_client::{Attachment, ShardsStream, StreamEnd};
use slates_ipc::protocol::{
  AttachTransport, AttachmentCapability, Conformance, DeleteWhileOpen, Established, Intent,
  KernelCache, ReadWritePolicy, Residency, SharingSemantics, SnapshotId, TargetPathConstraint,
  VolumeId, VolumeSummary,
};

use crate::error::{Error, Exhausted, Existing, Invalid, KindMismatch, Missing, Note};
use crate::mount::{MountOptions, Mounted};
use crate::outcome::Work;
use crate::seam::Volumes;
use crate::volume::{Provisioned, Source, VolumeSpec};

/// Format: what a refused destroy of a pinned snapshot, or of a volume a green's origin holds, carries as its note:
/// the daemon answers both as the volume core's `Pinned` with no typed class of its own yet (A-310, owed to the
/// contract), and the simulator answers what the daemon does.
const PINNED: &str = "Pinned";
/// Format: the table name an exhausted simulator's refusal carries.
const VOLUMES_TABLE: &str = "the simulator's volumes";
/// Format: the table name an exhausted simulator's refusal carries for its attachments.
const ATTACHMENTS_TABLE: &str = "the simulator's attachments";
/// Format: the table name a simulator whose id counter reached its last value refuses with.
const IDS_TABLE: &str = "the simulator's ids";
/// Format: the instance a simulator's channel failure names: no daemon's, so none is retried.
const SIMULATOR: &str = "the simulator";
/// Format: the pause a channel failure would retry after: none, since the simulator's are its host's refusals (a
/// socket pair the OS would not make), never a daemon's load.
const NO_PAUSE_NS: u64 = 0;

/// What kind a simulated volume is, as far as the seam's verbs tell kinds apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
  /// Empty, or a clone.
  Plain,
  /// Laid over a host folder.
  Overlay,
  /// A green.
  Green,
  /// A work volume over a green.
  Work,
}

/// One simulated volume.
#[derive(Debug)]
struct SimVolume {
  /// Its name.
  name: String,
  /// Its kind.
  kind: Kind,
  /// Its snapshots, by value, each with the clones that pin it.
  snapshots: BTreeMap<u64, u32>,
  /// The snapshot a clone was made from, which it pins.
  origin: Option<(VolumeId, SnapshotId)>,
  /// The snapshot a green was made over, whose volume it holds.
  green_base: Option<(VolumeId, SnapshotId)>,
}

/// What a simulated attachment holds.
#[derive(Debug)]
enum Held {
  /// A Shards stream's end the simulator keeps and never answers.
  Stream {
    /// The simulator's end; dropped, its peer reads the end.
    _end: StreamEnd,
  },
  /// A mount's record: its path.
  Mount {
    /// The path the mount was asked at.
    path: String,
  },
}

/// One simulated attachment.
#[derive(Debug)]
struct SimAttachment {
  /// The volume it serves.
  volume: VolumeId,
  /// What it holds.
  held: Held,
}

/// The simulator (see the module's notes).
#[derive(Debug)]
pub struct SimVolumes {
  /// The volumes, by id.
  volumes: BTreeMap<VolumeId, SimVolume>,
  /// The attachments, by id.
  attachments: BTreeMap<u64, SimAttachment>,
  /// The ids issued so far, volumes', snapshots' and attachments' alike.
  issued: u64,
  /// The most volumes and the most attachments it holds at once: the caller's bound.
  most: usize,
}

impl SimVolumes {
  /// A simulator holding at most `most` volumes and at most `most` attachments at once, and nothing yet.
  pub fn new(most: usize) -> SimVolumes {
    SimVolumes {
      volumes: BTreeMap::new(),
      attachments: BTreeMap::new(),
      issued: 0,
      most,
    }
  }

  /// The next id, refused at the counter's last value.
  fn next_id(&mut self) -> Result<u64, Error> {
    let next = self.issued.checked_add(1).ok_or_else(|| table(IDS_TABLE))?;
    self.issued = next;
    Ok(next)
  }

  /// A fresh volume id.
  fn next_volume(&mut self) -> Result<VolumeId, Error> {
    let id = self.next_id()?;
    let mut bytes = [0u8; 16];
    for (slot, byte) in bytes.iter_mut().zip(id.to_le_bytes()) {
      *slot = byte;
    }
    Ok(VolumeId { bytes })
  }

  /// The volume named `name`, if one is.
  fn named(&self, name: &str) -> Option<VolumeId> {
    self
      .volumes
      .iter()
      .find_map(|(id, volume)| (volume.name == name).then_some(*id))
  }

  /// Makes a volume of `kind` named `name`: refused if the name is taken or the table is full.
  fn make(
    &mut self,
    name: &str,
    kind: Kind,
    origin: Option<(VolumeId, SnapshotId)>,
    green_base: Option<(VolumeId, SnapshotId)>,
  ) -> Result<VolumeId, Error> {
    if let Some(existing) = self.named(name) {
      return Err(Error::AlreadyExists(Existing::Volume { existing }));
    }
    if self.volumes.len() >= self.most {
      return Err(table(VOLUMES_TABLE));
    }
    let id = self.next_volume()?;
    self.volumes.insert(
      id,
      SimVolume {
        name: name.to_owned(),
        kind,
        snapshots: BTreeMap::new(),
        origin,
        green_base,
      },
    );
    Ok(id)
  }

  /// Whether `volume` holds `snapshot`.
  fn holds(&self, volume: VolumeId, snapshot: SnapshotId) -> bool {
    self
      .volumes
      .get(&volume)
      .is_some_and(|held| held.snapshots.contains_key(&snapshot.value))
  }

  /// A clone of `snapshot` of `volume` named `name`, pinning the snapshot.
  fn clone_volume(
    &mut self,
    name: &str,
    volume: VolumeId,
    snapshot: SnapshotId,
  ) -> Result<VolumeId, Error> {
    if !self.holds(volume, snapshot) {
      return Err(Error::NotFound(Missing::Snapshot { volume, snapshot }));
    }
    let clone = self.make(name, Kind::Plain, Some((volume, snapshot)), None)?;
    if let Some(pins) = self
      .volumes
      .get_mut(&volume)
      .and_then(|origin| origin.snapshots.get_mut(&snapshot.value))
    {
      *pins = pins.saturating_add(1);
    }
    Ok(clone)
  }

  /// A green named `name`, empty or over `base`, which must not be served live from a host folder.
  fn green(
    &mut self,
    name: &str,
    base: Option<slates_ipc::protocol::GreenBase>,
  ) -> Result<VolumeId, Error> {
    let Some(base) = base else {
      return self.make(name, Kind::Green, None, None);
    };
    let Some(origin) = self.volumes.get(&base.volume) else {
      return Err(Error::NotFound(Missing::Snapshot {
        volume: base.volume,
        snapshot: base.snapshot,
      }));
    };
    if !origin.snapshots.contains_key(&base.snapshot.value) {
      return Err(Error::NotFound(Missing::Snapshot {
        volume: base.volume,
        snapshot: base.snapshot,
      }));
    }
    if origin.kind == Kind::Overlay {
      return Err(Error::Invalid(Invalid::LiveBase));
    }
    self.make(name, Kind::Green, None, Some((base.volume, base.snapshot)))
  }

  /// A work volume named `name` over `green`, at its first version.
  fn work(&mut self, name: &str, green: VolumeId) -> Result<Provisioned, Error> {
    let Some(over) = self.volumes.get(&green) else {
      return Err(Error::NotFound(Missing::Volume(green)));
    };
    if over.kind != Kind::Green {
      return Err(Error::Invalid(Invalid::Kind(KindMismatch::NotGreen)));
    }
    let volume = self.make(name, Kind::Work, None, None)?;
    Ok(Provisioned::Work {
      work: Work { volume, green },
      base: 0,
    })
  }

  /// A new attachment of `volume` holding `held`: its id, refused when the table is full.
  fn attach(&mut self, volume: VolumeId, held: Held) -> Result<u64, Error> {
    if self.attachments.len() >= self.most {
      return Err(table(ATTACHMENTS_TABLE));
    }
    let id = self.next_id()?;
    self.attachments.insert(id, SimAttachment { volume, held });
    Ok(id)
  }
}

/// The refusal of a full table named `name`.
fn table(name: &str) -> Error {
  Error::Exhausted(Exhausted::Table {
    table: Note::new(name),
  })
}

/// The answer for a pinned snapshot's destroy, or a held volume's: the daemon's, untyped yet (see [`PINNED`]).
fn pinned() -> Error {
  Error::Invalid(Invalid::Request(Note::new(PINNED)))
}

/// A connected pair: the simulator's end and the end it hands out.
#[cfg(unix)]
fn stream_pair() -> Result<(StreamEnd, StreamEnd), Error> {
  slates_ipc::stream::pair().map_err(|e| Error::of_ipc(SIMULATOR, e, NO_PAUSE_NS))
}

/// A connected pair: the simulator's end and the end it hands out. Nothing is ever written to it by the simulator, so
/// it buffers nothing (the system's default): a peer's write waits, as one to a server that never reads does.
#[cfg(windows)]
fn stream_pair() -> Result<(StreamEnd, StreamEnd), Error> {
  /// Format: no buffering: nothing reads the simulator's end.
  const UNBUFFERED: usize = 0;
  slates_ipc::stream_pipe::pair(UNBUFFERED).map_err(|e| Error::of_ipc(SIMULATOR, e, NO_PAUSE_NS))
}

/// What a simulated stream reports of itself, as the daemon reports a Shards stream (its `transports::shards_fs`), but
/// with no evidence claimed: nothing serves it.
fn stream_capability(intent: Intent) -> AttachmentCapability {
  AttachmentCapability {
    transport: AttachTransport::ShardsFs,
    supported: true,
    unsupported_reason: None,
    target_path: TargetPathConstraint::GuestTag,
    read_write: match intent {
      Intent::Write => ReadWritePolicy::ReadWrite,
      Intent::Read => ReadWritePolicy::ReadOnly,
    },
    sharing: SharingSemantics {
      one_owning_shard: true,
      server_open_state: true,
      cache: KernelCache::Negotiated {
        writeback: false,
        explicit_invalidation: false,
      },
      delete_while_open: DeleteWhileOpen::Unlinked,
    },
    residency: Residency::DaemonRamAndGuestPageCache { dax_mapped: false },
    conformance: Conformance::None,
  }
}

impl Volumes for SimVolumes {
  fn provision(&mut self, spec: &VolumeSpec) -> Result<Provisioned, Error> {
    let name = spec.name.as_str();
    match &spec.source {
      Source::Overlay { folder, .. } => {
        crate::absolute_text(folder)?;
        self
          .make(name, Kind::Overlay, None, None)
          .map(Provisioned::Volume)
      }
      Source::Empty { .. } => self
        .make(name, Kind::Plain, None, None)
        .map(Provisioned::Volume),
      Source::Clone { volume, snapshot } => self
        .clone_volume(name, *volume, *snapshot)
        .map(Provisioned::Volume),
      Source::Green { base, .. } => self.green(name, *base).map(Provisioned::Volume),
      Source::Work { green } => self.work(name, *green),
    }
  }

  fn snapshot(&mut self, volume: VolumeId) -> Result<SnapshotId, Error> {
    if !self.volumes.contains_key(&volume) {
      return Err(Error::NotFound(Missing::Volume(volume)));
    }
    let snapshot = SnapshotId {
      value: self.next_id()?,
    };
    if let Some(held) = self.volumes.get_mut(&volume) {
      held.snapshots.insert(snapshot.value, 0);
    }
    Ok(snapshot)
  }

  fn destroy_snapshot(&mut self, volume: VolumeId, snapshot: SnapshotId) -> Result<(), Error> {
    let Some(pins) = self
      .volumes
      .get(&volume)
      .and_then(|held| held.snapshots.get(&snapshot.value))
    else {
      return Err(Error::NotFound(Missing::Snapshot { volume, snapshot }));
    };
    if *pins > 0 {
      return Err(pinned());
    }
    if let Some(held) = self.volumes.get_mut(&volume) {
      held.snapshots.remove(&snapshot.value);
    }
    Ok(())
  }

  fn destroy(&mut self, volume: VolumeId) -> Result<(), Error> {
    if !self.volumes.contains_key(&volume) {
      return Err(Error::NotFound(Missing::Volume(volume)));
    }
    // A green over one of this volume's snapshots holds the volume (A-217).
    if self
      .volumes
      .values()
      .any(|other| other.green_base.is_some_and(|(held, _)| held == volume))
    {
      return Err(pinned());
    }
    let gone = self.volumes.remove(&volume);
    // Its clone pin goes with it; its attachments end, a stream's peer reading the end as its end drops.
    if let Some((origin, snapshot)) = gone.and_then(|gone| gone.origin)
      && let Some(pins) = self
        .volumes
        .get_mut(&origin)
        .and_then(|held| held.snapshots.get_mut(&snapshot.value))
    {
      *pins = pins.saturating_sub(1);
    }
    self
      .attachments
      .retain(|_, attachment| attachment.volume != volume);
    Ok(())
  }

  fn list(&mut self) -> Result<Vec<VolumeSummary>, Error> {
    Ok(
      self
        .volumes
        .iter()
        .map(|(id, volume)| VolumeSummary {
          id: *id,
          name: volume.name.clone(),
          referenced_bytes: 0,
          unique_bytes: 0,
          overlay: volume.kind == Kind::Overlay,
          live_shipped: false,
          not_recovered: None,
        })
        .collect(),
    )
  }

  fn serve_shards(
    &mut self,
    volume: VolumeId,
    intent: Intent,
    _subtree: Option<&str>,
  ) -> Result<ShardsStream, Error> {
    if !self.volumes.contains_key(&volume) {
      return Err(Error::NotFound(Missing::Volume(volume)));
    }
    let (kept, handed) = stream_pair()?;
    let attachment = self.attach(volume, Held::Stream { _end: kept })?;
    Ok(ShardsStream {
      attachment: Attachment {
        attachment,
        lease_epoch: None,
        path: None,
        version: None,
        established: Established::Record,
        capability: stream_capability(intent),
        token: None,
      },
      stream: handed,
      // The simulator holds no channel a successor could resync the stream over: a consumer handles none, as it does on
      // the platforms whose daemon hands none (A-316).
      resync: None,
    })
  }

  fn mount(
    &mut self,
    volume: VolumeId,
    path: &str,
    _options: &MountOptions,
  ) -> Result<Mounted, Error> {
    if !self.volumes.contains_key(&volume) {
      return Err(Error::NotFound(Missing::Volume(volume)));
    }
    let attachment = self.attach(
      volume,
      Held::Mount {
        path: path.to_owned(),
      },
    )?;
    Ok(Mounted {
      attachment,
      path: path.to_owned(),
    })
  }

  fn unmount(&mut self, path: &str) -> Result<(), Error> {
    self
      .attachments
      .retain(|_, attachment| !matches!(&attachment.held, Held::Mount { path: at } if at == path));
    Ok(())
  }

  fn detach(&mut self, attachment: u64) -> Result<(), Error> {
    self
      .attachments
      .remove(&attachment)
      .map(|_| ())
      .ok_or(Error::NotFound(Missing::Attachment { attachment }))
  }
}
