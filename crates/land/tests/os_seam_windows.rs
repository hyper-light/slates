//! The Windows writer's own promises, each verb driven directly over real NTFS (§4.15 step 6, AUD-29-04, A-185): a name
//! that exists is never replaced or written through, and a junction is moved or removed as itself, never as what it
//! names. A landing's oracles did not hold all of these: with an entry's open set to follow junctions, or a temporary's
//! create set to overwrite, `os`, `os_escape` and `os_removal` all still passed (jupiter, 2026-11-06), since the engine
//! picks fresh names and walks around links; a rename set to replace failed `os_removal` alone. Only a verb called on the
//! name that exists tells them apart. The directories are real ones in the build output (`CARGO_TARGET_TMPDIR`; A-50),
//! removed at the end.

#![cfg(windows)]
// Test harness code: an unwrap here is a failed test, and the outsider's edits are `std::fs`'s.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::disallowed_methods
)]

use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};

use slates_land::os::{OsLand, TargetRefusal};
use slates_vfs::host::{HostDir, HostError, LandFs};

/// Format: `EEXIST`, as the seam reports a name that exists.
const ERRNO_EXIST: i32 = 17;
/// Format: `EPERM`, as the seam reports a verb that would need a privilege slates never asks for (R10).
const ERRNO_PERM: i32 = 1;
/// Format: `EOPNOTSUPP`, as the seam reports a verb NTFS does not have.
const ERRNO_NOTSUP: i32 = 95;
/// Format: `FILE_ATTRIBUTE_REPARSE_POINT` (winnt.h): the entry is a link or a junction itself.
const REPARSE_POINT: u32 = 0x400;
/// Format: what the name that exists holds.
const EXISTING: &[u8] = b"the bytes of the name that exists";
/// Format: what the file behind the junction holds.
const SECRET: &[u8] = b"the file the junction reaches";

/// A target and a directory outside it holding one file, removed on drop.
struct Workspace {
  path: PathBuf,
}

