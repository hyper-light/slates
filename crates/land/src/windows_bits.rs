//! The Windows attribute word a landed object takes (A-262's landing half, A-451): from the bits the volume keeps for it
//! (`slates_vfs::inode::WindowsAttributes`, at Windows' own values), so what a person set through the drive (`attrib +h`,
//! a system folder, Git for Windows hiding `.git`) is what the landing leaves on the disk. A file's read-only state is its
//! mode's write bits (A-262's one model, [`mode_word`]); every other bit the volume keeps is the volume's
//! ([`set_word`]); a link's directory kind is the object's kind, made so at its creation ([`directory_link`]), never a
//! bit `FILE_BASIC_INFO` can set. Every bit the volume keeps is one a caller may set (`FILE_ATTRIBUTE_VALID_SET_FLAGS`
//! names each) and NTFS and ReFS hold, but a file system that holds fewer drops the rest without a word: FAT keeps no
//! `OFFLINE` and no `NOT_CONTENT_INDEXED` ("the rest are silently dropped on the floor", `FatSetBasicInfo` in
//! Microsoft's FastFAT sample, `filesys/fastfat/fileinfo.c`). So the writer reads the word back and asks [`held`]
//! whether every bit took, and refuses the entry where one did not: a bit is landed or refused, never dropped. Pure, so
//! it is tested on every host; the Windows writer (`os_windows.rs`) is its only user.

use slates_vfs::inode::{Kind, WindowsAttributes};

/// Format: `FILE_ATTRIBUTE_READONLY` (winnt.h).
const READONLY: u32 = 0x0000_0001;
/// Format: `FILE_ATTRIBUTE_NORMAL` (winnt.h): a word with no other bit; a zero word in `FILE_BASIC_INFO` leaves the
/// attributes as they are, so a word with no bit is written as this alone.
const NORMAL: u32 = 0x0000_0080;
/// Format: `FILE_ATTRIBUTE_VALID_SET_FLAGS` (winnt.h): the bits a caller may set through `FILE_BASIC_INFO` (`READONLY`,
/// `HIDDEN`, `SYSTEM`, `ARCHIVE`, `NORMAL`, `TEMPORARY`, `OFFLINE`, `NOT_CONTENT_INDEXED`); the others (a directory, a
/// reparse point, compression, encryption, sparseness) are the file system's own and are never written.
const SETTABLE: u32 = 0x0000_31A7;
/// Format: a POSIX mode's write bits: a file with none of them is read-only (A-262).
const WRITE_BITS: u32 = 0o222;
/// Format: `FILE_WRITE_DATA` (winnt.h): the right that writes a file's data. `FlushFileBuffers` asks it of its handle
/// ("The file handle must have the GENERIC_WRITE access right"), and NTFS refuses it to a read-only file (MS-FSA
/// 2.1.5.1.2.1).
const WRITE_DATA: u32 = 0x0000_0002;
/// Format: `FILE_APPEND_DATA` (winnt.h): the other right MS-FSA 2.1.5.1.2.1 refuses a read-only file. Only the tests
/// read it, to show neither right is asked of one: the rule asks it nowhere, which is the point.
#[cfg(test)]
const APPEND_DATA: u32 = 0x0000_0004;
/// Format: `FILE_READ_ATTRIBUTES` (winnt.h): what a file's fingerprint and its attribute word are read with.
const READ_ATTRIBUTES: u32 = 0x0000_0080;
/// Format: `FILE_WRITE_ATTRIBUTES` (winnt.h): the right a set of `FILE_BASIC_INFO` asks (its attributes and times).
const WRITE_ATTRIBUTES: u32 = 0x0000_0100;
/// Format: `SYNCHRONIZE` (winnt.h): a handle whose calls complete before they return.
const SYNCHRONIZE: u32 = 0x0010_0000;
/// Format: `FILE_WRITE_THROUGH` (ntioapi.h), `NtCreateFile`'s create option behind `CreateFile`'s
/// `FILE_FLAG_WRITE_THROUGH`.
const WRITE_THROUGH: u32 = 0x0000_0002;
/// Format: `FILE_NON_DIRECTORY_FILE` (ntioapi.h): the open refuses a directory.
const NON_DIRECTORY: u32 = 0x0000_0040;
/// Format: `FILE_OPEN_REPARSE_POINT` (ntioapi.h): the object at the name itself, never one a link names.
const OPEN_REPARSE_POINT: u32 = 0x0020_0000;

