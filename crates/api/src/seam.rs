//! The seam a host reaches slates through when its own rules reach every other product only through a trait (§4.12
//! "The seam", A-310): [`Volumes`], the volume verbs of the consumer contract (A-299) with their own signatures and
//! types, implemented by an [`Instance`] and by an in-memory simulator ([`crate::sim::SimVolumes`]), and held to one
//! conformance suite both pass (`crates/api/tests/seam.rs`).
//!
//! **Why.** Hex, kiln's scheduler and runtime (`vendor/hex`), keeps its volumes on slates (its design's README; kiln's
//! plan T7, CP-T7.8 and CP-T7.13), and its rules allow no direct call into another product: "Other products are reached
//! only through their `-api` crates, and Hex only through its own: each trait with a simulator implementation and a
//! conformance suite both pass" (Hex `CLAUDE.md` §9). Its own simulated cluster (`hex-sim`, kwok's kind) runs on fakes,
//! so it needs a slates it can run without a daemon that answers as the real one does.
//!
//! **What it is not.** A second API. Every method is the [`Instance`] verb of the same name, taking and returning the
//! contract's own types: no outcome or error type is added for it, and a contract change reaches it as it reaches the
//! instance. The trait carries what a scheduler does with volumes: provision from every source, snapshot, destroy,
//! list, serve one to a Shards microVM, mount one on the host, and end an attachment. Landings, merges and write
//! policy stay on the instance, which a host that schedules work does not drive.
//!
//! **What the simulator answers.** The same typed outcome or [`Error`] the instance answers for the same request, for
//! every case the conformance suite exercises; it serves no file system, so a stream it hands out is answered by
//! nothing and a mount it makes is a record of one ([`crate::sim`]).

use crate::error::Error;
use crate::instance::Instance;
use crate::mount::{MountOptions, Mounted};
use crate::volume::{Provisioned, VolumeSpec};
use slates_client::ShardsStream;
use slates_ipc::protocol::{Intent, SnapshotId, VolumeId, VolumeSummary};

/// The volume verbs of the consumer contract a host schedules work over (A-310): each is the [`Instance`] verb of the
/// same name, with its signature.
pub trait Volumes {
  /// [`Instance::provision`]: a volume from `spec`'s source.
  fn provision(&mut self, spec: &VolumeSpec) -> Result<Provisioned, Error>;

  /// [`Instance::snapshot`]: a snapshot of `volume`'s head.
  fn snapshot(&mut self, volume: VolumeId) -> Result<SnapshotId, Error>;

  /// [`Instance::destroy_snapshot`]: `snapshot` of `volume` destroyed; refused while a clone pins it.
  fn destroy_snapshot(&mut self, volume: VolumeId, snapshot: SnapshotId) -> Result<(), Error>;

  /// [`Instance::destroy`]: `volume` destroyed, its attachments ended.
  fn destroy(&mut self, volume: VolumeId) -> Result<(), Error>;

  /// [`Instance::list`]: the caller's volumes.
  fn list(&mut self) -> Result<Vec<VolumeSummary>, Error>;

  /// [`Instance::serve_shards`]: `volume` served to a Shards microVM over a stream whose end is returned.
  fn serve_shards(
    &mut self,
    volume: VolumeId,
    intent: Intent,
    subtree: Option<&str>,
  ) -> Result<ShardsStream, Error>;

  /// [`Instance::mount`]: `volume` mounted on the host at `path`.
  fn mount(
    &mut self,
    volume: VolumeId,
    path: &str,
    options: &MountOptions,
  ) -> Result<Mounted, Error>;

  /// [`Instance::unmount`]: the mount at `path` removed.
  fn unmount(&mut self, path: &str) -> Result<(), Error>;

  /// [`Instance::detach`]: an attachment (a mount's, a stream's) ended.
  fn detach(&mut self, attachment: u64) -> Result<(), Error>;
}

impl Volumes for Instance {
  fn provision(&mut self, spec: &VolumeSpec) -> Result<Provisioned, Error> {
    Instance::provision(self, spec)
  }

  fn snapshot(&mut self, volume: VolumeId) -> Result<SnapshotId, Error> {
    Instance::snapshot(self, volume)
  }

  fn destroy_snapshot(&mut self, volume: VolumeId, snapshot: SnapshotId) -> Result<(), Error> {
    Instance::destroy_snapshot(self, volume, snapshot)
  }

  fn destroy(&mut self, volume: VolumeId) -> Result<(), Error> {
    Instance::destroy(self, volume)
  }

  fn list(&mut self) -> Result<Vec<VolumeSummary>, Error> {
    Instance::list(self)
  }

  fn serve_shards(
    &mut self,
    volume: VolumeId,
    intent: Intent,
    subtree: Option<&str>,
  ) -> Result<ShardsStream, Error> {
    Instance::serve_shards(self, volume, intent, subtree)
  }

  fn mount(
    &mut self,
    volume: VolumeId,
    path: &str,
    options: &MountOptions,
  ) -> Result<Mounted, Error> {
    Instance::mount(self, volume, path, options)
  }

  fn unmount(&mut self, path: &str) -> Result<(), Error> {
    Instance::unmount(self, path)
  }

  fn detach(&mut self, attachment: u64) -> Result<(), Error> {
    Instance::detach(self, attachment)
  }
}