impl Workspace {
  fn new(name: &str) -> Self {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
      .join(format!("slates-land-seam-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(path.join("target")).unwrap();
    std::fs::create_dir_all(path.join("outside")).unwrap();
    std::fs::write(path.join("outside/secret"), SECRET).unwrap();
    std::fs::write(path.join("target/existing"), EXISTING).unwrap();
    Self { path }
  }

  fn target(&self) -> PathBuf {
    self.path.join("target")
  }

  fn outside(&self) -> PathBuf {
    self.path.join("outside")
  }

  /// The writer over the target.
  fn writer(&self) -> (OsLand, HostDir) {
    let (writer, target) = OsLand::open_target(&self.target()).unwrap();
    (writer, target.dir)
  }

  /// `target/name` made a junction to `outside/`, as an outsider makes one without privilege.
  fn junction(&self, name: &str) {
    let backslashed = |path: &Path| path.to_string_lossy().replace('/', "\\");
    let made = std::process::Command::new("cmd")
      .args(["/C", "mklink", "/J"])
      .arg(backslashed(&self.target().join(name)))
      .arg(backslashed(&self.outside()))
      .output()
      .unwrap();
    assert!(made.status.success(), "mklink /J: {made:?}");
  }

  /// Everything outside the target is as it was made.
  fn assert_outside_untouched(&self) {
    assert_eq!(
      std::fs::read(self.outside().join("secret")).unwrap(),
      SECRET
    );
    assert_eq!(std::fs::read_dir(self.outside()).unwrap().count(), 1);
  }

  fn assert_existing_untouched(&self) {
    assert_eq!(
      std::fs::read(self.target().join("existing")).unwrap(),
      EXISTING
    );
  }
}

impl Drop for Workspace {
  fn drop(&mut self) {
    let _ = std::fs::remove_dir_all(&self.path);
  }
}

/// `result` is the refusal the seam gives a name that exists.
fn assert_name_exists<T: std::fmt::Debug>(result: Result<T, HostError>) {
  assert!(
    matches!(result, Err(HostError::Unavailable(ERRNO_EXIST))),
    "{result:?}"
  );
}

/// §4.15 step 4: a target is the writer's when it bears the owner this process's own creations get, as Unix's check is
/// `st_uid == geteuid`. Do: make a directory in this process and open it as a target. Expect: it opens, whatever the
/// token: an elevated token's default owner is `BUILTIN\Administrators`, not its user (CI's runners, an elevated shell;
/// measured 2026-11-06), and until then the writer refused every directory such a process made. Git Bash hides this: it
/// sets its token's default owner to the user, so run this from PowerShell to see it red.
#[test]
fn a_directory_this_process_makes_is_a_target_it_owns() {
  let space = Workspace::new("own");
  let opened = OsLand::open_target(&space.target());
  assert!(opened.is_ok(), "{:?}", opened.err());
}

/// Format: PowerShell's words for this process's token: its user's SID, then its default owner's (`TokenOwner`).
const TOKEN_SIDS: &str =
  "$i = [Security.Principal.WindowsIdentity]::GetCurrent(); $i.User.Value; $i.Owner.Value";

/// Runs `script` in PowerShell, which inherits this process's token, and returns its output's lines.
fn powershell(script: &str) -> Vec<String> {
  let out = std::process::Command::new("powershell")
    .args(["-NoProfile", "-NonInteractive", "-Command", script])
    .output()
    .unwrap();
  assert!(out.status.success(), "powershell: {out:?}");
  String::from_utf8_lossy(&out.stdout)
    .lines()
    .map(|line| line.trim().to_owned())
    .filter(|line| !line.is_empty())
    .collect()
}

/// This token's user SID and its default owner's SID.
fn token_sids() -> (String, String) {
  let lines = powershell(TOKEN_SIDS);
  let [user, default_owner] = lines.as_slice() else {
    panic!("a user and a default owner: {lines:?}")
  };
  (user.clone(), default_owner.clone())
}

/// The SID that owns `path`, as Windows reports it.
fn owner_sid(path: &Path) -> String {
  let script = format!(
    "(Get-Acl -LiteralPath '{}').GetOwner([Security.Principal.SecurityIdentifier]).Value",
    path.display()
  );
  powershell(&script).concat()
}

/// §4.15 step 4, the branch an elevated token takes: a target is the writer's when its owner is the token's default
/// owner, the owner this process's own creations get, though that is not the token's user. Do: make a directory in this
/// process, read the token's user and default owner and the directory's owner, and open the directory as a target.
/// Expect: the directory bears the default owner and not the user, and opens. On a token whose default owner is its
/// user (an unelevated one), the branch cannot be reached: skipped loudly. CI's runners and an elevated shell take it
/// (`BUILTIN\Administrators`, measured 2026-11-06).
#[test]
fn a_tree_bearing_the_tokens_default_owner_is_its_own_though_not_its_users() {
  let (user, default_owner) = token_sids();
  if user == default_owner {
    eprintln!(
      "SKIP: this token's default owner is its user {user}, so no tree it makes bears another owner"
    );
    return;
  }
  let space = Workspace::new("default-owner");
  let owner = owner_sid(&space.target());
  assert_eq!(
    owner, default_owner,
    "made by this process, so its default owner's"
  );
  assert_ne!(owner, user, "the branch under test: not the user's");
  let opened = OsLand::open_target(&space.target());
  assert!(opened.is_ok(), "{:?}", opened.err());
}

/// §4.15 step 4: a target another SID owns is refused, whatever the token. Do: read the owner of the system's own
/// directory, which `TrustedInstaller` owns on every Windows, check it is neither this token's user nor its default
/// owner, and open it as a target. Expect: `TargetNotOwned`, before anything could be written.
#[test]
fn a_target_another_user_owns_is_refused() {
  let system = PathBuf::from(std::env::var("SystemRoot").unwrap()).join("System32");
  let (user, default_owner) = token_sids();
  let owner = owner_sid(&system);
  assert!(
    owner != user && owner != default_owner,
    "the target's owner {owner} is another SID than the user {user} and the default owner {default_owner}"
  );
  let opened = OsLand::open_target(&system);
  assert!(
    matches!(opened, Err(TargetRefusal::TargetNotOwned)),
    "{:?}",
    opened.err()
  );
}

/// AUD-29-04: a move never replaces. Do: move a file onto a name that exists, first with nothing holding that name,
/// then with another process's kind of handle open on it. Expect: `EEXIST` both times, both files as they were.
#[test]
fn a_move_onto_a_name_that_exists_is_refused_and_replaces_nothing() {
  let space = Workspace::new("move");
  std::fs::write(space.target().join("mover"), b"mover").unwrap();
  let (mut writer, dir) = space.writer();
  assert_name_exists(writer.rename_noreplace(dir, "mover", dir, "existing"));
  let held = std::fs::File::open(space.target().join("existing")).unwrap();
  assert_name_exists(writer.rename_noreplace(dir, "mover", dir, "existing"));
  drop(held);
  space.assert_existing_untouched();
  assert_eq!(
    std::fs::read(space.target().join("mover")).unwrap(),
    b"mover"
  );
}

/// AUD-29-04: placing a landed file never replaces. Do: write a temporary and place it at a name that exists. Expect:
/// `EEXIST`, the name's bytes as they were; at a free name the same temporary lands whole.
#[test]
fn a_temporary_placed_onto_a_name_that_exists_is_refused_and_replaces_nothing() {
  let space = Workspace::new("place");
  let (mut writer, dir) = space.writer();
  let file = writer.create_temp(dir, ".slates-temporary").unwrap();
  writer.write_at(file, 0, b"landed").unwrap();
  assert_name_exists(writer.place(file, dir, "existing"));
  space.assert_existing_untouched();
  writer.place(file, dir, "free").unwrap();
  assert_eq!(
    std::fs::read(space.target().join("free")).unwrap(),
    b"landed"
  );
}

/// §4.15 step 6: a create never writes through a name. Do: create a temporary at a name that exists and at a junction
/// to a directory outside. Expect: `EEXIST` at the name, and at the junction `NotFile` (a file's create at a directory's
/// name, `STATUS_FILE_IS_A_DIRECTORY`, measured on jupiter); the file's bytes and the outside directory as they were.
#[test]
fn a_temporary_is_never_made_over_a_name_or_through_a_junction() {
  let space = Workspace::new("create");
  space.junction("link");
  let (mut writer, dir) = space.writer();
  assert_name_exists(writer.create_temp(dir, "existing"));
  let through = writer.create_temp(dir, "link");
  assert!(matches!(through, Err(HostError::NotFile)), "{through:?}");
  space.assert_existing_untouched();
  space.assert_outside_untouched();
}

/// §4.15 step 6: a directory's create is held to the same rule. Do: make a directory at a name that exists and at a
/// junction. Expect: `EEXIST` both times and nothing changed.
#[test]
fn a_directory_is_never_made_over_a_name_or_through_a_junction() {
  let space = Workspace::new("mkdir");
  space.junction("link");
  let (mut writer, dir) = space.writer();
  assert_name_exists(writer.mkdir(dir, "existing", 0o755));
  assert_name_exists(writer.mkdir(dir, "link", 0o755));
  space.assert_existing_untouched();
  space.assert_outside_untouched();
}

/// §4.15 step 6, §4.13: a move takes the entry itself. Do: an outsider makes a junction to a directory outside; move
/// it within the target. Expect: the junction has the new name and is still a junction, and the directory it reaches
/// keeps its own name and contents. Opened through the junction, the outside directory itself would be moved.
#[test]
fn a_move_of_a_junction_moves_the_junction_and_not_what_it_reaches() {
  let space = Workspace::new("move-junction");
  space.junction("link");
  let (mut writer, dir) = space.writer();
  writer.rename_noreplace(dir, "link", dir, "moved").unwrap();
  let moved = std::fs::symlink_metadata(space.target().join("moved")).unwrap();
  assert_ne!(moved.file_attributes() & REPARSE_POINT, 0, "{moved:?}");
  space.assert_outside_untouched();
}

/// §4.15 step 6, §4.13: a removal takes the entry itself. Do: remove one junction as a file and another as a directory.
/// Expect: both are gone, and the directory they reached keeps its contents.
#[test]
fn a_removal_of_a_junction_removes_the_junction_and_not_what_it_reaches() {
  let space = Workspace::new("remove-junction");
  space.junction("as-file");
  space.junction("as-directory");
  let (mut writer, dir) = space.writer();
  writer.unlink(dir, "as-file").unwrap();
  writer.rmdir(dir, "as-directory").unwrap();
  assert!(!space.target().join("as-file").exists());
  assert!(!space.target().join("as-directory").exists());
  space.assert_outside_untouched();
}

/// §4.15: each removal verb is refused the other's kind of entry. Do: unlink a directory and rmdir a file. Expect:
/// `NotFile` and `NotDirectory`, and both remain.
#[test]
fn a_removal_is_refused_the_other_kind_of_entry() {
  let space = Workspace::new("kinds");
  std::fs::create_dir(space.target().join("directory")).unwrap();
  let (mut writer, dir) = space.writer();
  let unlinked = writer.unlink(dir, "directory");
  assert!(matches!(unlinked, Err(HostError::NotFile)), "{unlinked:?}");
  let removed = writer.rmdir(dir, "existing");
  assert!(
    matches!(removed, Err(HostError::NotDirectory)),
    "{removed:?}"
  );
  assert!(space.target().join("directory").is_dir());
  space.assert_existing_untouched();
}

/// R10 and the crash oracle's `NTFS` shape: what the writer cannot do it says, typed. Do: ask its capabilities, make a
/// symbolic link and exchange two names. Expect: no exchange and no unnamed temporaries, the shape
/// `crates/land/tests/oracle.rs` crashes at every write instruction; the link refused `EPERM` and the exchange
/// `EOPNOTSUPP`, nothing made and both names as they were.
#[test]
fn what_the_writer_cannot_do_is_refused_typed_and_reported_in_its_capabilities() {
  let space = Workspace::new("refused");
  std::fs::write(space.target().join("other"), b"other").unwrap();
  let (mut writer, dir) = space.writer();
  let capabilities = writer.capabilities(dir).unwrap();
  assert!(!capabilities.exchange && !capabilities.unnamed_temporaries);
  let linked = writer.symlink(dir, "made", "existing");
  assert!(
    matches!(linked, Err(HostError::Unavailable(ERRNO_PERM))),
    "{linked:?}"
  );
  assert!(!space.target().join("made").exists());
  let exchanged = writer.exchange(dir, "existing", "other");
  assert!(
    matches!(exchanged, Err(HostError::Unavailable(ERRNO_NOTSUP))),
    "{exchanged:?}"
  );
  space.assert_existing_untouched();
  assert_eq!(
    std::fs::read(space.target().join("other")).unwrap(),
    b"other"
  );
}
