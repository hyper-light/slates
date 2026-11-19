//! The name WinFsp's driver gives the file node an open makes, on a drive over a folding volume (A-551; §4.4's name
//! policy, §4.6 "Windows"; B2's Windows half, after A-543's `slates_bridge_core::stored_path`). Pure and cfg-free, so
//! every rule here is unit-tested on any host; the Windows mount host hands the answer to WinFsp in the open's reply
//! (`FSP_FSCTL_OPEN_FILE_INFO`'s `NormalizedName`) and records the node's name in the open's ledger record and in
//! [`OpenNames`].
//!
//! **What the driver does with a name (WinFsp v2.1).** A drive over a folding volume does not declare
//! `CaseSensitiveSearch` (the host's `Mounted::new`), so the driver keys its open files by name compared without case:
//! two names are one when they have as many UTF-16 units and each pair is equal once upcased, ASCII by the driver's own
//! rule and the rest by the system's upcase table (`src/sys/util.c` 1543-1552, `RtlCompareUnicodeString` with
//! `CaseInSensitive`), and an open of a name equal so to an open node's joins that node (`src/sys/device.c` 743-760,
//! `src/sys/file.c` 662-666). A new node is named by the normalized name the open's reply carries, which the driver
//! refuses, `STATUS_OBJECT_NAME_INVALID`, unless it is equal so to the asked name; with none, by the asked name
//! upper-cased (`src/sys/create.c` 1035-1069). The DLL hands the driver a normalized name of at most
//! `FSP_FSCTL_TRANSACT_PATH_SIZEMAX` bytes and drops a longer one (`src/dll/fsop.c` 466 and 515). A rename renames the
//! node and every node beneath it, found by the same comparison, to the new name followed by each one's own rest
//! (`src/sys/file.c` 1573-1640), and the new name is the target directory's node name followed by the last component as
//! the caller typed it (`src/sys/fileinfo.c` 1814-1862). `FspFileSystemNotify` must name a node by its normalized name
//! (`inc/winfsp/winfsp.h` 1285-1299).
//!
//! **The rule.** So a node's name is chosen once, by whichever open makes the node, and every later open that joins it,
//! or makes a node beneath it, must agree with it byte for byte, or the mount's records ([`OpenNames`] and the open
//! ledger, matched byte for byte as the driver's own comment expects of normalized names, `src/sys/device.c` 756-758)
//! part from the driver's: a rename would move the driver's node and leave the record, and the notifications sent by
//! the record would miss the node. An open of `asked` is named component by component: each prefix as Windows already
//! holds it, by a name open at it or beneath it; past the last prefix Windows holds, each component as the volume
//! stores it (the stored path) when the driver takes that for the asked component, and otherwise as asked. Every
//! spelling the driver takes for one path therefore names one node by one name, the made spelling where nothing else
//! holds it, which is what `GetFinalPathNameByHandleW` then shows.
//!
//! **What it cannot reach.** Three cases, each in GAPS (A-551):
//! - Names the volume folds alike and Windows' table does not, since the table has no normalization and keeps letters
//!   such as the Kelvin sign apart from `K` (a decomposed `é` against a precomposed one): each such component is
//!   answered as asked, so Windows holds one node per such spelling, as it would of two names on NTFS.
//! - A path past [`NORMALIZED_UNITS_MAX`] units is named by the driver itself, upper-cased: the record keeps that name,
//!   but a rename of a directory above it that the records spell otherwise moves the driver's node and not the record.
//! - Names Windows' table folds alike and the volume does not (final sigma `ς` against `σ`, which the table upcases
//!   alike and Unicode lowercasing keeps apart) are two files to the volume and one node to the driver: an open of the
//!   second while the first is open joins the first's node, and its record takes that node's name.

use slates_vfs::names::NameEquivalence;

use crate::notify::OpenNames;

