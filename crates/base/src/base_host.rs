//! A volume's base host (§4.15 "Live source"): the directory a host path names, read through the operating system
//! ([`OsHost`]), or a tree the shard holds (a mounted work's green version, A-774). The shard's volume slot holds one, so
//! every reader of a base goes through the one read-only seam ([`HostFs`]) whichever it is, and only what belongs to a
//! directory alone asks which: a landing writes a host directory through the handles of the host its overlay's base names
//! (`slates-land`'s `OsLand`), and a clone duplicates those handles (`OsHost::try_clone_dirs`).

use slates_vfs::host::{
  BaseEntry, Hint, HostDir, HostError, HostFacts, HostFile, HostFs, WatchState,
};
use slates_vfs::inode::Fingerprint;

use crate::OsHost;

/// A volume's base host: a host directory, or a tree the shard holds.
#[derive(Debug)]
pub enum BaseHost {
  /// A host directory, read through the operating system.
  Disk(OsHost),
  /// A tree the shard holds (a mounted work's green version, A-774), read only through a lend of the tree with the host
  /// (`TreeHost::lend`); lent alone, it refuses every read.
  Tree(Box<slates_vfs::host::tree::TreeHost>),
}

impl BaseHost {
  /// The directory host, when the base is a host directory.
  pub fn as_disk(&self) -> Option<&OsHost> {
    match self {
      BaseHost::Disk(host) => Some(host),
      BaseHost::Tree(_) => None,
    }
  }

  /// The descriptor of the host directory's watcher, for the shard's hint set (A-709, `crate::HintSet`); `None` for a
  /// tree, which no watcher reports, and for a host the kernel gave no watcher.
  #[cfg(unix)]
  pub fn hint_source(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
    self.as_disk().and_then(OsHost::hint_source)
  }

  /// The directory host itself, when the base is a host directory; the base back otherwise.
  pub fn into_disk(self) -> Result<OsHost, BaseHost> {
    match self {
      BaseHost::Disk(host) => Ok(host),
      other @ BaseHost::Tree(_) => Err(other),
    }
  }

  /// The host as the seam's trait object, whichever it is.
  fn seam(&mut self) -> &mut dyn HostFs {
    match self {
      BaseHost::Disk(host) => host,
      BaseHost::Tree(host) => host.as_mut(),
    }
  }
}

impl From<OsHost> for BaseHost {
  fn from(host: OsHost) -> BaseHost {
    BaseHost::Disk(host)
  }
}

impl HostFs for BaseHost {
  fn facts(&mut self, dir: HostDir) -> Result<HostFacts, HostError> {
    self.seam().facts(dir)
  }

  fn fingerprint_dir(&mut self, dir: HostDir) -> Result<Fingerprint, HostError> {
    self.seam().fingerprint_dir(dir)
  }

  fn list(&mut self, dir: HostDir) -> Result<Vec<BaseEntry>, HostError> {
    self.seam().list(dir)
  }

  fn open_dir(&mut self, parent: HostDir, name: &str) -> Result<HostDir, HostError> {
    self.seam().open_dir(parent, name)
  }

  fn open_file(&mut self, dir: HostDir, name: &str) -> Result<HostFile, HostError> {
    self.seam().open_file(dir, name)
  }

  fn fstat(&mut self, file: HostFile) -> Result<Fingerprint, HostError> {
    self.seam().fstat(file)
  }

  fn read_at(&mut self, file: HostFile, off: u64, buf: &mut [u8]) -> Result<usize, HostError> {
    self.seam().read_at(file, off, buf)
  }

  fn read_link(&mut self, dir: HostDir, name: &str) -> Result<Box<str>, HostError> {
    self.seam().read_link(dir, name)
  }

  fn close_file(&mut self, file: HostFile) {
    self.seam().close_file(file);
  }

  fn close_dir(&mut self, dir: HostDir) {
    self.seam().close_dir(dir);
  }

  fn watch(&mut self, dir: HostDir) -> WatchState {
    self.seam().watch(dir)
  }

  fn hints(&mut self) -> Vec<Hint> {
    self.seam().hints()
  }

  fn now_ns(&mut self) -> i64 {
    self.seam().now_ns()
  }
}
