//! A-198's held mount: its checked handoff record, and the Windows drive link's lifetime. The record names the exact
//! WinFsp volume, drive and read-only posture. The drive is defined in the caller's DOS namespace and its temporary
//! symbolic-link handle is owned explicitly, following WinFsp v2.1 `src/dll/mount.c` `FspMountSet_Drive`; no launcher,
//! privileged broker or directory is needed. The anchor duplicates this handle together with the control and ledger
//! handles. Normal teardown removes that exact definition after the anchor acknowledged releasing its copies.

/// Format: `FSP_FSCTL_VOLUME_NAME_SIZEMAX / sizeof(WCHAR)`, from WinFsp v2.1 fsctl.h.
pub const VOLUME_UNITS: usize = 64 + 192;
/// Format: a drive's letter and colon, the whole volume-name field, then one read-only flag byte.
pub const RECORD_BYTES: usize = (VOLUME_UNITS + 2) * size_of::<u16>() + size_of::<u8>();

/// Why a held mount's record cannot be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// The record is not the fixed format's length.
  Length,
  /// The volume is not a terminated, padded UTF-16 kernel name.
  Volume,
  /// The drive is not an ASCII letter and colon.
  Drive,
  /// The read-only flag is not a boolean.
  Flags,
}

/// The mount's record, opaque to the supervising anchor and validated by its daemon before adoption.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
  /// The NUL-terminated kernel volume name, with zero padding.
  pub volume: [u16; VOLUME_UNITS],
  /// The DOS drive letter and colon.
  pub drive: [u16; 2],
  /// The posture used to construct WinFsp's matching dispatcher object.
  pub read_only: bool,
}

impl Record {
  /// Encodes a validated record; invalid kernel names or drive letters are refused before a handle is offered.
  pub fn encode(&self) -> Result<Vec<u8>, Refusal> {
    self.check()?;
    let mut encoded = Vec::with_capacity(RECORD_BYTES);
    for unit in self.volume.iter().chain(self.drive.iter()) {
      encoded.extend_from_slice(&unit.to_le_bytes());
    }
    encoded.push(u8::from(self.read_only));
    Ok(encoded)
  }

  /// Checks length before decoding or allocating; the outer hold protocol already checked its CRC32C.
  pub fn decode(encoded: &[u8]) -> Result<Self, Refusal> {
    if encoded.len() != RECORD_BYTES {
      return Err(Refusal::Length);
    }
    let mut record = Self {
      volume: [0; VOLUME_UNITS],
      drive: [0; 2],
      read_only: false,
    };
    let units = record.volume.iter_mut().chain(record.drive.iter_mut());
    for (unit, bytes) in units.zip(encoded.as_chunks::<{ size_of::<u16>() }>().0) {
      *unit = u16::from_le_bytes(*bytes);
    }
    record.read_only = match encoded.last() {
      Some(0) => false,
      Some(1) => true,
      _ => return Err(Refusal::Flags),
    };
    record.check()?;
    Ok(record)
  }

  /// The canonical drive text used by the catalog and by the existing drive's successor.
  pub fn drive_text(&self) -> Result<String, Refusal> {
    self.check()?;
    String::from_utf16(&self.drive).map_err(|_| Refusal::Drive)
  }

  fn check(&self) -> Result<(), Refusal> {
    let end = self
      .volume
      .iter()
      .position(|unit| *unit == 0)
      .ok_or(Refusal::Volume)?;
    let head = self.volume.get(..end).ok_or(Refusal::Volume)?;
    if head.first() != Some(&u16::from(b'\\'))
      || String::from_utf16(head).is_err()
      || self
        .volume
        .get(end..)
        .is_none_or(|tail| tail.iter().any(|unit| *unit != 0))
    {
      return Err(Refusal::Volume);
    }
    let [letter, colon] = self.drive;
    if u8::try_from(letter).is_err()
      || !u8::try_from(letter).is_ok_and(|letter| letter.is_ascii_alphabetic())
      || colon != u16::from(b':')
    {
      return Err(Refusal::Drive);
    }
    Ok(())
  }
}

#[cfg(windows)]
pub mod windows {
  //! The three owned Windows handles, and the temporary DOS-device link (A-198). Kernel objects keep their lifetime
  //! through duplicated handles; every create and inherited adoption uses this same drive owner and teardown.

  use super::Record;
  use crate::host::WinFspError;
  use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
  use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
  use windows_sys::Wdk::Storage::FileSystem::NtOpenSymbolicLinkObject;
  use windows_sys::Wdk::System::SystemServices::NtMakeTemporaryObject;
  use windows_sys::Win32::Foundation::{OBJ_CASE_INSENSITIVE, UNICODE_STRING};
  use windows_sys::Win32::Storage::FileSystem::{
    DDD_EXACT_MATCH_ON_REMOVE, DDD_RAW_TARGET_PATH, DDD_REMOVE_DEFINITION, DELETE, DefineDosDeviceW,
  };

  /// The handles inherited from the anchor; ownership is unique before the mapping or dispatcher is built.
  pub struct Inherited {
    /// The held volume's control file object.
    pub volume: OwnedHandle,
    /// The temporary drive-link object.
    pub link: OwnedHandle,
    /// The open ledger's section.
    pub ledger: OwnedHandle,
    /// The checked identity and posture of these objects.
    pub record: Record,
  }

  /// Duplicates offered for the anchor to hold. The mount still owns its originals until teardown.
  pub struct Offer {
    /// A duplicate of the control file object, held while the acknowledgement is owed.
    pub volume: OwnedHandle,
    /// A duplicate of the drive's temporary link.
    pub link: OwnedHandle,
    /// Its checked description.
    pub record: Record,
  }