/// What a landing made, as the volume's bits name it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Made {
  /// A file.
  File,
  /// A directory.
  Dir,
  /// A symbolic link (a file link or a directory link, [`directory_link`]).
  Link,
}

impl Made {
  /// The volume's kind for it, by which the volume masks the bits it keeps.
  fn kind(self) -> Kind {
    match self {
      Made::File => Kind::File,
      Made::Dir => Kind::Dir,
      Made::Link => Kind::Symlink,
    }
  }
}

/// The word that sets a file's read-only state from its mode (A-262: a file's read-only state is its mode): `READONLY`
/// where the mode has no write bit, none where it has one; every other settable bit the file holds (`current`) kept,
/// and `NORMAL` alone for a word with no bit.
pub(crate) fn mode_word(current: u32, mode: u32) -> u32 {
  let kept = current & SETTABLE & !(READONLY | NORMAL);
  let word = if mode & WRITE_BITS == 0 {
    kept | READONLY
  } else {
    kept
  };
  if word == 0 { NORMAL } else { word }
}

/// Every bit the volume keeps for an object made as `made` (`WindowsAttributes::kept` of all of them): hidden, system,
/// archive, offline and not indexed on each; a directory's read-only; a link's directory kind. A file's read-only state
/// is its mode, and a link's is nothing the volume keeps, so neither is among them.
fn tracked(made: Made) -> u32 {
  u32::from(WindowsAttributes::kept(u32::MAX, made.kind()).bits())
}

/// The bits the volume keeps for an object made as `made`, as it keeps them (`volume`), at Windows' values.
fn decided(volume: u16, made: Made) -> u32 {
  u32::from(WindowsAttributes::kept(u32::from(volume), made.kind()).bits())
}

/// The word that gives an object the landing made (`current`: what it holds now) the bits the volume keeps for it
/// (`volume`): each bit the volume keeps for the kind is the volume's, set or cleared ([`tracked`]: a directory's
/// read-only among them); every other settable bit stays as the object holds it, a file's read-only state (what its
/// mode made it, [`mode_word`], set first) and a `TEMPORARY` the volume keeps nothing of; and `NORMAL` alone for a
/// word with no bit. A link's directory kind is no settable bit, so it is never in the word ([`directory_link`]).
pub(crate) fn set_word(current: u32, volume: u16, made: Made) -> u32 {
  let untracked = current & SETTABLE & !tracked(made) & !NORMAL;
  let word = untracked | (decided(volume, made) & SETTABLE);
  if word == 0 { NORMAL } else { word }
}

/// Whether an object made as `made`, whose attributes read `read`, holds every bit the volume keeps for it as the
/// volume keeps it (`volume`): each set where the volume sets it and clear where it does not, a link's directory kind
/// among them. False where the file system dropped a bit it does not hold, which the writer refuses.
pub(crate) fn held(read: u32, volume: u16, made: Made) -> bool {
  read & tracked(made) == decided(volume, made)
}

/// How a file's metadata entry holds its file on Windows (A-458, A-472, A-519): the access its open asks, the open's
/// create options, and whether its batch's barrier flushes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MetaOpen {
  /// The access asked (`NtCreateFile`'s `DesiredAccess`).
  pub(crate) access: u32,
  /// The create options (`NtCreateFile`'s `CreateOptions`).
  pub(crate) options: u32,
  /// Whether its batch's barrier flushes it (`FlushFileBuffers`).
  pub(crate) flushed: bool,
}

/// The open a file's metadata entry takes (A-519). A file that is not read-only is opened to write its attributes and
/// its data, which its batch's barrier needs to flush it (A-472): `FlushFileBuffers`' handle "must have the GENERIC_WRITE
/// access right". NTFS opens a read-only file to no handle that writes its data: "If Open.File.FileType is DataFile and
/// (File.FileAttributes.FILE_ATTRIBUTE_READONLY && (DesiredAccess.FILE_WRITE_DATA || DesiredAccess.FILE_APPEND_DATA)),
/// then return STATUS_ACCESS_DENIED" (MS-FSA 2.1.5.1.2.1), the attributes' write not among them. So a read-only file is
/// opened to write its attributes alone, write-through: "File system metadata is always cached. Therefore, to store any
/// metadata changes to disk, the file must either be flushed or be opened with FILE_FLAG_WRITE_THROUGH" (Microsoft's
/// File Caching). Each set through it is stored as it returns, and its barrier leaves it out. Both are opened as files,
/// never through a link.
pub(crate) fn meta_open(read_only: bool) -> MetaOpen {
  let file = NON_DIRECTORY | OPEN_REPARSE_POINT;
  if read_only {
    MetaOpen {
      access: READ_ATTRIBUTES | WRITE_ATTRIBUTES | SYNCHRONIZE,
      options: file | WRITE_THROUGH,
      flushed: false,
    }
  } else {
    MetaOpen {
      access: READ_ATTRIBUTES | WRITE_ATTRIBUTES | WRITE_DATA | SYNCHRONIZE,
      options: file,
      flushed: true,
    }
  }
}

