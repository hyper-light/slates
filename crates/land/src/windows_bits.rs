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

/// Whether the volume's bits make a link a directory link (A-275's `LINK_DIRECTORY`, `FILE_ATTRIBUTE_DIRECTORY` on a
/// link): Windows makes one by creating the link's object as a directory, and removes it as a directory.
pub(crate) fn directory_link(volume: u16) -> bool {
  volume & WindowsAttributes::LINK_DIRECTORY != 0
}

#[cfg(test)]
mod tests {
  use super::*;

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
    assert_eq!(set_word(stale | TEMPORARY, 0, Made::File), TEMPORARY);
    assert_eq!(
      set_word(stale, WindowsAttributes::HIDDEN, Made::File),
      HIDDEN
    );
    assert_eq!(
      set_word(stale, 0, Made::File),
      NORMAL,
      "no bit: NORMAL alone"
    );
    assert_eq!(
      set_word(DIRECTORY | stale, WindowsAttributes::READONLY, Made::Dir),
      READONLY,
      "a directory's read-only is a bit"
    );
    assert_eq!(
      set_word(READONLY, 0, Made::File),
      READONLY,
      "a file's read-only is its mode's, kept"
    );
    assert_eq!(
      set_word(
        REPARSE_POINT | DIRECTORY,
        WindowsAttributes::LINK_DIRECTORY,
        Made::Link
      ),
      NORMAL,
      "a link's kind is never a bit set"
    );
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
