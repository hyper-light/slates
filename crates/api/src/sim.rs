//! An in-memory stand-in for an instance behind the [`Volumes`] seam (§4.12 "The seam", A-310): what a host's own
//! simulated cluster drives where no daemon runs (Hex's `hex-sim`). It answers each verb with the outcome or the typed
//! [`Error`] an [`crate::Instance`] answers for the same request, for every case the conformance suite exercises
//! (`the_seam_conforms` in `crates/api/tests/api.rs`, which runs the one suite over both), and keeps nothing on disk and starts no daemon.
//!
//! **What it models.** Volumes by name and id, each with its kind (plain, overlay, green, work) and its snapshots;
//! a clone pins the snapshot it was made from, as the volume core's clone pin does (§4.4), so that snapshot's destroy is
//! refused while the clone lives; a green over a snapshot holds that snapshot's volume, whose destroy is refused while
//! the green lives (A-217). Names are unique among the volumes, as they are among a caller's.
//!
//! **What it does not.** It serves no file system. A stream [`SimVolumes::serve_shards`] hands out is one end of a
//! real connected pair (a socket pair on Unix, a duplex pipe on Windows) whose other end the simulator holds and never
//! answers, until a detach or the volume's destroy closes it, when its peer reads the end. A mount is a record of one:
//! [`SimVolumes::mount`] answers the path as given and touches nothing at it. A container bind is a record of one too
//! (A-520): [`SimVolumes::bind_into_container`] judges the mounts it recorded as the daemon judges the kernel's table and
//! its own records (the newest mount at the path is the one seen, a mount beneath it refuses the bind, and the mount
//! must be the volume's and writable for a writable bind), answers Slates' own runtime entry
//! (`slates_bridge_oci::binding`), claims none of the table's evidence (no table vouched for it), and ends the bind
//! with its mount. Where the daemon refuses another volume's mount by the table's source (macOS's NFS export names the
//! volume) before its records, the simulator answers the records' refusal, as Linux's daemon does. A green's version is
//! its first (no merge is simulated), and every volume's bytes are none. A host test that needs a guest's bytes runs a
//! real instance.
//!
//! **Bounds.** Each table (volumes, attachments) holds at most what its constructor was given, the caller's own bound,
//! and refuses past it [`Exhausted::Table`]; ids come from one counter, refused the same way at its last value.

use std::collections::BTreeMap;

use slates_client::{Attachment, ShardsStream, StreamEnd};
use slates_ipc::protocol::{
  AttachTransport, AttachmentCapability, Conformance, DeleteWhileOpen, Established, HostPathReason,
  Intent, KernelCache, OciBinding, ReadWritePolicy, Residency, SharingSemantics, SnapshotId,
  TargetPathConstraint, VolumeId, VolumeRefusal, VolumeSummary,
};

use crate::container::ContainerBind;
use crate::error::{
  Busy, Denied, Error, Exhausted, Existing, Invalid, KindMismatch, Missing, Note,
};
use crate::mount::{MountOptions, Mounted, Unmounted};
use crate::outcome::Work;
use crate::seam::Volumes;
use crate::volume::{Provisioned, Source, VolumeSpec};

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
/// Format: the verb the daemon's record check names when a container bind's source is no mount of the volume's that
/// admits the bind (`crates/server/src/oci.rs`, `Binding::consumer`).
const SOURCE_MOUNT_VERB: &str = "OCI source mount";

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
  /// A mount's record: its path, and how it was made.
  Mount {
    /// The path the mount was asked at.
    path: String,
    /// Made read-only: a bind of it for writing is refused.
    read_only: bool,
    /// Made reachable by other local users (a rootful container runtime's): what a bind's evidence says of it.
    shared: bool,
  },
  /// A container bind's record (A-520): the mount it borrows, with which it ends.
  Bind {
    /// The source mount's attachment.
    mount: u64,
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

  /// The mount a container bind of `volume` at `source` borrows, judged as the daemon judges the kernel's table and its
  /// own records (`crates/server/src/oci.rs`), in its order: `source`, then `destination`, absolute; a mount at `source`
  /// (the newest there, as the table's last entry at a path is the one seen); none recorded beneath it (the bind is
  /// non-recursive); and that mount `volume`'s, and writable for a writable bind. Its attachment, its path as recorded,
  /// and whether it was made shared.
  fn bindable(
    &self,
    volume: VolumeId,
    (source, destination): (&str, &str),
    read_only: bool,
  ) -> Result<(u64, String, bool), Error> {
    let chosen = |reason| Error::Invalid(Invalid::ChosenPath(reason));
    if !source.starts_with('/') {
      return Err(chosen(HostPathReason::NotAbsolute));
    }
    if !destination.starts_with('/') {
      return Err(chosen(HostPathReason::DestinationNotAbsolute));
    }
    let at = trimmed(source);
    let mounts = || {
      self
        .attachments
        .iter()
        .filter_map(|(id, attachment)| match &attachment.held {
          Held::Mount {
            path,
            read_only: made_read_only,
            shared,
          } => Some((*id, attachment.volume, path, *made_read_only, *shared)),
          Held::Stream { .. } | Held::Bind { .. } => None,
        })
    };
    let (mount, mounted, path, mount_read_only, shared) = mounts()
      .rev()
      .find(|(_, _, path, _, _)| trimmed(path) == at)
      .ok_or_else(|| chosen(HostPathReason::NotAMountPoint))?;
    if let Some((_, _, beneath, _, _)) = mounts().find(|(_, _, other, _, _)| {
      let other = trimmed(other);
      other != at
        && other
          .strip_prefix(at)
          .is_some_and(|rest| rest.starts_with('/') || at == "/")
    }) {
      return Err(chosen(HostPathReason::DescendantMount {
        mount_point: beneath.clone(),
      }));
    }
    if mounted != volume || (mount_read_only && !read_only) {
      return Err(Error::Denied(Denied::Rights {
        verb: Note::new(SOURCE_MOUNT_VERB),
      }));
    }
    Ok((mount, path.clone(), shared))
  }

  /// Ends the attachments `ids`, and every container bind that borrows one of them: a bind ends with its source mount,
  /// as the daemon ends it.
  fn end(&mut self, ids: &[u64]) {
    self.attachments.retain(|id, attachment| {
      !ids.contains(id) && !matches!(attachment.held, Held::Bind { mount } if ids.contains(&mount))
    });
  }
}