/// Format: `FSP_FSCTL_TRANSACT_PATH_SIZEMAX` (`1024 * sizeof(WCHAR)` bytes, `inc/winfsp/fsctl.h` 136) in UTF-16 units:
/// the longest normalized name WinFsp's DLL hands its driver (`src/dll/fsop.c` 466 and 515). For a longer name the
/// driver names the node itself.
pub const NORMALIZED_UNITS_MAX: usize = 1024;

/// Format: `\`, the separator of a WinFsp path, as a UTF-16 unit.
pub(crate) const SEPARATOR: u16 = 0x5C;

/// How WinFsp's driver compares a drive's names, as the drive's volume parameters declare (A-551).
#[derive(Clone, Copy, Debug, Default)]
pub enum NameSearch {
  /// `CaseSensitiveSearch`: two names are one only unit for unit. An exact volume's drive.
  #[default]
  CaseSensitive,
  /// No `CaseSensitiveSearch`: two names are one when they have as many UTF-16 units and each pair is equal once
  /// `upcase` maps it (`src/sys/util.c` 1543-1552). A folding volume's drive.
  CaseInsensitive {
    /// One UTF-16 unit as the system's upcase table maps it: the Windows host's `RtlUpcaseUnicodeChar`, the table the
    /// driver's `RtlCompareUnicodeString` and `RtlUpcaseUnicodeString` read. It agrees with the driver's own rule for
    /// ASCII, which maps only `a` to `z`.
    upcase: fn(u16) -> u16,
  },
}

impl NameSearch {
  /// The search the drive of a volume whose names follow `policy` declares: case-sensitive for an exact volume, and
  /// without case, by `upcase`, for a folding one.
  pub fn of(policy: NameEquivalence, upcase: fn(u16) -> u16) -> NameSearch {
    if policy.case_sensitive() {
      NameSearch::CaseSensitive
    } else {
      NameSearch::CaseInsensitive { upcase }
    }
  }

  /// Whether the drive declares `CaseSensitiveSearch`.
  pub fn case_sensitive(self) -> bool {
    matches!(self, NameSearch::CaseSensitive)
  }

  /// The UTF-16 units the driver compares `name` by: as they are, or each upcased.
  pub fn key(self, name: &str) -> Vec<u16> {
    match self {
      NameSearch::CaseSensitive => name.encode_utf16().collect(),
      NameSearch::CaseInsensitive { upcase } => name.encode_utf16().map(upcase).collect(),
    }
  }

  /// Whether the driver takes `a` and `b` for one name.
  pub fn same(self, a: &str, b: &str) -> bool {
    match self {
      NameSearch::CaseSensitive => a == b,
      NameSearch::CaseInsensitive { upcase } => a
        .encode_utf16()
        .map(upcase)
        .eq(b.encode_utf16().map(upcase)),
    }
  }
}

/// The name an open's file node has in WinFsp's driver, and whether the open's reply names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeName {
  /// The node's name: what the open's ledger record and [`OpenNames`] keep, and what a notification names it by.
  pub name: String,
  /// Whether the open's reply hands the name to the driver as its normalized name. Otherwise the driver names the node
  /// itself: as asked on a case-sensitive drive, and upper-cased past [`NORMALIZED_UNITS_MAX`] units.
  pub answered: bool,
}

