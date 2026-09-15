//! Recoverable overlay witnesses and source directories (§4.5, §4.8, D-25). Source paths stay
//! independent of overlay renames. Reacquisition opens each component without following links and
//! requires the saved directory fingerprint; a replaced or unavailable source is a typed refusal.
//! Cached listings, digests and watcher tokens are rebuilt, never mistaken for retained authority.

use super::{BaseConfig, BasePlane, DriftKind, Listing};
use crate::error::VfsError;
use crate::host::{HostDir, HostFacts, HostFs, WatchState};
use crate::ids::InodeNo;
use crate::inode::{Fingerprint, Witness};
use slates_wire::Wire;

#[derive(Clone, Debug, PartialEq, Eq, Wire)]
struct Directory {
  inode: u64,
  source: Vec<String>,
  fingerprint: Fingerprint,
}

#[derive(Clone, Debug, PartialEq, Eq, Wire)]
struct Witnessed {
  inode: u64,
  witness: Witness,
  parent: u64,
  name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Wire)]
struct Whiteout {
  parent: u64,
  name: String,
  fingerprint: Fingerprint,
}

#[derive(Clone, Debug, PartialEq, Eq, Wire)]
struct Redirect {
  inode: u64,
  fingerprint: Fingerprint,
}

#[derive(Clone, Debug, PartialEq, Eq, Wire)]
struct Drift {
  inode: u64,
  kind: DriftKind,
}

/// The durable base plane. Pinned file extents live in the corresponding inode images.
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
pub struct BaseImage {
  granularity_ns: u64,
  large_class_bytes: u64,
  directories: Vec<Directory>,
  witnesses: Vec<Witnessed>,
  whiteouts: Vec<Whiteout>,
  redirects: Vec<Redirect>,
  drift: Vec<Drift>,
}

impl BasePlane {
  /// Releases handles opened during a refused reconstruction; the root is still the caller's.
  pub(crate) fn release_sources(self, host: &mut dyn HostFs) {
    for listing in self.listings.values() {
      if listing.dir != self.root {
        host.close_dir(listing.dir);
      }
    }
  }

  pub(crate) fn image(&self, host: &mut dyn HostFs) -> Result<BaseImage, VfsError> {
    let directories = self
      .listings
      .iter()
      .map(|(inode, listing)| {
        Ok(Directory {
          inode: inode.0,
          source: listing.source.clone(),
          fingerprint: host
            .fingerprint_dir(listing.dir)
            .map_err(super::host_refusal)?,
        })
      })
      .collect::<Result<_, VfsError>>()?;
    let witnesses = self
      .witnesses
      .iter()
      .map(|(inode, witness)| {
        let (parent, name) = self
          .witness_homes
          .get(inode)
          .ok_or(VfsError::RecoveryIncomplete)?;
        Ok(Witnessed {
          inode: inode.0,
          witness: *witness,
          parent: parent.0,
          name: name.to_string(),
        })
      })
      .collect::<Result<_, VfsError>>()?;
    Ok(BaseImage {
      granularity_ns: self.facts.timestamp_granularity_ns,
      large_class_bytes: self.large_class_bytes,
      directories,
      witnesses,
      whiteouts: self
        .whiteouts
        .iter()
        .map(|((parent, name), fingerprint)| Whiteout {
          parent: parent.0,
          name: name.to_string(),
          fingerprint: *fingerprint,
        })
        .collect(),
      redirects: self
        .redirects
        .iter()
        .map(|(inode, fingerprint)| Redirect {
          inode: inode.0,
          fingerprint: *fingerprint,
        })
        .collect(),
      drift: self
        .drift
        .iter()
        .map(|(inode, kind)| Drift {
          inode: inode.0,
          kind: *kind,
        })
        .collect(),
    })
  }

  pub(crate) fn recover(
    image: &BaseImage,
    host: &mut dyn HostFs,
    root: HostDir,
    root_no: InodeNo,
  ) -> Result<Self, VfsError> {
    let mut plane = Self::new(
      BaseConfig {
        root,
        facts: HostFacts {
          timestamp_granularity_ns: image.granularity_ns,
        },
        large_class_bytes: image.large_class_bytes,
      },
      root_no,
    );
    plane.listings.clear();
    let result = (|| {
      for directory in &image.directories {
        if (directory.inode == root_no.0) != directory.source.is_empty() {
          return Err(VfsError::RecoveryIncomplete);
        }
        let opened = open_source(host, root, &directory.source)?;
        let fingerprint = host.fingerprint_dir(opened).map_err(super::host_refusal);
        if fingerprint.as_ref() != Ok(&directory.fingerprint) {
          if opened != root {
            host.close_dir(opened);
          }
          fingerprint?;
          return Err(VfsError::RecoveryIncomplete);
        }
        if plane.listings.contains_key(&InodeNo(directory.inode)) {
          if opened != root {
            host.close_dir(opened);
          }
          return Err(VfsError::RecoveryIncomplete);
        }
        plane.listings.insert(
          InodeNo(directory.inode),
          Listing {
            dir: opened,
            source: directory.source.clone(),
            fingerprint: None,
            entries: None,
            read_at_ns: 0,
            watch: WatchState::Unavailable,
          },
        );
      }
      if !plane.listings.contains_key(&root_no) {
        return Err(VfsError::RecoveryIncomplete);
      }
      for entry in &image.witnesses {
        if !plane.listings.contains_key(&InodeNo(entry.parent))
          || plane
            .witnesses
            .insert(InodeNo(entry.inode), entry.witness)
            .is_some()
        {
          return Err(VfsError::RecoveryIncomplete);
        }
        plane.witness_homes.insert(
          InodeNo(entry.inode),
          (InodeNo(entry.parent), entry.name.as_str().into()),
        );
      }
      for entry in &image.whiteouts {
        plane.whiteouts.insert(
          (InodeNo(entry.parent), entry.name.as_str().into()),
          entry.fingerprint,
        );
      }
      for entry in &image.redirects {
        plane
          .redirects
          .insert(InodeNo(entry.inode), entry.fingerprint);
      }
      for entry in &image.drift {
        plane.drift.insert(InodeNo(entry.inode), entry.kind);
      }
      // Host monotonic clocks and watcher queues do not survive a process lifetime.
      plane.recheck_all = true;
      Ok(())
    })();
    if let Err(error) = result {
      for listing in plane.listings.values() {
        if listing.dir != root {
          host.close_dir(listing.dir);
        }
      }
      return Err(error);
    }
    Ok(plane)
  }
}

fn open_source(host: &mut dyn HostFs, root: HostDir, path: &[String]) -> Result<HostDir, VfsError> {
  let mut current = root;
  for component in path {
    if component.is_empty()
      || component == "."
      || component == ".."
      || component.contains(['/', '\\', '\0'])
    {
      if current != root {
        host.close_dir(current);
      }
      return Err(VfsError::RecoveryIncomplete);
    }
    let next = host.open_dir(current, component);
    if current != root {
      host.close_dir(current);
    }
    current = next.map_err(super::host_refusal)?;
  }
  Ok(current)
}