/// `path` without its trailing separators, the root kept: how the daemon compares a path with the kernel's table.
fn trimmed(path: &str) -> &str {
  let trimmed = path.trim_end_matches('/');
  if trimmed.is_empty() { "/" } else { trimmed }
}

/// Whether this host offers a host mount to bind into a container (`crates/server/src/oci.rs`): every Unix host the
/// simulator stands in for, whose mounts it records.
#[cfg(unix)]
fn offered() -> Result<(), Error> {
  Ok(())
}

/// Windows offers no host mount to bind into a container (`crates/server/src/oci.rs`: a container runtime there cannot
/// bind a WinFsp drive): the daemon's refusal.
#[cfg(windows)]
fn not_offered() -> Error {
  Error::Unsupported(crate::error::Unsupported::Attachment {
    transport: AttachTransport::Oci,
    reason: slates_ipc::protocol::UnsupportedReason::HostPlatform,
  })
}

/// Whether this host offers a host mount to bind into a container: Windows does not ([`not_offered`]), refused as the
/// daemon refuses it, before the paths are judged.
#[cfg(windows)]
fn offered() -> Result<(), Error> {
  Err(not_offered())
}

/// The binding of the mount at `source` (as recorded) at `destination`: Slates' own runtime entry
/// (`slates_bridge_oci::binding`, the words the daemon answers), with none of the kernel table's evidence claimed (no
/// table vouched for a simulated mount: an empty type and source, no identity) beyond what the record holds, whether the
/// mount was made shared.
#[cfg(unix)]
fn binding_of(
  (source, destination): (&str, &str),
  read_only: bool,
  shared: bool,
) -> Result<OciBinding, Error> {
  let entry = slates_bridge_oci::binding::OciMountEntry {
    destination: destination.to_owned(),
    source: source.to_owned(),
    read_only,
  };
  Ok(OciBinding {
    mount_type: entry.mount_type().to_owned(),
    options: entry.options(),
    source: entry.source,
    destination: entry.destination,
    read_only,
    evidence: slates_ipc::protocol::HostMountEvidence {
      fstype: String::new(),
      mount_source: String::new(),
      names_volume: true,
      mount_id: 0,
      mount_device: 0,
      shared_with_other_users: shared,
    },
  })
}

/// No binding on Windows: [`offered`] refuses first, and this answers the same refusal.
#[cfg(windows)]
fn binding_of(_paths: (&str, &str), _read_only: bool, _shared: bool) -> Result<OciBinding, Error> {
  Err(not_offered())
}

/// The refusal of a full table named `name`.
fn table(name: &str) -> Error {
  Error::Exhausted(Exhausted::Table {
    table: Note::new(name),
  })
}