/// The name of the file node an open of `asked` makes or joins, on the drive whose open names are `names` (see the
/// module docs). `stored` is `asked` with each component as the volume stores it (`slates_bridge_core::stored_path`),
/// when the open could name it: `None` names each component Windows does not hold as asked.
pub fn node_name(names: &OpenNames, asked: &str, stored: Option<&str>) -> NodeName {
  let search = names.search();
  let NameSearch::CaseInsensitive { upcase } = search else {
    // A drive that searches with case takes no normalized name and names the node as asked (`create.c` 1035).
    return NodeName {
      name: asked.to_owned(),
      answered: false,
    };
  };
  if asked.encode_utf16().count() > NORMALIZED_UNITS_MAX {
    return NodeName {
      name: upcased(asked, upcase),
      answered: false,
    };
  }
  if asked.contains(char::REPLACEMENT_CHARACTER) {
    // A name WinFsp sent with an unpaired surrogate reaches the host with U+FFFD in its place (the host's
    // `wide_to_string`), so no answer could equal the driver's name, and the driver would refuse the open. It is left
    // unanswered, as every open was before A-551, and the driver names the node itself.
    return NodeName {
      name: asked.to_owned(),
      answered: false,
    };
  }
  let spelled = Spelling::of(names, asked, stored);
  // The driver refuses a normalized name it does not take for the asked one (`create.c` 1060-1066), and always takes
  // the asked name itself: a walk that went astray answers that rather than refuse an open the volume made.
  let name = if search.same(&spelled, asked) {
    spelled
  } else {
    asked.to_owned()
  };
  NodeName {
    name,
    answered: true,
  }
}

/// `name` as the driver names a node it is given no normalized name for: each unit upcased (`create.c` 1041-1043,
/// `RtlUpcaseUnicodeString`). A table that mapped a surrogate unit would leave units no string holds; Windows' maps
/// none (from memory, to be checked against a live drive), and the asked name is kept if one ever did.
fn upcased(name: &str, upcase: fn(u16) -> u16) -> String {
  let units: Vec<u16> = name.encode_utf16().map(upcase).collect();
  String::from_utf16(&units).unwrap_or_else(|_| name.to_owned())
}

/// The walk that spells an open's path one component at a time (see the module docs).
struct Spelling<'n> {
  names: &'n OpenNames,
  search: NameSearch,
  /// The units the driver compares the path spelled so far by.
  key: Vec<u16>,
  /// The path spelled so far.
  spelled: String,
  /// Whether Windows may hold a name at or beneath the path so far. A name beneath a longer prefix is beneath this one
  /// too, so once Windows holds none at a prefix, the walk stops asking.
  held: bool,
}