/// Whether a file's attribute word (`attributes`, read through a handle that writes none of its data) holds `READONLY`.
pub(crate) fn read_only(attributes: u32) -> bool {
  attributes & READONLY != 0
}

/// Whether the volume's bits make a link a directory link (A-275's `LINK_DIRECTORY`, `FILE_ATTRIBUTE_DIRECTORY` on a
/// link): Windows makes one by creating the link's object as a directory, and removes it as a directory.
pub(crate) fn directory_link(volume: u16) -> bool {
  volume & WindowsAttributes::LINK_DIRECTORY != 0
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A-519 (a read-only file's metadata entry). Do: take the open a file's metadata entry makes of a file that is not
  /// read-only and of one that is, and judge two attribute words. Expect the first to ask the right `FlushFileBuffers`
  /// needs (`FILE_WRITE_DATA`) and to be flushed by its batch's barrier; the second to ask neither right NTFS refuses a
  /// read-only file (`FILE_WRITE_DATA`, `FILE_APPEND_DATA`: MS-FSA 2.1.5.1.2.1), to ask the attributes' write and the
  /// write-through its metadata is stored by (Microsoft's File Caching), and to be left out of its barrier; both opened
  /// as files and never through a link; and a word read-only exactly where it holds `READONLY`. Before A-519 a read-only
  /// file's entry asked the first open, which NTFS refused, and failed alone.
  #[test]
  fn a_read_only_files_metadata_is_opened_write_through_to_its_attributes_alone() {
    let flushed = meta_open(false);
    assert_eq!(flushed.access & WRITE_DATA, WRITE_DATA);
    assert_eq!(flushed.options & WRITE_THROUGH, 0);
    assert!(flushed.flushed);
    let through = meta_open(true);
    assert_opened_to_its_attributes_alone_and_written_through(&through);
    for open in [flushed, through] {
      assert_eq!(
        open.options & (NON_DIRECTORY | OPEN_REPARSE_POINT),
        NON_DIRECTORY | OPEN_REPARSE_POINT
      );
      assert_eq!(open.access & READ_ATTRIBUTES, READ_ATTRIBUTES);
    }
    assert!(read_only(READONLY | NORMAL));
    assert!(!read_only(NORMAL));
  }

  /// A-519's read-only open: neither right NTFS refuses a read-only file (MS-FSA 2.1.5.1.2.1), the attributes' write,
  /// write-through for its metadata (Microsoft's File Caching), and no barrier flush, which would ask a right it lacks.
  fn assert_opened_to_its_attributes_alone_and_written_through(through: &MetaOpen) {
    assert_eq!(
      through.access & (WRITE_DATA | APPEND_DATA),
      0,
      "MS-FSA refuses either right to a read-only file"
    );
    assert_eq!(through.access & WRITE_ATTRIBUTES, WRITE_ATTRIBUTES);
    assert_eq!(through.options & WRITE_THROUGH, WRITE_THROUGH);
    assert!(!through.flushed, "a flush asks a right it does not hold");
  }

  /// A-519. The access rights and create options above are Windows' own, as windows-sys names them (on Windows).
  #[cfg(windows)]
  #[test]
  fn the_metadata_opens_values_are_windows_own() {
    use windows_sys::Wdk::Storage::FileSystem as nt;
    use windows_sys::Win32::Storage::FileSystem as fs;
    assert_eq!(WRITE_DATA, fs::FILE_WRITE_DATA);
    assert_eq!(APPEND_DATA, fs::FILE_APPEND_DATA);
    assert_eq!(READ_ATTRIBUTES, fs::FILE_READ_ATTRIBUTES);
    assert_eq!(WRITE_ATTRIBUTES, fs::FILE_WRITE_ATTRIBUTES);
    assert_eq!(SYNCHRONIZE, fs::SYNCHRONIZE);
    assert_eq!(WRITE_THROUGH, nt::FILE_WRITE_THROUGH);
    assert_eq!(NON_DIRECTORY, nt::FILE_NON_DIRECTORY_FILE);
    assert_eq!(OPEN_REPARSE_POINT, nt::FILE_OPEN_REPARSE_POINT);
    assert_eq!(READONLY, fs::FILE_ATTRIBUTE_READONLY);
  }

  /// Format: winnt.h's values the tests name.
  const HIDDEN: u32 = 0x0002;
  const SYSTEM: u32 = 0x0004;
  const DIRECTORY: u32 = 0x0010;
  const ARCHIVE: u32 = 0x0020;
  const TEMPORARY: u32 = 0x0100;
  const REPARSE_POINT: u32 = 0x0400;
  const OFFLINE: u32 = 0x1000;
  const NOT_CONTENT_INDEXED: u32 = 0x2000;

  /// The attributes the object holds once landed: what NTFS gave it as it was made (a file `ARCHIVE`, a directory
  /// `DIRECTORY`, a link `REPARSE_POINT` and, made as a directory, `DIRECTORY`), then the word set over the settable
  /// bits.
  fn landed(volume: u16, made: Made, mode: u32) -> u32 {
    let fresh = match made {
      Made::File => ARCHIVE,
      Made::Dir => DIRECTORY,
      Made::Link if directory_link(volume) => REPARSE_POINT | DIRECTORY,
      Made::Link => REPARSE_POINT,
    };
    let after_mode = match made {
      Made::File => (fresh & !SETTABLE) | mode_word(fresh, mode),
      Made::Dir | Made::Link => fresh,
    };
    (after_mode & !SETTABLE) | (set_word(after_mode, volume, made) & !NORMAL)
  }

  /// A-262's landing half, A-451. Do: land every combination of the bits the volume keeps (hidden, system, archive,
  /// offline, not indexed, a directory's read-only, a link's directory kind) on a file, a directory and a link, and read
  /// each landed object's attributes back as the volume reads a Windows disk (`WindowsAttributes::kept`). Expect the
  /// bits the volume keeps for that kind, every one, set and cleared alike: none dropped and none added.
  #[test]
  fn every_bit_the_volume_keeps_lands_and_reads_back_the_same() {
    let bits = [
      WindowsAttributes::READONLY,
      WindowsAttributes::HIDDEN,
      WindowsAttributes::SYSTEM,
      WindowsAttributes::ARCHIVE,
      WindowsAttributes::OFFLINE,
      WindowsAttributes::NOT_CONTENT_INDEXED,
      WindowsAttributes::LINK_DIRECTORY,
    ];
    for subset in 0..(1u32 << bits.len()) {
      let volume = bits
        .iter()
        .enumerate()
        .filter(|(at, _)| subset & (1 << at) != 0)
        .fold(0u16, |word, (_, bit)| word | bit);
      for made in [Made::File, Made::Dir, Made::Link] {
        let wanted = WindowsAttributes::kept(u32::from(volume), made.kind());
        let on_disk = landed(volume, made, 0o644);
        let read_back = WindowsAttributes::kept(on_disk, made.kind());
        assert_eq!(
          read_back, wanted,
          "{made:?} with the volume's {volume:#06x}"
        );
        assert!(
          held(on_disk, volume, made),
          "{made:?} with the volume's {volume:#06x}"
        );
      }
    }
  }

  /// A-451 (a bit is landed or refused, never dropped). Do: land each bit the volume keeps on each kind that keeps it,
  /// and read the object back whole, as a file system that dropped that bit leaves it (FAT drops `OFFLINE` and
  /// `NOT_CONTENT_INDEXED`), and as one holding it where the volume does not; then a directory link made as a file
  /// link, and the reverse. Expect the landed object held and every other one not, so the writer refuses it.
  #[test]
  fn a_bit_the_file_system_did_not_take_is_found_missing() {
    for made in [Made::File, Made::Dir, Made::Link] {
      for bit in [
        WindowsAttributes::READONLY,
        WindowsAttributes::HIDDEN,
        WindowsAttributes::SYSTEM,
        WindowsAttributes::ARCHIVE,
        WindowsAttributes::OFFLINE,
        WindowsAttributes::NOT_CONTENT_INDEXED,
      ] {
        if WindowsAttributes::kept(u32::from(bit), made.kind()).bits() == 0 {
          // A file's or a link's read-only state: no bit the volume keeps.
          continue;
        }
        let on_disk = landed(bit, made, 0o644);
        assert!(held(on_disk, bit, made), "{made:?} {bit:#06x}");
        assert!(
          !held(on_disk & !u32::from(bit), bit, made),
          "{made:?} without its {bit:#06x}"
        );
        assert!(
          !held(on_disk, 0, made),
          "{made:?} holding {bit:#06x}, which the volume does not"
        );
      }
    }
    let link_kind = WindowsAttributes::LINK_DIRECTORY;
    assert!(held(
      landed(link_kind, Made::Link, 0o644),
      link_kind,
      Made::Link
    ));
    assert!(
      !held(landed(0, Made::Link, 0o644), link_kind, Made::Link),
      "a directory link made as a file link"
    );
    assert!(
      !held(landed(link_kind, Made::Link, 0o644), 0, Made::Link),
      "a file link made as a directory link"
    );
  }

  /// A-262 (a file's read-only state is its mode). Do: land a file whose mode has no write bit and one whose mode has
  /// one, each with and without the volume's hidden bit. Expect read-only exactly where the mode has no write bit,
  /// whatever the volume's other bits, and the hidden bit as the volume keeps it.
  #[test]
  fn a_files_read_only_state_is_its_mode_and_its_other_bits_the_volumes() {
    for hidden in [0, WindowsAttributes::HIDDEN] {
      let read_only = landed(hidden, Made::File, 0o444);
      let writable = landed(hidden, Made::File, 0o644);
      assert_eq!(read_only & READONLY, READONLY, "{hidden:#x}");
      assert_eq!(writable & READONLY, 0, "{hidden:#x}");
      assert_eq!(read_only & HIDDEN, u32::from(hidden));
      assert_eq!(writable & HIDDEN, u32::from(hidden));
    }
  }

  /// A-451. Do: set the volume's bits on objects holding bits of their own: a file's `TEMPORARY` (a bit the volume keeps
  /// nothing of), and stale hidden, system, offline and not-indexed bits the volume does not hold. Expect the volume's
  /// bits to replace every bit it keeps, the untracked bit kept, the file system's own bits never in the word, and
  /// `NORMAL` alone for a word with no bit.
  #[test]
  fn the_word_replaces_what_the_volume_keeps_and_keeps_what_it_does_not() {
    let stale = HIDDEN | SYSTEM | OFFLINE | NOT_CONTENT_INDEXED | ARCHIVE;
    let rows: [(u32, u16, Made, u32, &str); 6] = [
      (
        stale | TEMPORARY,
        0,
        Made::File,
        TEMPORARY,
        "the untracked bit kept",
      ),
      (
        stale,
        WindowsAttributes::HIDDEN,
        Made::File,
        HIDDEN,
        "the volume's bit replaces the stale ones",
      ),
      (stale, 0, Made::File, NORMAL, "no bit: NORMAL alone"),
      (
        DIRECTORY | stale,
        WindowsAttributes::READONLY,
        Made::Dir,
        READONLY,
        "a directory's read-only is a bit",
      ),
      (
        READONLY,
        0,
        Made::File,
        READONLY,
        "a file's read-only is its mode's, kept",
      ),
      (
        REPARSE_POINT | DIRECTORY,
        WindowsAttributes::LINK_DIRECTORY,
        Made::Link,
        NORMAL,
        "a link's kind is never a bit set",
      ),
    ];
    for (current, volume, made, word, why) in rows {
      assert_eq!(set_word(current, volume, made), word, "{why}");
    }
  }

  /// A-262, A-451. Do: ask whether the volume's bits make a link a directory link, and the word a file's mode sets.
  /// Expect a link a directory link exactly where its bits hold the directory kind, and a file read-only exactly where its
  /// mode has no write bit, its other bits kept.
  #[test]
  fn a_links_kind_is_its_bits_and_a_files_read_only_state_its_mode() {
    assert!(directory_link(
      WindowsAttributes::LINK_DIRECTORY | WindowsAttributes::HIDDEN
    ));
    assert!(!directory_link(WindowsAttributes::HIDDEN));
    assert_eq!(
      mode_word(ARCHIVE | HIDDEN, 0o444),
      ARCHIVE | HIDDEN | READONLY
    );
    assert_eq!(mode_word(READONLY, 0o644), NORMAL);
  }
}