/// The answer for a pinned snapshot's destroy, or a volume a green's origin holds: the daemon's, the volume core's
/// `Pinned`, typed since A-299 as a holder the caller ends first.
fn pinned() -> Error {
  Error::Busy(Busy::Volume(VolumeRefusal::Pinned))
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
  slates_ipc::stream::pair(UNBUFFERED).map_err(|e| Error::of_ipc(SIMULATOR, e, NO_PAUSE_NS))
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
    options: &MountOptions,
  ) -> Result<Mounted, Error> {
    if !self.volumes.contains_key(&volume) {
      return Err(Error::NotFound(Missing::Volume(volume)));
    }
    let attachment = self.attach(
      volume,
      Held::Mount {
        path: path.to_owned(),
        read_only: options.read_only,
        shared: options.shared,
      },
    )?;
    Ok(Mounted {
      attachment,
      path: path.to_owned(),
    })
  }

  fn unmount(&mut self, path: &str) -> Result<Unmounted, Error> {
    let gone: Vec<u64> = self
      .attachments
      .iter()
      .filter(
        |(_, attachment)| matches!(&attachment.held, Held::Mount { path: at, .. } if at == path),
      )
      .map(|(id, _)| *id)
      .collect();
    self.end(&gone);
    // The simulator asks no volume's status for its mounts, so none goes unasked.
    Ok(Unmounted::default())
  }

  fn bind_into_container(
    &mut self,
    volume: VolumeId,
    source: &str,
    destination: &str,
    intent: Intent,
  ) -> Result<ContainerBind, Error> {
    if !self.volumes.contains_key(&volume) {
      return Err(Error::NotFound(Missing::Volume(volume)));
    }
    offered()?;
    let read_only = matches!(intent, Intent::Read);
    let (mount, recorded, shared) = self.bindable(volume, (source, destination), read_only)?;
    let binding = binding_of((recorded.as_str(), destination), read_only, shared)?;
    let attachment = self.attach(volume, Held::Bind { mount })?;
    Ok(ContainerBind {
      attachment,
      binding,
    })
  }

  fn detach(&mut self, attachment: u64) -> Result<(), Error> {
    if !self.attachments.contains_key(&attachment) {
      return Err(Error::NotFound(Missing::Attachment { attachment }));
    }
    self.end(&[attachment]);
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Shape: room for the few volumes and attachments these cases hold.
  const MOST: usize = 8;
  /// Shape: a volume's bound; any will do, since nothing is written.
  const ROOM: u64 = 1 << 20;
  /// Shape: where a container sees a bound mount.
  const INSIDE: &str = "/work";
  /// Shape: where a mount is recorded.
  #[cfg(unix)]
  const POINT: &str = "/sim/work";

  /// A new empty volume `name`.
  fn volume(sim: &mut SimVolumes, name: &str) -> VolumeId {
    sim
      .provision(&VolumeSpec::empty(name, ROOM))
      .unwrap()
      .volume()
  }

  /// The runtime entry's options the daemon answers for a bind with `access` (`rw` or `ro`), word for word.
  #[cfg(unix)]
  fn words(access: &str) -> Vec<String> {
    ["bind", access, "private", "nosuid", "nodev"]
      .map(str::to_owned)
      .to_vec()
  }

  /// §4.6 A-9, A-520. Do: mount a volume in the simulator shared, bind its mount into a container for writing, and bind
  /// it again for reading by its path with a trailing separator; detach the first binding, then unmount the mount.
  /// Expect each binding Slates' runtime entry at the mount's recorded path and the destination given (`bind`, `rw` or
  /// `ro`, `private`, `nosuid`, `nodev`, as the daemon answers them), the mount's sharing in its evidence and none of a
  /// kernel table's facts claimed; each binding an attachment of its own that its detach ends, and the other ended with
  /// its mount.
  #[cfg(unix)]
  #[test]
  fn a_mounts_bind_answers_the_runtimes_entry_and_ends_with_its_mount() {
    let mut sim = SimVolumes::new(MOST);
    let work = volume(&mut sim, "work");
    let shared = MountOptions {
      shared: true,
      ..MountOptions::default()
    };
    let mounted = sim.mount(work, POINT, &shared).unwrap();
    let writable = sim
      .bind_into_container(work, POINT, INSIDE, Intent::Write)
      .unwrap();
    assert_eq!(
      writable.binding,
      OciBinding {
        source: POINT.to_owned(),
        destination: INSIDE.to_owned(),
        read_only: false,
        mount_type: "bind".to_owned(),
        options: words("rw"),
        evidence: slates_ipc::protocol::HostMountEvidence {
          fstype: String::new(),
          mount_source: String::new(),
          names_volume: true,
          mount_id: 0,
          mount_device: 0,
          shared_with_other_users: true,
        },
      }
    );
    let readable = sim
      .bind_into_container(work, &format!("{POINT}/"), INSIDE, Intent::Read)
      .unwrap();
    assert_eq!(
      (
        readable.binding.source.as_str(),
        readable.binding.read_only,
        &readable.binding.options
      ),
      (POINT, true, &words("ro"))
    );
    assert!(
      writable.attachment != readable.attachment
        && writable.attachment != mounted.attachment
        && readable.attachment != mounted.attachment,
      "each binding is an attachment of its own"
    );
    sim.detach(writable.attachment).unwrap();
    assert_eq!(
      sim.detach(writable.attachment),
      Err(Error::NotFound(Missing::Attachment {
        attachment: writable.attachment
      }))
    );
    sim.unmount(POINT).unwrap();
    assert_eq!(
      sim.detach(readable.attachment),
      Err(Error::NotFound(Missing::Attachment {
        attachment: readable.attachment
      })),
      "a binding ends with its source mount"
    );
  }

  /// §4.6 A-9, A-520. Do: bind a volume no one made, a relative source, a relative destination, a folder no mount holds,
  /// a mount with another mounted beneath it, another volume's mount, and a read-only mount for writing; then, in a
  /// simulator with room for exactly the attachments made, bind the read-only mount for reading. Expect each refused
  /// typed as the daemon refuses it, in its order (`NotFound`; `ChosenPath`'s `NotAbsolute`, `DestinationNotAbsolute`,
  /// `NotAMountPoint`, and `DescendantMount` naming the mount beneath; `Denied::Rights` naming the source mount, for the
  /// last two), and the read bound: no refusal held an attachment's room.
  #[cfg(unix)]
  #[test]
  fn a_bind_the_daemon_would_refuse_is_refused_typed_and_holds_nothing() {
    /// Shape: the attachments these cases keep, three mounts and the one binding at the end, and room for the two
    /// volumes.
    const KEPT: usize = 4;
    let mut sim = SimVolumes::new(KEPT);
    let outer = volume(&mut sim, "outer");
    let inner = volume(&mut sim, "inner");
    sim
      .mount(outer, "/sim/outer", &MountOptions::default())
      .unwrap();
    sim
      .mount(inner, "/sim/outer/inner", &MountOptions::default())
      .unwrap();
    let read_only = MountOptions {
      read_only: true,
      ..MountOptions::default()
    };
    sim.mount(outer, "/sim/read-only", &read_only).unwrap();
    let chosen =
      |reason| -> Result<ContainerBind, Error> { Err(Error::Invalid(Invalid::ChosenPath(reason))) };
    let rights: Result<ContainerBind, Error> = Err(Error::Denied(Denied::Rights {
      verb: Note::new(SOURCE_MOUNT_VERB),
    }));
    let nowhere = VolumeId { bytes: [0xAB; 16] };
    assert_eq!(
      sim.bind_into_container(nowhere, "/sim/outer", INSIDE, Intent::Read),
      Err(Error::NotFound(Missing::Volume(nowhere)))
    );
    assert_eq!(
      sim.bind_into_container(outer, "sim/outer", INSIDE, Intent::Read),
      chosen(HostPathReason::NotAbsolute)
    );
    assert_eq!(
      sim.bind_into_container(outer, "/sim/outer", "work", Intent::Read),
      chosen(HostPathReason::DestinationNotAbsolute)
    );
    assert_eq!(
      sim.bind_into_container(outer, "/sim", INSIDE, Intent::Read),
      chosen(HostPathReason::NotAMountPoint)
    );
    assert_eq!(
      sim.bind_into_container(outer, "/sim/outer", INSIDE, Intent::Read),
      chosen(HostPathReason::DescendantMount {
        mount_point: "/sim/outer/inner".to_owned()
      })
    );
    assert_eq!(
      sim.bind_into_container(outer, "/sim/outer/inner", INSIDE, Intent::Read),
      rights
    );
    assert_eq!(
      sim.bind_into_container(outer, "/sim/read-only", INSIDE, Intent::Write),
      rights
    );
    let bound = sim
      .bind_into_container(outer, "/sim/read-only", INSIDE, Intent::Read)
      .unwrap();
    assert!(bound.binding.read_only, "a read binding is read-only");
  }

  /// §4.6 A-9, A-520: Windows offers no host mount to bind. Do: mount a volume in the simulator at a drive and bind it,
  /// and bind a volume no one made. Expect the bind refused `Unsupported::Attachment { Oci, HostPlatform }`, as the
  /// daemon refuses it, and the stranger `NotFound` first.
  #[cfg(windows)]
  #[test]
  fn a_bind_on_windows_is_refused_as_the_daemon_refuses_it() {
    let mut sim = SimVolumes::new(MOST);
    let work = volume(&mut sim, "work");
    sim.mount(work, "S:", &MountOptions::default()).unwrap();
    assert_eq!(
      sim.bind_into_container(work, "S:", INSIDE, Intent::Write),
      Err(not_offered())
    );
    let nowhere = VolumeId { bytes: [0xAB; 16] };
    assert_eq!(
      sim.bind_into_container(nowhere, "S:", INSIDE, Intent::Write),
      Err(Error::NotFound(Missing::Volume(nowhere)))
    );
  }
}