impl Spelling<'_> {
  /// `asked` spelled as the module docs say, with `stored` its components as the volume stores them. A path that does
  /// not start at the root is no path WinFsp sends, and is kept as asked; a stored path of another shape is not used.
  fn of(names: &OpenNames, asked: &str, stored: Option<&str>) -> String {
    let Some(rest) = asked.strip_prefix('\\') else {
      return asked.to_owned();
    };
    let count = rest.split('\\').count();
    let stored: Vec<&str> = stored
      .and_then(|path| path.strip_prefix('\\'))
      .map(|path| path.split('\\').collect::<Vec<_>>())
      .filter(|components| components.len() == count)
      .unwrap_or_default();
    let mut walk = Spelling {
      names,
      search: names.search(),
      key: vec![SEPARATOR],
      spelled: "\\".to_owned(),
      held: true,
    };
    for (index, component) in rest.split('\\').enumerate() {
      walk.push(index > 0, component, stored.get(index).copied());
    }
    walk.spelled
  }

  /// Spells one more component: `asked` as the open names it, and `stored` as the volume stores it. `separated` when a
  /// component comes before it.
  fn push(&mut self, separated: bool, asked: &str, stored: Option<&str>) {
    if separated {
      self.key.push(SEPARATOR);
      self.spelled.push('\\');
    }
    self.key.extend(self.search.key(asked));
    if self.held {
      if let Some(held) = self.names.spelled_at(&self.key) {
        self.spelled = held;
        return;
      }
      self.held = false;
    }
    let search = self.search;
    let component = stored
      .filter(|stored| search.same(stored, asked))
      .unwrap_or(asked);
    self.spelled.push_str(component);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A model of Windows' upcase table for the tests: ASCII's `a` to `z`; `é` to `É`, which the table maps as Unicode
  /// does; final sigma `ς` and `σ` both to `Σ`, which the table maps alike where Unicode lowercasing keeps them apart
  /// (the table as ntfs-3g transcribes it, `uc_run_table` `{0x03C2, 0x03C3, -31}`, from memory); and everything else to
  /// itself, the Kelvin sign and a combining acute among it.
  fn model_upcase(unit: u16) -> u16 {
    match unit {
      0x61..=0x7A => unit - 0x20,
      0xE9 => 0xC9,
      0x3C2 | 0x3C3 => 0x3A3,
      _ => unit,
    }
  }

  /// A folding volume's drive holding the names `held` open, each by its own handle and inode.
  fn folding(held: &[&str]) -> OpenNames {
    let mut names = OpenNames::searching(NameSearch::CaseInsensitive {
      upcase: model_upcase,
    });
    for (handle, path) in (1u64..).zip(held) {
      names.opened(handle, handle, path);
    }
    names
  }

  /// The node an open of `asked` names on `names`, with `stored` as the volume stores it, answered.
  fn answered(names: &OpenNames, asked: &str, stored: &str) -> String {
    let node = node_name(names, asked, Some(stored));
    assert!(node.answered, "{asked} is answered");
    assert!(
      names.search().same(&node.name, asked),
      "the driver takes {} for {asked}",
      node.name
    );
    node.name
  }

  /// A-551 (B2), §4.4 name policy, T-4.6. Do: on a folding volume's drive holding nothing open, name opens of a file
  /// made as `\Src\Main.RS` under three spellings, and the root. Expect: each is answered with the made spelling, the
  /// name `GetFinalPathNameByHandleW` then shows and every spelling's one node takes; the root as itself. Before, an
  /// open was named as asked, so each spelling was a node of its own.
  #[test]
  fn every_spelling_of_a_file_on_a_folding_drive_names_one_node_by_its_made_spelling() {
    let names = folding(&[]);
    for asked in ["\\Src\\Main.RS", "\\SRC\\main.rs", "\\src\\MAIN.RS"] {
      assert_eq!(answered(&names, asked, "\\Src\\Main.RS"), "\\Src\\Main.RS");
    }
    assert_eq!(answered(&names, "\\", "\\"), "\\");
  }

  /// A-551, `create.c` 1035. Do: name an open of `\SRC\main.rs` on an exact volume's drive, which declares
  /// `CaseSensitiveSearch`. Expect: the node named as asked and nothing answered, since that driver takes no normalized
  /// name.
  #[test]
  fn a_case_sensitive_drive_names_the_node_as_asked_and_answers_nothing() {
    let names = OpenNames::new();
    assert_eq!(
      node_name(&names, "\\SRC\\main.rs", Some("\\Src\\Main.RS")),
      NodeName {
        name: "\\SRC\\main.rs".to_owned(),
        answered: false
      }
    );
  }

  /// A-551, `util.c` 1543-1552. Do: name opens whose stored spelling differs from the asked one by a letter the table
  /// upcases alike (`é`, `É`), by normalization (a decomposed `é` asked of a precomposed one), and by the Kelvin sign
  /// asked of `K`. Expect: the first answered with the stored spelling; the others with the asked component and the
  /// stored spelling of every component the driver takes, since the driver would refuse an open answered with a name it
  /// does not take for the asked one.
  #[test]
  fn a_stored_spelling_is_answered_only_where_the_driver_takes_it_for_the_asked_one() {
    let names = folding(&[]);
    assert_eq!(
      answered(&names, "\\CAF\u{c9}.TXT", "\\Caf\u{e9}.txt"),
      "\\Caf\u{e9}.txt"
    );
    assert_eq!(
      answered(&names, "\\SRC\\cafe\u{301}.txt", "\\Src\\caf\u{e9}.txt"),
      "\\Src\\cafe\u{301}.txt",
      "a decomposed spelling is answered as asked"
    );
    assert_eq!(
      answered(&names, "\\src\\\u{212a}.txt", "\\Src\\K.txt"),
      "\\Src\\\u{212a}.txt",
      "the Kelvin sign is no K to the table"
    );
    assert_eq!(
      node_name(&names, "\\src\\main.rs", None).name,
      "\\src\\main.rs",
      "with no stored path, each component as asked"
    );
  }

  /// A-551, `device.c` 743-760 and `file.c` 1573-1640. Do: hold `\src\a.txt` open (its directory was since respelled
  /// `\Src` from another path, so the stored path is `\Src\…`), then name opens of that file under another spelling, of
  /// a sibling beneath the held directory, and of a path that only shares a prefix as a string. Expect: the held file's
  /// own name, the sibling under the held spelling of its directory (a rename of the driver's `\src` node moves both,
  /// and the records match it byte for byte), and the third as the volume stores it.
  #[test]
  fn an_open_takes_the_spelling_windows_already_holds_its_path_or_a_prefix_by() {
    let names = folding(&["\\src\\a.txt"]);
    assert_eq!(
      answered(&names, "\\SRC\\A.TXT", "\\Src\\A.txt"),
      "\\src\\a.txt"
    );
    assert_eq!(
      answered(&names, "\\SRC\\b.txt", "\\Src\\b.txt"),
      "\\src\\b.txt"
    );
    assert_eq!(
      answered(&names, "\\SRCS\\c.txt", "\\Srcs\\c.txt"),
      "\\Srcs\\c.txt",
      "`\\srcs` is not beneath `\\src`"
    );
  }

  /// A-551, `fsop.c` 466. Do: name opens of a path of 1,024 units and of one of 1,025. Expect: the first answered with
  /// its stored spelling; the second unanswered and named as the driver names it, upper-cased, since the DLL drops a
  /// normalized name that long. A held name that long lends no spelling to a shorter open beneath its directory.
  #[test]
  fn a_path_past_the_dlls_limit_is_named_as_the_driver_upper_cases_it() {
    let names = folding(&[]);
    let fits = format!("\\d\\{}", "x".repeat(NORMALIZED_UNITS_MAX - 3));
    assert_eq!(fits.encode_utf16().count(), NORMALIZED_UNITS_MAX);
    assert_eq!(
      answered(&names, &fits, &fits.replace("\\d", "\\D")),
      fits.replace("\\d", "\\D")
    );
    let long = format!("\\d\\{}", "x".repeat(NORMALIZED_UNITS_MAX - 2));
    assert_eq!(
      node_name(&names, &long, Some(&long)),
      NodeName {
        name: long.to_uppercase(),
        answered: false
      }
    );
    let upper = long.to_uppercase();
    let held = folding(&[upper.as_str()]);
    assert_eq!(answered(&held, "\\d\\y", "\\d\\y"), "\\d\\y");
  }

  /// A-551, `create.c` 1060-1066. Do: name an open whose path reached the host with a replacement character, as one
  /// with an unpaired surrogate does. Expect: unanswered, since no answer could equal the driver's name for it.
  #[test]
  fn a_path_that_lost_an_unpaired_surrogate_is_not_answered() {
    let names = folding(&[]);
    let asked = "\\src\\\u{fffd}.txt";
    assert_eq!(
      node_name(&names, asked, Some("\\Src\\\u{fffd}.txt")),
      NodeName {
        name: asked.to_owned(),
        answered: false
      }
    );
  }

  /// A-551, the third case the module docs name. Do: hold `\σ.txt` open and name an open of `\ς.txt`, another file to
  /// the volume, whose name the table upcases alike. Expect: the held node's name, since the driver joins the open to
  /// that node whatever the reply says, and the records keep the driver's name for it.
  #[test]
  fn a_name_the_table_folds_alike_takes_the_open_nodes_name() {
    let names = folding(&["\\\u{3c3}.txt"]);
    assert_eq!(
      answered(&names, "\\\u{3c2}.txt", "\\\u{3c2}.txt"),
      "\\\u{3c3}.txt"
    );
  }
}
