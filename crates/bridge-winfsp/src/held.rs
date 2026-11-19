//! A-198's held mount: its checked handoff record, and the lifetime of the volume and its drive. The record names the
//! exact WinFsp volume, drive, read-only posture and name search (A-551). The drive is a Mount Manager drive WinFsp's
//! driver makes for the volume (A-369; WinFsp v2.1 `src/sys/volume.c` `FspVolumeUseMountmgr`), which the machine allows
//! once, as WinFsp itself is installed once; no launcher or privileged process runs. The anchor duplicates the control
//! and ledger handles, and the volume keeps its drive: normal teardown ends both once the anchor acknowledged releasing
//! its copies. Until A-369 the drive was defined in the caller's DOS namespace (`DefineDosDeviceW`) and its temporary
//! symbolic link was held beside them; Windows resolves no file's final path on such a drive.

/// Format: `FSP_FSCTL_VOLUME_NAME_SIZEMAX / sizeof(WCHAR)`, from WinFsp v2.1 fsctl.h.
pub const VOLUME_UNITS: usize = 64 + 192;
/// Format: a drive's letter and colon, the whole volume-name field, then one flag byte.
pub const RECORD_BYTES: usize = (VOLUME_UNITS + 2) * size_of::<u16>() + size_of::<u8>();
/// Format: bit 0 of the record's flag byte: the drive is read-only.
const READ_ONLY: u8 = 1 << 0;
/// Format: bit 1 of the record's flag byte: the drive declared no `CaseSensitiveSearch` (A-551). Clear in a record made
/// before A-551, when every drive declared it, so such a record reads as what its drive was told.
const CASE_INSENSITIVE: u8 = 1 << 1;

/// Why a held mount's record cannot be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// The record is not the fixed format's length.
  Length,
  /// The volume is not a terminated, padded UTF-16 kernel name.
  Volume,
  /// The drive is not an ASCII letter and colon.
  Drive,
  /// The flag byte holds a bit no flag defines.
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
  /// Whether the drive declared no `CaseSensitiveSearch` (A-551). WinFsp's driver fixed it when it made the volume, so
  /// a successor names opens as the drive compares names, whatever the volume's names call for now: a drive mounted
  /// before A-551 declared it whatever its volume's names, and keeps it until it is mounted again.
  pub case_insensitive: bool,
}

impl Record {
  /// Encodes a validated record; invalid kernel names or drive letters are refused before a handle is offered.
  pub fn encode(&self) -> Result<Vec<u8>, Refusal> {
    self.check()?;
    let mut encoded = Vec::with_capacity(RECORD_BYTES);
    for unit in self.volume.iter().chain(self.drive.iter()) {
      encoded.extend_from_slice(&unit.to_le_bytes());
    }
    let read_only = if self.read_only { READ_ONLY } else { 0 };
    let case_insensitive = if self.case_insensitive {
      CASE_INSENSITIVE
    } else {
      0
    };
    encoded.push(read_only | case_insensitive);
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
      case_insensitive: false,
    };
    let units = record.volume.iter_mut().chain(record.drive.iter_mut());
    for (unit, bytes) in units.zip(encoded.as_chunks::<{ size_of::<u16>() }>().0) {
      *unit = u16::from_le_bytes(*bytes);
    }
    let flags = *encoded.last().ok_or(Refusal::Length)?;
    if flags & !(READ_ONLY | CASE_INSENSITIVE) != 0 {
      return Err(Refusal::Flags);
    }
    record.read_only = flags & READ_ONLY != 0;
    record.case_insensitive = flags & CASE_INSENSITIVE != 0;
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
  //! The owned Windows handles of a held mount, and its drive (A-198, A-369). Kernel objects keep their lifetime through
  //! duplicated handles: the control handle keeps the volume, and the volume keeps its Mount Manager drive.