  /// The live DOS drive definition and its temporary link; its cleanup always names the exact original target.
  pub(crate) struct Drive {
    link: Option<OwnedHandle>,
    name: Vec<u16>,
    target: Vec<u16>,
  }

  impl Drive {
    pub(crate) fn create(record: &Record) -> Result<Self, WinFspError> {
      let name: Vec<_> = record.drive.iter().copied().chain(Some(0)).collect();
      let target = record.volume.to_vec();
      // SAFETY: both names are terminated, and the target is a kernel volume name. This defines only this logon's drive.
      if unsafe { DefineDosDeviceW(DDD_RAW_TARGET_PATH, name.as_ptr(), target.as_ptr()) } == 0 {
        return Err(os("DefineDosDeviceW"));
      }
      let mut drive = Self {
        link: None,
        name,
        target,
      };
      let mut symbol: Vec<_> = "\\??\\".encode_utf16().chain(record.drive).collect();
      let length = u16::try_from(symbol.len().saturating_mul(size_of::<u16>()))
        .map_err(|_| WinFspError::Record(super::Refusal::Drive))?;
      let mut name = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: symbol.as_mut_ptr(),
      };
      let attributes = OBJECT_ATTRIBUTES {
        Length: u32::try_from(size_of::<OBJECT_ATTRIBUTES>())
          .map_err(|_| WinFspError::Record(super::Refusal::Drive))?,
        RootDirectory: std::ptr::null_mut(),
        ObjectName: &mut name,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: std::ptr::null_mut(),
        SecurityQualityOfService: std::ptr::null_mut(),
      };
      let mut link = std::ptr::null_mut();
      // SAFETY: attributes and its counted UTF-16 name remain live; `link` is the writable handle output.
      let status = unsafe { NtOpenSymbolicLinkObject(&mut link, DELETE, &attributes) };
      if status < 0 || link.is_null() {
        return Err(WinFspError::Api {
          call: "NtOpenSymbolicLinkObject",
          status,
        });
      }
      // SAFETY: the call returned this newly owned handle; the Drive closes it on every partial or normal exit.
      let link = unsafe { OwnedHandle::from_raw_handle(link) };
      // SAFETY: `link` is a symbolic-link handle with DELETE, just opened by this process. No privilege is requested.
      let status = unsafe { NtMakeTemporaryObject(link.as_raw_handle()) };
      if status < 0 {
        return Err(WinFspError::Api {
          call: "NtMakeTemporaryObject",
          status,
        });
      }
      drive.link = Some(link);
      Ok(drive)
    }

    pub(crate) fn inherited(record: &Record, link: OwnedHandle) -> Self {
      Self {
        link: Some(link),
        name: record.drive.iter().copied().chain(Some(0)).collect(),
        target: record.volume.to_vec(),
      }
    }

    pub(crate) fn duplicate(&self) -> Result<OwnedHandle, WinFspError> {
      self
        .link
        .as_ref()
        .ok_or(WinFspError::MountThread)?
        .try_clone()
        .map_err(|error| WinFspError::Os {
          call: "duplicate drive link",
          code: error.raw_os_error(),
        })
    }
  }

  impl Drop for Drive {
    fn drop(&mut self) {
      // SAFETY: the strings remain owned and terminated; EXACT_MATCH prevents removal of any other target's definition.
      if unsafe {
        DefineDosDeviceW(
          DDD_RAW_TARGET_PATH | DDD_REMOVE_DEFINITION | DDD_EXACT_MATCH_ON_REMOVE,
          self.name.as_ptr(),
          self.target.as_ptr(),
        )
      } == 0
      {
        eprintln!(
          "slates-winfsp: drive cleanup refused: {}",
          os("DefineDosDeviceW (remove)")
        );
      }
    }
  }

  fn os(call: &'static str) -> WinFspError {
    WinFspError::Os {
      call,
      code: std::io::Error::last_os_error().raw_os_error(),
    }
  }
}

#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
  use super::*;

  fn mount() -> Record {
    let mut volume = [0; VOLUME_UNITS];
    for (out, unit) in volume
      .iter_mut()
      .zip("\\Device\\Volume{1234}".encode_utf16())
    {
      *out = unit;
    }
    Record {
      volume,
      drive: [u16::from(b'S'), u16::from(b':')],
      read_only: true,
    }
  }

  /// A-198. Do: hand a mount record through the hold format. Expect: the same volume identity, drive and posture.
  #[test]
  fn the_mount_record_keeps_its_volume_drive_and_access() {
    let expected = mount();
    let restored = Record::decode(&expected.encode().unwrap()).unwrap();
    assert_eq!(restored, expected);
    assert_eq!(restored.drive_text().unwrap(), "S:");
  }

  /// A-198. Do: truncate or extend a record, supply invalid UTF-16, change its drive or flags. Expect: each refused.
  #[test]
  fn a_malformed_record_cannot_select_a_drive_or_volume() {
    let encoded = mount().encode().unwrap();
    for end in 0..encoded.len() {
      assert_eq!(Record::decode(&encoded[..end]), Err(Refusal::Length));
    }
    let mut extended = encoded.clone();
    extended.push(0);
    assert_eq!(Record::decode(&extended), Err(Refusal::Length));
    let mut flags = encoded;
    *flags.last_mut().unwrap() = 2;
    assert_eq!(Record::decode(&flags), Err(Refusal::Flags));
    let mut invalid = mount();
    invalid.volume[1] = 0xd800;
    assert_eq!(invalid.encode(), Err(Refusal::Volume));
    let mut directory = mount();
    directory.drive = [u16::from(b'S'), u16::from(b'\\')];
    assert_eq!(directory.encode(), Err(Refusal::Drive));
  }
}
