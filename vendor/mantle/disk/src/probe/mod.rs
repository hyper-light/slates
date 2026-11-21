//! Identifying the device and file system under a path, per operating system.

use std::path::Path;

use crate::identity::Identity;

#[cfg(any(target_os = "linux", target_os = "android"))]
mod linux;
#[cfg(any(target_os = "linux", target_os = "android"))]
use linux::identify as platform_identify;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos::identify as platform_identify;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows::identify as platform_identify;

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    windows
)))]
fn platform_identify(path: &Path) -> Identity {
    use crate::identity::{FileSystem, FileSystemKind};
    let mut identity = Identity::unknown(FileSystem {
        kind: FileSystemKind::Unknown,
        block_size: None,
        total_bytes: None,
        available_bytes: None,
    });
    identity.note(
        format!("identify {}", path.display()),
        "no device probe for this operating system",
    );
    identity
}

/// What the OS says about the storage under `path`. Never fails: whatever cannot be learned
/// is `Unknown` with a note saying why.
pub fn identify(path: &Path) -> Identity {
    platform_identify(path)
}