  use super::Record;
  use crate::host::WinFspError;
  use std::os::windows::io::{AsRawHandle, BorrowedHandle, OwnedHandle};
  use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_FILE_EXISTS,
  };
  use windows_sys::Win32::Storage::FileSystem::QueryDosDeviceW;
  use windows_sys::Win32::System::IO::DeviceIoControl;

  /// Format: `FSP_FSCTL_MOUNTMGR` (WinFsp v2.1 `inc/winfsp/fsctl.h`), `CTL_CODE(FILE_DEVICE_FILE_SYSTEM, 0x800 + 'm',
  /// METHOD_BUFFERED, FILE_ANY_ACCESS)`: the request with which WinFsp's driver makes a volume's Mount Manager drive.
  const FSP_FSCTL_MOUNTMGR: u32 = (9 << 16) | ((0x800 + b'm' as u32) << 2);
  /// Format: the most UTF-16 units `QueryDosDeviceW` writes for one drive letter's single target: the kernel volume
  /// name ([`super::VOLUME_UNITS`]), its terminator and the list's.
  const TARGET_UNITS: usize = super::VOLUME_UNITS + 2;

  /// The handles inherited from the anchor; ownership is unique before the mapping or dispatcher is built.
  pub struct Inherited {
    /// The held volume's control file object, which keeps the volume, and with it its drive.
    pub volume: OwnedHandle,
    /// The open ledger's section.
    pub ledger: OwnedHandle,
    /// The checked identity and posture of these objects.
    pub record: Record,
  }

  /// Duplicates offered for the anchor to hold. The mount still owns its originals until teardown.
  pub struct Offer {
    /// A duplicate of the control file object, held while the acknowledgement is owed.
    pub volume: OwnedHandle,
    /// Its checked description.
    pub record: Record,
  }

  /// The mount's drive: a Mount Manager drive WinFsp's driver made for the volume (A-369), so Windows resolves every
  /// file's final path on it (`GetFinalPathNameByHandleW`), which it does for no drive `DefineDosDeviceW` made. It holds
  /// nothing: the Mount Manager keeps the letter while the volume's device lives, and WinFsp's driver removes it with
  /// that device (`FspMountdevFini`, WinFsp v2.1 `src/sys/mountdev.c`), when the last copy of the volume's control
  /// handle closes, the daemon's and the anchor's. So the drive outlives a daemon whose volume the anchor holds, as the
  /// drive link A-198 held until A-369 did, and a daemon that exits never takes it from the anchor.
  pub(crate) struct Drive;

  impl Drive {
    /// Has WinFsp's driver make the record's drive letter a Mount Manager drive for the volume behind `control`
    /// (`FspVolumeUseMountmgr`, WinFsp v2.1 `src/sys/volume.c`), then checks that the letter names that volume in this
    /// session. Refused [`WinFspError::MountManagerNotAllowed`] where the machine has not allowed WinFsp's driver to
    /// make such a drive, and [`WinFspError::DriveLetterTaken`] where the letter names another volume.
    pub(crate) fn create(
      control: BorrowedHandle<'_>,
      record: &Record,
    ) -> Result<Self, WinFspError> {
      let letter = record.drive;
      let bytes = u32::try_from(size_of_val(&letter))
        .map_err(|_| WinFspError::Record(super::Refusal::Drive))?;
      let mut returned = 0u32;
      // SAFETY: the control handle is the live volume's for the call; the input is the record's two UTF-16 units with no
      // terminator, as WinFsp's own `FspFsctlUseMountmgr` sends them, and nothing is written back. The driver answers
      // this request at once, so no OVERLAPPED is passed on the overlapped handle, as WinFsp's own call passes none.
      let made = unsafe {
        DeviceIoControl(
          control.as_raw_handle(),
          FSP_FSCTL_MOUNTMGR,
          letter.as_ptr().cast(),
          bytes,
          std::ptr::null_mut(),
          0,
          &mut returned,
          std::ptr::null_mut(),
        )
      };
      if made == 0 {
        let code = std::io::Error::last_os_error().raw_os_error();
        return Err(match code.and_then(|code| u32::try_from(code).ok()) {
          Some(ERROR_ACCESS_DENIED) => WinFspError::MountManagerNotAllowed,
          Some(ERROR_ALREADY_EXISTS | ERROR_FILE_EXISTS) => WinFspError::DriveLetterTaken,
          _ => WinFspError::Os {
            call: "FSP_FSCTL_MOUNTMGR",
            code,
          },
        });
      }
      if !names_volume(record) {
        return Err(WinFspError::DriveLetterTaken);
      }
      Ok(Drive)
    }

    /// An adopted mount's drive: the Mount Manager kept it with the volume the anchor held.
    pub(crate) fn inherited() -> Self {
      Drive
    }
  }

  /// Whether the record's drive letter names its volume in this session: a definition local to the session hides the
  /// machine's, so a letter the Mount Manager gave the volume can still name another target here.
  fn names_volume(record: &Record) -> bool {
    let name: Vec<u16> = record.drive.iter().copied().chain(Some(0)).collect();
    let mut target = [0u16; TARGET_UNITS];
    let capacity = u32::try_from(target.len()).unwrap_or(0);
    // SAFETY: `name` is terminated, and `target` holds `capacity` writable UTF-16 units for the call.
    let units = unsafe { QueryDosDeviceW(name.as_ptr(), target.as_mut_ptr(), capacity) };
    let written = target
      .get(..usize::try_from(units).unwrap_or(0))
      .unwrap_or_default();
    let first = written.split(|&unit| unit == 0).next().unwrap_or_default();
    let volume = record
      .volume
      .split(|&unit| unit == 0)
      .next()
      .unwrap_or_default();
    !first.is_empty() && first == volume
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
      case_insensitive: true,
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

  /// A-551. Do: hand through the hold format a record of a drive that declared no `CaseSensitiveSearch` and one that
  /// did, then read a record as a daemon before A-551 wrote it, its flag byte the read-only bit alone. Expect: each
  /// posture kept, and the old record read as a drive that compares names with case, which every drive then declared.
  #[test]
  fn the_mount_record_keeps_how_its_drive_compares_names() {
    for case_insensitive in [true, false] {
      let expected = Record {
        case_insensitive,
        ..mount()
      };
      assert_eq!(Record::decode(&expected.encode().unwrap()).unwrap(), expected);
    }
    let mut before = mount().encode().unwrap();
    for read_only in [0u8, 1] {
      *before.last_mut().unwrap() = read_only;
      let restored = Record::decode(&before).unwrap();
      assert!(!restored.case_insensitive);
      assert_eq!(restored.read_only, read_only == 1);
    }
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
    *flags.last_mut().unwrap() = 4;
    assert_eq!(Record::decode(&flags), Err(Refusal::Flags));
    let mut invalid = mount();
    invalid.volume[1] = 0xd800;
    assert_eq!(invalid.encode(), Err(Refusal::Volume));
    let mut directory = mount();
    directory.drive = [u16::from(b'S'), u16::from(b'\\')];
    assert_eq!(directory.encode(), Err(Refusal::Drive));
  }
}
