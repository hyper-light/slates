//! Recoverable overlay witnesses and source directories (§4.5, §4.8, D-25). Source paths stay
//! independent of overlay renames. Reacquisition opens each component without following links and
//! requires the saved directory fingerprint; a replaced or unavailable source is a typed refusal.
//! Cached listings, digests and watcher tokens are rebuilt, never mistaken for retained authority.
//! The witness tables are kept with every version a snapshot still reads (A-48), so a recovered volume
//! lands and clones its snapshots against the witnesses they froze.

use super::{BaseConfig, BasePlane, DriftKind, Listing, Version};
use crate::error::VfsError;
use crate::host::{HostDir, HostFacts, HostFs, WatchState};
use crate::ids::{Epoch, InodeNo};
use crate::inode::{Fingerprint, Witness};
use slates_wire::Wire;

#[derive(Clone, Debug, PartialEq, Eq, Wire)]
struct Directory {
  inode: u64,
  source: Vec<String>,
  fingerprint: Fingerprint,
}

/// One version of an inode's witness: from epoch `from`, the witness (or its removal).
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
struct Witnessed {
  inode: u64,
  from: u64,
  witness: Option<Witness>,
}

/// Where on the disk a witness was taken: the base directory's inode and the entry name there.
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
struct Home {
  parent: u64,
  name: String,
}

/// One version of a witness's home.
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
struct Homed {
  inode: u64,
  from: u64,
  home: Option<Home>,
}

/// One version of a whiteout's hidden base fingerprint.
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
struct Whiteout {
  parent: u64,
  name: String,
  from: u64,
  fingerprint: Option<Fingerprint>,
}

/// One version of a redirect's moved base fingerprint.
#[derive(Clone, Debug, PartialEq, Eq, Wire)]
struct Redirect {
  inode: u64,
  from: u64,
  fingerprint: Option<Fingerprint>,
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
  homes: Vec<Homed>,
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
    // The head's witnesses each keep their home: drift checks and descriptors need it.
    if self
      .witnesses
      .iter_head()
      .any(|(inode, _)| !self.witness_homes.head_has(inode))
    {
      return Err(VfsError::RecoveryIncomplete);
    }
    Ok(BaseImage {
      granularity_ns: self.facts.timestamp_granularity_ns,
      large_class_bytes: self.large_class_bytes,
      directories,
      witnesses: self
        .witnesses
        .versions()
        .map(|(inode, version)| Witnessed {
          inode: inode.0,
          from: version.from.0,
          witness: version.value,
        })
        .collect(),
      homes: self
        .witness_homes
        .versions()
        .map(|(inode, version)| Homed {
          inode: inode.0,
          from: version.from.0,
          home: version.value.as_ref().map(|(parent, name)| Home {
            parent: parent.0,
            name: name.to_string(),
          }),
        })
        .collect(),
      whiteouts: self
        .whiteouts
        .versions()
        .map(|((parent, name), version)| Whiteout {
          parent: parent.0,
          name: name.to_string(),
          from: version.from.0,
          fingerprint: version.value,
        })
        .collect(),
      redirects: self
        .redirects
        .versions()
        .map(|(inode, version)| Redirect {
          inode: inode.0,
          from: version.from.0,
          fingerprint: version.value,
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
      restore_versions(&mut plane, image)?;
      // Every head witness keeps a home in a reacquired directory, as it did when it was imaged.
      let homeless = plane.witnesses.iter_head().any(|(inode, _)| {
        !plane
          .witness_homes
          .head(inode)
          .is_some_and(|(parent, _)| plane.listings.contains_key(parent))
      });
      if homeless {
        return Err(VfsError::RecoveryIncomplete);
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

/// Rebuilds the versioned witness tables from the image, refusing any table whose versions do not run
/// forward in epoch order: that is not a history the plane recorded.
fn restore_versions(plane: &mut BasePlane, image: &BaseImage) -> Result<(), VfsError> {
  let refused = |_| VfsError::RecoveryIncomplete;
  for entry in &image.witnesses {
    plane
      .witnesses
      .restore(InodeNo(entry.inode), version(entry.from, entry.witness))
      .map_err(refused)?;
  }
  for entry in &image.homes {
    let home = entry
      .home
      .as_ref()
      .map(|home| (InodeNo(home.parent), home.name.as_str().into()));
    plane
      .witness_homes
      .restore(InodeNo(entry.inode), version(entry.from, home))
      .map_err(refused)?;
  }
  for entry in &image.whiteouts {
    plane
      .whiteouts
      .restore(
        (InodeNo(entry.parent), entry.name.as_str().into()),
        version(entry.from, entry.fingerprint),
      )
      .map_err(refused)?;
  }
  for entry in &image.redirects {
    plane
      .redirects
      .restore(InodeNo(entry.inode), version(entry.from, entry.fingerprint))
      .map_err(refused)?;
  }
  Ok(())
}

/// A recorded version from epoch `from`.
fn version<V>(from: u64, value: Option<V>) -> Version<V> {
  Version {
    from: Epoch(from),
    value,
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
