//! winfstest's twenty scripts, transcribed from Python 2 into calls on [`Script`] (A-214). Each function is one upstream
//! file at `billziss-gh/secfs.test@6ac65cd` (the commit WinFsp v2.1 pins), named by its path and the SHA-256 of the
//! file it was transcribed from, and makes the same calls with the same expectations in the same order, so a case's
//! number here is its number upstream. The transcription was made by a one-time tool and reviewed by hand; it is held
//! to the upstream scripts by the golden test in `mod.rs`, which compares every call, expectation and `testeval` source
//! line against those the unchanged upstream scripts make under a stand-in driver.
//!
//! The scripts are winfstest's, and its licence travels with this transcription:
//!
//! Copyright (c) 2015, Bill Zissimopoulos. All rights reserved.
//!
//! Redistribution and use in source and binary forms, with or without modification, are permitted provided that the
//! following conditions are met:
//!
//! 1. Redistributions of source code must retain the above copyright notice, this list of conditions and the following
//!    disclaimer.
//! 2. Redistributions in binary form must reproduce the above copyright notice, this list of conditions and the
//!    following disclaimer in the documentation and/or other materials provided with the distribution.
//! 3. Neither the name of the copyright holder nor the names of its contributors may be used to endorse or promote
//!    products derived from this software without specific prior written permission.
//!
//! THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES,
//! INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
//! DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
//! SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
//! SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY,
//! WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
//! OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

use super::script::Script;

/// One transcribed script.
pub(crate) struct Transcribed {
  /// The upstream file, relative to `winfstest/` (`t/base/00.t`): the case ids' file part.
  pub(crate) path: &'static str,
  /// The SHA-256 of the upstream file the transcription was made from.
  pub(crate) sha256: &'static str,
  /// The transcription.
  pub(crate) run: fn(&mut Script<'_>),
}

/// The NT name an absolute link's target is stored under: `"\\??\\" + (dstpath if "\\" != dstpath[:1] else "UNC" +
/// dstpath[1:])`, as `t/reparse/02.t` and `03.t` state it.
fn nt_substitute_name(path: &str) -> String {
  match path.strip_prefix('\\') {
    Some(rest) => format!("\\??\\UNC{rest}"),
    None => format!("\\??\\{path}"),
  }
}

/// The name an absolute link prints: `dstpath if "\\" != dstpath[:1] else dstpath[1:]`.
fn print_name(path: &str) -> &str {
  path.strip_prefix('\\').unwrap_or(path)
}

/// `t/base/00.t` (sha256 `80606107049d57fe87037a9264fc33d1a9b009cef80bd35fb1573b9268f53ba4`).
fn base_00(t: &mut Script<'_>) {
  // CreateFile CreationDisposition
  // DeleteFile
  let name = t.name();
  t.ok(&format!(
    "CreateFile {} GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.err(
    &format!(
      "CreateFile {} GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
      name
    ),
    "ERROR_FILE_EXISTS",
  );
  t.ok(&format!("DeleteFile {}", name));
  t.err(&format!("DeleteFile {}", name), "ERROR_FILE_NOT_FOUND");
  t.ok(&format!(
    "CreateFile {} GENERIC_WRITE 0 0 CREATE_ALWAYS FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.ok(&format!(
    "CreateFile {} GENERIC_WRITE 0 0 CREATE_ALWAYS FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.err(
    &format!(
      "-e CreateFile {} GENERIC_WRITE 0 0 CREATE_ALWAYS FILE_ATTRIBUTE_NORMAL 0",
      name
    ),
    "ERROR_ALREADY_EXISTS",
  );
  t.ok(&format!("DeleteFile {}", name));
  t.ok(&format!(
    "CreateFile {} GENERIC_WRITE 0 0 OPEN_ALWAYS FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.ok(&format!(
    "CreateFile {} GENERIC_WRITE 0 0 OPEN_ALWAYS FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.err(
    &format!(
      "-e CreateFile {} GENERIC_WRITE 0 0 OPEN_ALWAYS FILE_ATTRIBUTE_NORMAL 0",
      name
    ),
    "ERROR_ALREADY_EXISTS",
  );
  t.ok(&format!("DeleteFile {}", name));
  t.err(
    &format!(
      "CreateFile {} GENERIC_WRITE 0 0 OPEN_EXISTING FILE_ATTRIBUTE_NORMAL 0",
      name
    ),
    "ERROR_FILE_NOT_FOUND",
  );
  t.ok(&format!(
    "CreateFile {} GENERIC_WRITE 0 0 CREATE_ALWAYS FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.ok(&format!(
    "CreateFile {} GENERIC_WRITE 0 0 OPEN_EXISTING FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.ok(&format!("DeleteFile {}", name));
  t.err(
    &format!(
      "CreateFile {} GENERIC_WRITE 0 0 TRUNCATE_EXISTING FILE_ATTRIBUTE_NORMAL 0",
      name
    ),
    "ERROR_FILE_NOT_FOUND",
  );
  t.ok(&format!(
    "CreateFile {} GENERIC_WRITE 0 0 CREATE_ALWAYS FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.ok(&format!(
    "CreateFile {} GENERIC_WRITE 0 0 TRUNCATE_EXISTING FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.ok(&format!("DeleteFile {}", name));
  t.err(
    &format!(
      "CreateFile {}\\bar GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
      name
    ),
    "ERROR_PATH_NOT_FOUND",
  );
}

/// `t/base/01.t` (sha256 `08a40af704003632a1db28ded4705ea2960057f3ca528ac1c111028bf262ca25`).
fn base_01(t: &mut Script<'_>) {
  // CreateFile FlagsAndAttributes
  // DeleteFile
  let name = t.name();
  t.ok(&format!(
    "CreateFile {} GENERIC_WRITE 0 0 CREATE_ALWAYS FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileAttributes\"] == 0x20",
    |r| r.first_int("FileAttributes") == Some(0x20),
  );
  t.ok(&format!(
    "CreateFile {} GENERIC_WRITE 0 0 CREATE_ALWAYS FILE_ATTRIBUTE_READONLY 0",
    name
  ));
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileAttributes\"] == 0x21",
    |r| r.first_int("FileAttributes") == Some(0x21),
  );
  t.ok(&format!("SetFileAttributes {} FILE_ATTRIBUTE_NORMAL", name));
  t.ok(&format!(
    "CreateFile {} GENERIC_WRITE 0 0 CREATE_ALWAYS FILE_ATTRIBUTE_SYSTEM 0",
    name
  ));
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileAttributes\"] == 0x24",
    |r| r.first_int("FileAttributes") == Some(0x24),
  );
  t.ok(&format!("SetFileAttributes {} FILE_ATTRIBUTE_NORMAL", name));
  t.ok(&format!(
    "CreateFile {} GENERIC_WRITE 0 0 CREATE_ALWAYS FILE_ATTRIBUTE_HIDDEN 0",
    name
  ));
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileAttributes\"] == 0x22",
    |r| r.first_int("FileAttributes") == Some(0x22),
  );
  t.ok(&format!("DeleteFile {}", name));
  t.ok(&format!(
    "CreateFile {} GENERIC_WRITE 0 0 CREATE_ALWAYS FILE_ATTRIBUTE_READONLY 0",
    name
  ));
  t.err(&format!("DeleteFile {}", name), "ERROR_ACCESS_DENIED");
  t.ok(&format!("SetFileAttributes {} FILE_ATTRIBUTE_NORMAL", name));
  t.ok(&format!("DeleteFile {}", name));
}

/// `t/base/02.t` (sha256 `2c8a132c33e57c5e371d4c30d6712ece5325985450982f129b74522a11b81b0b`).
fn base_02(t: &mut Script<'_>) {
  // CreateDirectory
  // RemoveDirectory
  let name = t.name();
  t.ok(&format!(
    "CreateFile {} GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.err(
    &format!("CreateDirectory {} 0", name),
    "ERROR_ALREADY_EXISTS",
  );
  t.err(&format!("RemoveDirectory {}", name), "ERROR_DIRECTORY");
  t.ok(&format!("DeleteFile {}", name));
  t.ok(&format!("CreateDirectory {} 0", name));
  t.err(
    &format!("CreateDirectory {} 0", name),
    "ERROR_ALREADY_EXISTS",
  );
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileAttributes\"] == 0x10",
    |r| r.first_int("FileAttributes") == Some(0x10),
  );
  t.err(&format!("DeleteFile {}", name), "ERROR_ACCESS_DENIED");
  t.ok(&format!("RemoveDirectory {}", name));
  t.err(&format!("RemoveDirectory {}", name), "ERROR_FILE_NOT_FOUND");
  t.ok(&format!("CreateDirectory {} 0", name));
  t.err(
    &format!(
      "CreateFile {} GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
      name
    ),
    "ERROR_ACCESS_DENIED",
  );
  t.ok(&format!("RemoveDirectory {}", name));
}

/// `t/base/03.t` (sha256 `f75cba61239badc357262fcc98bad483092d31437ae2aca07008182888c934c1`).
fn base_03(t: &mut Script<'_>) {
  // FindFiles (FindFirstFile, FindNextFile, FindClose)
  let name = t.name();
  t.ok(&format!("CreateDirectory {} 0", name));
  t.ok(&format!(
    "CreateFile {}\\foo GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.err(&format!("RemoveDirectory {}", name), "ERROR_DIR_NOT_EMPTY");
  t.err(&format!("RemoveDirectory {}\\foo", name), "ERROR_DIRECTORY");
  t.ok(&format!("DeleteFile {}\\foo", name));
  t.ok(&format!("RemoveDirectory {}", name));
  t.ok(&format!("CreateDirectory {} 0", name));
  t.ok(&format!(
    "CreateFile {}\\foo GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.ok(&format!(
    "CreateFile {}\\bar GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  let r = t.ok(&format!("FindFiles {}\\*", name));
  let s = r.text_set("FileName");
  t.eval(s.len() == 4, "testeval(len(s) == 4)");
  t.eval(s.contains("."), "testeval(\".\" in s)");
  t.eval(s.contains(".."), "testeval(\"..\" in s)");
  t.eval(s.contains("foo"), "testeval(\"foo\" in s)");
  t.eval(s.contains("bar"), "testeval(\"bar\" in s)");
  t.ok(&format!("DeleteFile {}\\bar", name));
  t.ok(&format!("DeleteFile {}\\foo", name));
  t.ok(&format!("RemoveDirectory {}", name));
}

/// `t/base/04.t` (sha256 `4480fcaab742123b4ce22acf00b2ee214da3198a57c1c32e77ee9f98bafa00c6`).
fn base_04(t: &mut Script<'_>) {
  // MoveFileEx
  let name = t.name();
  t.ok(&format!("CreateDirectory {} 0", name));
  t.ok(&format!(
    "CreateFile {}\\foo GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.ok(&format!(
    "CreateFile {}\\baz GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_READONLY 0",
    name
  ));
  t.ok(&format!("MoveFileEx {}\\foo {}\\bar 0", name, name));
  t.err(
    &format!("GetFileInformation {}\\foo", name),
    "ERROR_FILE_NOT_FOUND",
  );
  t.ok(&format!("GetFileInformation {}\\bar", name));
  t.err(
    &format!("MoveFileEx {}\\bar {}\\baz 0", name, name),
    "ERROR_ALREADY_EXISTS",
  );
  t.ok(&format!("GetFileInformation {}\\bar", name));
  t.ok(&format!("GetFileInformation {}\\baz", name));
  t.err(
    &format!(
      "MoveFileEx {}\\bar {}\\baz MOVEFILE_REPLACE_EXISTING",
      name, name
    ),
    "ERROR_ACCESS_DENIED",
  );
  t.ok(&format!(
    "SetFileAttributes {}\\baz FILE_ATTRIBUTE_NORMAL",
    name
  ));
  t.ok(&format!(
    "MoveFileEx {}\\bar {}\\baz MOVEFILE_REPLACE_EXISTING",
    name, name
  ));
  t.err(
    &format!("GetFileInformation {}\\bar", name),
    "ERROR_FILE_NOT_FOUND",
  );
  t.ok(&format!("GetFileInformation {}\\baz", name));
  t.ok(&format!("DeleteFile {}\\baz", name));
  t.ok(&format!("RemoveDirectory {}", name));
  t.ok(&format!("CreateDirectory {} 0", name));
  t.ok(&format!("CreateDirectory {}\\foo 0", name));
  t.ok(&format!("CreateDirectory {}\\baz 0", name));
  t.ok(&format!("MoveFileEx {}\\foo {}\\bar 0", name, name));
  t.err(
    &format!("GetFileInformation {}\\foo", name),
    "ERROR_FILE_NOT_FOUND",
  );
  t.ok(&format!("GetFileInformation {}\\bar", name));
  t.err(
    &format!("MoveFileEx {}\\bar {}\\baz 0", name, name),
    "ERROR_ALREADY_EXISTS",
  );
  t.ok(&format!("GetFileInformation {}\\bar", name));
  t.ok(&format!("GetFileInformation {}\\baz", name));
  t.err(
    &format!(
      "MoveFileEx {}\\bar {}\\baz MOVEFILE_REPLACE_EXISTING",
      name, name
    ),
    "ERROR_ACCESS_DENIED",
  );
  t.ok(&format!("GetFileInformation {}\\bar", name));
  t.ok(&format!("GetFileInformation {}\\baz", name));
  t.ok(&format!("RemoveDirectory {}\\baz", name));
  t.ok(&format!(
    "MoveFileEx {}\\bar {}\\baz MOVEFILE_REPLACE_EXISTING",
    name, name
  ));
  t.err(
    &format!("GetFileInformation {}\\bar", name),
    "ERROR_FILE_NOT_FOUND",
  );
  t.ok(&format!("GetFileInformation {}\\baz", name));
  t.ok(&format!("RemoveDirectory {}\\baz", name));
  t.ok(&format!("RemoveDirectory {}", name));
  t.ok(&format!("CreateDirectory {} 0", name));
  t.ok(&format!("CreateDirectory {}\\foo 0", name));
  t.ok(&format!("CreateDirectory {}\\bar 0", name));
  t.ok(&format!(
    "CreateFile {}\\foo\\hello GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.ok(&format!(
    "MoveFileEx {}\\foo\\hello {}\\bar\\world 0",
    name, name
  ));
  t.err(
    &format!("GetFileInformation {}\\foo\\hello", name),
    "ERROR_FILE_NOT_FOUND",
  );
  t.ok(&format!("GetFileInformation {}\\bar\\world", name));
  t.ok(&format!("DeleteFile {}\\bar\\world", name));
  t.ok(&format!("RemoveDirectory {}\\bar", name));
  t.ok(&format!("RemoveDirectory {}\\foo", name));
  t.ok(&format!("RemoveDirectory {}", name));
}

/// `t/base/05.t` (sha256 `6cf2e237456f3e91924125db35b6977b690d80182d4e04c4db3a574473b1cfc0`).
fn base_05(t: &mut Script<'_>) {
  // SetEndOfFile
  // TRUNCATE_EXISTING
  let name = t.name();
  t.ok(&format!(
    "CreateFile {} GENERIC_WRITE 0 0 CREATE_ALWAYS FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileSize\"] == 0",
    |r| r.first_int("FileSize") == Some(0),
  );
  t.ok(&format!("SetEndOfFile {} 42", name));
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileSize\"] == 42",
    |r| r.first_int("FileSize") == Some(42),
  );
  t.ok(&format!("SetEndOfFile {} 13", name));
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileSize\"] == 13",
    |r| r.first_int("FileSize") == Some(13),
  );
  t.ok(&format!("DeleteFile {}", name));
  t.ok(&format!(
    "CreateFile {} GENERIC_WRITE 0 0 CREATE_ALWAYS FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.ok(&format!("SetEndOfFile {} 42", name));
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileSize\"] == 42",
    |r| r.first_int("FileSize") == Some(42),
  );
  t.ok(&format!(
    "CreateFile {} GENERIC_WRITE 0 0 TRUNCATE_EXISTING 0 0",
    name
  ));
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileSize\"] == 0",
    |r| r.first_int("FileSize") == Some(0),
  );
  t.ok(&format!("DeleteFile {}", name));
}

/// `t/base/06.t` (sha256 `ae4c60fc3f3bc916fab1507de73e9eaf4590340b55f13dbb61fcce48691e411c`).
fn base_06(t: &mut Script<'_>) {
  // SetFileTime
  let name = t.name();
  t.ok(&format!(
    "CreateFile {} GENERIC_WRITE 0 0 CREATE_ALWAYS FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  let r = t.ok(&format!("GetFileInformation {}", name));
  // btime = r[0]["CreationTime"]: upstream reads it and checks nothing with it
  // atime = r[0]["LastAccessTime"]: upstream reads it and checks nothing with it
  let mtime = r.first_text("LastWriteTime").map(str::to_owned);
  t.ok(&format!("SetFileTime {} 2134 2134-10-11T12:34:56 0", name));
  let r = t.ok(&format!("GetFileInformation {}", name));
  t.eval(
    r.first_text("CreationTime") == Some("2134-01-01T00:00:00Z"),
    "testeval(\"2134-01-01T00:00:00Z\" == r[0][\"CreationTime\"])",
  );
  t.eval(
    r.first_text("LastAccessTime") == Some("2134-10-11T12:34:56Z"),
    "testeval(\"2134-10-11T12:34:56Z\" == r[0][\"LastAccessTime\"])",
  );
  t.eval(
    mtime.is_some() && mtime.as_deref() == r.first_text("LastWriteTime"),
    "testeval(mtime == r[0][\"LastWriteTime\"])",
  );
  t.ok(&format!("DeleteFile {}", name));
}

/// `t/base/07.t` (sha256 `8d96716a8500197a4460bc75b8a1b57423546c050450a7b6225dd89a834b7ee6`).
fn base_07(t: &mut Script<'_>) {
  // SetFileAttributes
  let name = t.name();
  t.ok(&format!(
    "CreateFile {} GENERIC_WRITE 0 0 CREATE_ALWAYS FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileAttributes\"] == 0x20",
    |r| r.first_int("FileAttributes") == Some(0x20),
  );
  t.ok(&format!(
    "SetFileAttributes {} FILE_ATTRIBUTE_READONLY",
    name
  ));
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileAttributes\"] == 0x1",
    |r| r.first_int("FileAttributes") == Some(0x1),
  );
  t.ok(&format!("SetFileAttributes {} FILE_ATTRIBUTE_SYSTEM", name));
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileAttributes\"] == 0x4",
    |r| r.first_int("FileAttributes") == Some(0x4),
  );
  t.ok(&format!("SetFileAttributes {} FILE_ATTRIBUTE_HIDDEN", name));
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileAttributes\"] == 0x2",
    |r| r.first_int("FileAttributes") == Some(0x2),
  );
  t.ok(&format!(
    "SetFileAttributes {} FILE_ATTRIBUTE_ARCHIVE",
    name
  ));
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileAttributes\"] == 0x20",
    |r| r.first_int("FileAttributes") == Some(0x20),
  );
  t.ok(&format!("DeleteFile {}", name));
}

/// `t/base/08.t` (sha256 `44d0bbcc2e3b6c4e9202e0544bfe0f33d4f441fce7ae97361a39e48e0cf80ee6`).
fn base_08(t: &mut Script<'_>) {
  // STATUS_DELETE_PENDING
  // FindFiles with delete pending
  let name = t.name();
  let held = t.hold(&format!("CreateFile {} GENERIC_WRITE FILE_SHARE_READ+FILE_SHARE_WRITE+FILE_SHARE_DELETE 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0", name));
  t.ok(&format!("CreateFile {} GENERIC_READ FILE_SHARE_READ+FILE_SHARE_WRITE+FILE_SHARE_DELETE 0 OPEN_EXISTING 0 0", name));
  t.ok(&format!("DeleteFile {}", name));
  // STATUS_DELETE_PENDING translated to ERROR_ACCESS_DENIED below
  t.err(&format!("CreateFile {} GENERIC_READ FILE_SHARE_READ+FILE_SHARE_WRITE+FILE_SHARE_DELETE 0 OPEN_EXISTING 0 0", name), "ERROR_ACCESS_DENIED");
  t.release(held);
  t.ok(&format!("CreateDirectory {} 0", name));
  let held = t.hold(&format!("CreateFile {}\\foo GENERIC_WRITE FILE_SHARE_READ+FILE_SHARE_WRITE+FILE_SHARE_DELETE 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0", name));
  t.ok(&format!("DeleteFile {}\\foo", name));
  let r = t.ok(&format!("FindFiles {}\\*", name));
  let s = r.text_set("FileName");
  t.eval(s.len() == 3, "testeval(len(s) == 3)");
  t.eval(s.contains("."), "testeval(\".\" in s)");
  t.eval(s.contains(".."), "testeval(\"..\" in s)");
  t.eval(s.contains("foo"), "testeval(\"foo\" in s)");
  t.release(held);
  let r = t.ok(&format!("FindFiles {}\\*", name));
  let s = r.text_set("FileName");
  t.eval(s.len() == 2, "testeval(len(s) == 2)");
  t.eval(s.contains("."), "testeval(\".\" in s)");
  t.eval(s.contains(".."), "testeval(\"..\" in s)");
  t.ok(&format!("RemoveDirectory {}", name));
}

/// `t/base/09.t` (sha256 `9add1871777f6645ab3a1865afe3aab22f1ec4240270de5dfa2305295009bf46`).
fn base_09(t: &mut Script<'_>) {
  // CreateFile share mode
  let name = t.name();
  let held = t.hold(&format!(
    "CreateFile {} GENERIC_WRITE FILE_SHARE_READ 0 CREATE_ALWAYS FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.err(
    &format!(
      "CreateFile {} GENERIC_READ FILE_SHARE_READ 0 OPEN_EXISTING 0 0",
      name
    ),
    "ERROR_SHARING_VIOLATION",
  );
  t.ok(&format!(
    "CreateFile {} GENERIC_READ FILE_SHARE_WRITE 0 OPEN_EXISTING 0 0",
    name
  ));
  t.err(
    &format!(
      "CreateFile {} DELETE FILE_SHARE_DELETE 0 OPEN_EXISTING 0 0",
      name
    ),
    "ERROR_SHARING_VIOLATION",
  );
  t.err(
    &format!(
      "CreateFile {} DELETE FILE_SHARE_WRITE 0 OPEN_EXISTING 0 0",
      name
    ),
    "ERROR_SHARING_VIOLATION",
  );
  t.release(held);
  let held = t.hold(&format!(
    "CreateFile {} GENERIC_READ FILE_SHARE_WRITE 0 CREATE_ALWAYS FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.err(
    &format!(
      "CreateFile {} GENERIC_WRITE FILE_SHARE_WRITE 0 OPEN_EXISTING 0 0",
      name
    ),
    "ERROR_SHARING_VIOLATION",
  );
  t.ok(&format!(
    "CreateFile {} GENERIC_WRITE FILE_SHARE_READ 0 OPEN_EXISTING 0 0",
    name
  ));
  t.err(
    &format!(
      "CreateFile {} DELETE FILE_SHARE_DELETE 0 OPEN_EXISTING 0 0",
      name
    ),
    "ERROR_SHARING_VIOLATION",
  );
  t.err(
    &format!(
      "CreateFile {} DELETE FILE_SHARE_WRITE 0 OPEN_EXISTING 0 0",
      name
    ),
    "ERROR_SHARING_VIOLATION",
  );
  t.release(held);
  let held = t.hold(&format!(
    "CreateFile {} DELETE FILE_SHARE_DELETE 0 CREATE_ALWAYS FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.err(
    &format!(
      "CreateFile {} GENERIC_WRITE FILE_SHARE_DELETE 0 OPEN_EXISTING 0 0",
      name
    ),
    "ERROR_SHARING_VIOLATION",
  );
  t.err(
    &format!(
      "CreateFile {} DELETE FILE_SHARE_WRITE 0 OPEN_EXISTING 0 0",
      name
    ),
    "ERROR_SHARING_VIOLATION",
  );
  t.ok(&format!(
    "CreateFile {} DELETE FILE_SHARE_DELETE 0 OPEN_EXISTING 0 0",
    name
  ));
  t.release(held);
  t.ok(&format!("DeleteFile {}", name));
  t.ok(&format!("CreateDirectory {} 0", name));
  let held = t.hold(&format!(
    "CreateFile {} GENERIC_READ 0 0 OPEN_EXISTING FILE_FLAG_BACKUP_SEMANTICS 0",
    name
  ));
  t.err(
    &format!(
      "CreateFile {} GENERIC_READ 0 0 OPEN_EXISTING FILE_FLAG_BACKUP_SEMANTICS 0",
      name
    ),
    "ERROR_SHARING_VIOLATION",
  );
  t.release(held);
  t.ok(&format!("RemoveDirectory {}", name));
}

/// `t/base/10.t` (sha256 `e9fe95f3c1a13a73e753bf731d8237fe95a1164c2338db1430aaba221d75863d`).
fn base_10(t: &mut Script<'_>) {
  // CreateFile security
  // CreateDirectory security
  // GetFileSecurity
  // SetFileSecurity
  let name = t.name();
  t.ok(&format!(
    "CreateFile {} GENERIC_WRITE 0 D:P(A;;GA;;;WD) CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.check(
    &format!("GetFileSecurity {} DACL_SECURITY_INFORMATION", name),
    "r[0][\"Sddl\"] == \"D:P(A;;FA;;;WD)\"",
    |r| r.first_text("Sddl") == Some("D:P(A;;FA;;;WD)"),
  );
  t.ok(&format!(
    "SetFileSecurity {} DACL_SECURITY_INFORMATION D:P(A;;GA;;;WD)(A;;GR;;;SY)",
    name
  ));
  t.check(
    &format!("GetFileSecurity {} DACL_SECURITY_INFORMATION", name),
    "r[0][\"Sddl\"] == \"D:P(A;;FA;;;WD)(A;;FR;;;SY)\"",
    |r| r.first_text("Sddl") == Some("D:P(A;;FA;;;WD)(A;;FR;;;SY)"),
  );
  t.ok(&format!("DeleteFile {}", name));
  t.ok(&format!(
    "CreateFile {} GENERIC_WRITE 0 D:P(A;;GA;;;WD) CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.ok(&format!(
    "CreateFile {} GENERIC_READ 0 0 OPEN_EXISTING 0 0",
    name
  ));
  t.ok(&format!(
    "SetFileSecurity {} DACL_SECURITY_INFORMATION D:P(D;;GR;;;WD)",
    name
  ));
  t.err(
    &format!("CreateFile {} GENERIC_READ 0 0 OPEN_EXISTING 0 0", name),
    "ERROR_ACCESS_DENIED",
  );
  t.ok(&format!("DeleteFile {}", name));
  t.ok(&format!("CreateDirectory {} D:P(A;;GA;;;WD)", name));
  t.check(
    &format!("GetFileSecurity {} DACL_SECURITY_INFORMATION", name),
    "r[0][\"Sddl\"] == \"D:P(A;;FA;;;WD)\"",
    |r| r.first_text("Sddl") == Some("D:P(A;;FA;;;WD)"),
  );
  t.ok(&format!(
    "SetFileSecurity {} DACL_SECURITY_INFORMATION D:P(A;;GA;;;WD)(A;;GR;;;SY)",
    name
  ));
  t.check(
    &format!("GetFileSecurity {} DACL_SECURITY_INFORMATION", name),
    "r[0][\"Sddl\"] == \"D:P(A;;FA;;;WD)(A;;FR;;;SY)\"",
    |r| r.first_text("Sddl") == Some("D:P(A;;FA;;;WD)(A;;FR;;;SY)"),
  );
  t.ok(&format!("RemoveDirectory {}", name));
  t.ok(&format!("CreateDirectory {} D:P(A;;FR;;;WD)", name));
  t.err(
    &format!(
      "CreateFile {}\\foobar GENERIC_READ 0 D:P(A;;FR;;;WD) CREATE_NEW 0 0",
      name
    ),
    "ERROR_ACCESS_DENIED",
  );
  t.ok(&format!(
    "SetFileSecurity {} DACL_SECURITY_INFORMATION D:P(A;;GA;;;WD)",
    name
  ));
  t.ok(&format!(
    "CreateFile {}\\foobar GENERIC_READ 0 D:P(A;;FR;;;WD) CREATE_NEW 0 0",
    name
  ));
  t.ok(&format!(
    "SetFileSecurity {} DACL_SECURITY_INFORMATION D:P(A;;FR;;;WD)",
    name
  ));
  t.err(
    &format!("DeleteFile {}\\foobar", name),
    "ERROR_ACCESS_DENIED",
  );
  t.ok(&format!(
    "SetFileSecurity {} DACL_SECURITY_INFORMATION D:P(A;;GA;;;WD)",
    name
  ));
  t.ok(&format!("DeleteFile {}\\foobar", name));
  t.ok(&format!("RemoveDirectory {}", name));
}

/// `t/reparse/00.t` (sha256 `ec993a043b1e6efc8af282ba0f273628793613daee271b621e4f776c3d39cc4f`).
fn reparse_00(t: &mut Script<'_>) {
  // reparse points (files)
  let name = t.name();
  t.ok(&format!(
    "CreateFile {} GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileAttributes\"] == 0x20",
    |r| r.first_int("FileAttributes") == Some(0x20),
  );
  t.ok(&format!(
    "SetReparsePoint {} 42 {{92A23BD8-99F5-4FD6-807C-C56F3A063C52}} 1A 2B 3C 4D 5F",
    name
  ));
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileAttributes\"] == 0x420",
    |r| r.first_int("FileAttributes") == Some(0x420),
  );
  t.check(&format!("GetReparsePoint {}", name), "r[0][\"ReparseTag\"] == 42 and r[0][\"ReparseDataLength\"] == 5 and r[0][\"ReparseGuid\"] == \"{92A23BD8-99F5-4FD6-807C-C56F3A063C52}\" and r[0][\"ReparseData\"] == \"1A 2B 3C 4D 5F\"", |r| r.first_int("ReparseTag") == Some(42) && r.first_int("ReparseDataLength") == Some(5) && r.first_text("ReparseGuid") == Some("{92A23BD8-99F5-4FD6-807C-C56F3A063C52}") && r.first_text("ReparseData") == Some("1A 2B 3C 4D 5F"));
  t.ok(&format!(
    "SetReparsePoint {} 42 {{92A23BD8-99F5-4FD6-807C-C56F3A063C52}} A1 B2 C3 D4 F5 1A 2B 3C 4D 5F",
    name
  ));
  t.check(&format!("GetReparsePoint {}", name), "r[0][\"ReparseTag\"] == 42 and r[0][\"ReparseDataLength\"] == 10 and r[0][\"ReparseGuid\"] == \"{92A23BD8-99F5-4FD6-807C-C56F3A063C52}\" and r[0][\"ReparseData\"] == \"A1 B2 C3 D4 F5 1A 2B 3C 4D 5F\"", |r| r.first_int("ReparseTag") == Some(42) && r.first_int("ReparseDataLength") == Some(10) && r.first_text("ReparseGuid") == Some("{92A23BD8-99F5-4FD6-807C-C56F3A063C52}") && r.first_text("ReparseData") == Some("A1 B2 C3 D4 F5 1A 2B 3C 4D 5F"));
  t.err(&format!("SetReparsePoint {} 43 {{92A23BD8-99F5-4FD6-807C-C56F3A063C52}} A1 B2 C3 D4 F5 1A 2B 3C 4D 5F", name), "ERROR(4394)");
  t.err(&format!("SetReparsePoint {} 42 {{02A23BD8-99F5-4FD6-807C-C56F3A063C52}} A1 B2 C3 D4 F5 1A 2B 3C 4D 5F", name), "ERROR(4391)");
  t.ok(&format!("DeleteFile {}", name));
  t.ok(&format!(
    "CreateFile {} GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileAttributes\"] == 0x20",
    |r| r.first_int("FileAttributes") == Some(0x20),
  );
  t.ok(&format!(
    "SetReparsePoint {} 42 {{92A23BD8-99F5-4FD6-807C-C56F3A063C52}} 1A 2B 3C 4D 5F",
    name
  ));
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileAttributes\"] == 0x420",
    |r| r.first_int("FileAttributes") == Some(0x420),
  );
  t.err(
    &format!(
      "DeleteReparsePoint {} 43 {{92A23BD8-99F5-4FD6-807C-C56F3A063C52}}",
      name
    ),
    "ERROR(4394)",
  );
  t.err(
    &format!(
      "DeleteReparsePoint {} 42 {{02A23BD8-99F5-4FD6-807C-C56F3A063C52}}",
      name
    ),
    "ERROR(4391)",
  );
  t.ok(&format!(
    "DeleteReparsePoint {} 42 {{92A23BD8-99F5-4FD6-807C-C56F3A063C52}}",
    name
  ));
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileAttributes\"] == 0x20",
    |r| r.first_int("FileAttributes") == Some(0x20),
  );
  t.ok(&format!("DeleteFile {}", name));
  t.ok(&format!(
    "CreateFile {} GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.ok(&format!("WriteFile {} 0 41 42 43 44 45", name));
  t.check(
    &format!("ReadFile {} 0 10", name),
    "r[0][\"Length\"] == 5 and r[0][\"Data\"] == \"41 42 43 44 45\"",
    |r| r.first_int("Length") == Some(5) && r.first_text("Data") == Some("41 42 43 44 45"),
  );
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileAttributes\"] == 0x20",
    |r| r.first_int("FileAttributes") == Some(0x20),
  );
  t.ok(&format!(
    "SetReparsePoint {} 42 {{92A23BD8-99F5-4FD6-807C-C56F3A063C52}} 1A 2B 3C 4D 5F",
    name
  ));
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileAttributes\"] == 0x420",
    |r| r.first_int("FileAttributes") == Some(0x420),
  );
  t.ok(&format!("WriteFile {} 0 61 62 63", name));
  t.check(
    &format!("ReadFile {} 0 10", name),
    "r[0][\"Length\"] == 5 and r[0][\"Data\"] == \"61 62 63 44 45\"",
    |r| r.first_int("Length") == Some(5) && r.first_text("Data") == Some("61 62 63 44 45"),
  );
  t.ok(&format!("SetEndOfFile {} 2", name));
  t.check(
    &format!("ReadFile {} 0 10", name),
    "r[0][\"Length\"] == 2 and r[0][\"Data\"] == \"61 62\"",
    |r| r.first_int("Length") == Some(2) && r.first_text("Data") == Some("61 62"),
  );
  t.ok(&format!("SetEndOfFile {} 5", name));
  t.check(
    &format!("ReadFile {} 0 10", name),
    "r[0][\"Length\"] == 5 and r[0][\"Data\"] == \"61 62 00 00 00\"",
    |r| r.first_int("Length") == Some(5) && r.first_text("Data") == Some("61 62 00 00 00"),
  );
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileAttributes\"] == 0x420",
    |r| r.first_int("FileAttributes") == Some(0x420),
  );
  t.check(&format!("GetReparsePoint {}", name), "r[0][\"ReparseTag\"] == 42 and r[0][\"ReparseDataLength\"] == 5 and r[0][\"ReparseGuid\"] == \"{92A23BD8-99F5-4FD6-807C-C56F3A063C52}\" and r[0][\"ReparseData\"] == \"1A 2B 3C 4D 5F\"", |r| r.first_int("ReparseTag") == Some(42) && r.first_int("ReparseDataLength") == Some(5) && r.first_text("ReparseGuid") == Some("{92A23BD8-99F5-4FD6-807C-C56F3A063C52}") && r.first_text("ReparseData") == Some("1A 2B 3C 4D 5F"));
  t.ok(&format!("DeleteFile {}", name));
  t.ok(&format!(
    "CreateFile {} GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.ok(&format!(
    "SetReparsePoint {} 42 {{92A23BD8-99F5-4FD6-807C-C56F3A063C52}} 1A 2B 3C 4D 5F",
    name
  ));
  t.err(
    &format!(
      "CreateFile {} GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
      name
    ),
    "ERROR_CANT_ACCESS_FILE",
  );
  t.err(&format!("CreateFile {} GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL+FILE_FLAG_OPEN_REPARSE_POINT 0", name), "ERROR_FILE_EXISTS");
  t.ok(&format!("DeleteFile {}", name));
}

/// `t/reparse/01.t` (sha256 `33bc9c9160d188aa3329dd71a97425c6316f153d447952150d0e60568f8efd47`).
fn reparse_01(t: &mut Script<'_>) {
  // reparse points (directories)
  let name = t.name();
  t.ok(&format!("CreateDirectory {} 0", name));
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileAttributes\"] == 0x10",
    |r| r.first_int("FileAttributes") == Some(0x10),
  );
  t.ok(&format!(
    "SetReparsePoint {} 42 {{92A23BD8-99F5-4FD6-807C-C56F3A063C52}} 1A 2B 3C 4D 5F",
    name
  ));
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileAttributes\"] == 0x410",
    |r| r.first_int("FileAttributes") == Some(0x410),
  );
  t.check(&format!("GetReparsePoint {}", name), "r[0][\"ReparseTag\"] == 42 and r[0][\"ReparseDataLength\"] == 5 and r[0][\"ReparseGuid\"] == \"{92A23BD8-99F5-4FD6-807C-C56F3A063C52}\" and r[0][\"ReparseData\"] == \"1A 2B 3C 4D 5F\"", |r| r.first_int("ReparseTag") == Some(42) && r.first_int("ReparseDataLength") == Some(5) && r.first_text("ReparseGuid") == Some("{92A23BD8-99F5-4FD6-807C-C56F3A063C52}") && r.first_text("ReparseData") == Some("1A 2B 3C 4D 5F"));
  t.ok(&format!(
    "SetReparsePoint {} 42 {{92A23BD8-99F5-4FD6-807C-C56F3A063C52}} A1 B2 C3 D4 F5 1A 2B 3C 4D 5F",
    name
  ));
  t.check(&format!("GetReparsePoint {}", name), "r[0][\"ReparseTag\"] == 42 and r[0][\"ReparseDataLength\"] == 10 and r[0][\"ReparseGuid\"] == \"{92A23BD8-99F5-4FD6-807C-C56F3A063C52}\" and r[0][\"ReparseData\"] == \"A1 B2 C3 D4 F5 1A 2B 3C 4D 5F\"", |r| r.first_int("ReparseTag") == Some(42) && r.first_int("ReparseDataLength") == Some(10) && r.first_text("ReparseGuid") == Some("{92A23BD8-99F5-4FD6-807C-C56F3A063C52}") && r.first_text("ReparseData") == Some("A1 B2 C3 D4 F5 1A 2B 3C 4D 5F"));
  t.err(&format!("SetReparsePoint {} 43 {{92A23BD8-99F5-4FD6-807C-C56F3A063C52}} A1 B2 C3 D4 F5 1A 2B 3C 4D 5F", name), "ERROR(4394)");
  t.err(&format!("SetReparsePoint {} 42 {{02A23BD8-99F5-4FD6-807C-C56F3A063C52}} A1 B2 C3 D4 F5 1A 2B 3C 4D 5F", name), "ERROR(4391)");
  t.ok(&format!("RemoveDirectory {}", name));
  t.ok(&format!("CreateDirectory {} 0", name));
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileAttributes\"] == 0x10",
    |r| r.first_int("FileAttributes") == Some(0x10),
  );
  t.ok(&format!(
    "SetReparsePoint {} 42 {{92A23BD8-99F5-4FD6-807C-C56F3A063C52}} 1A 2B 3C 4D 5F",
    name
  ));
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileAttributes\"] == 0x410",
    |r| r.first_int("FileAttributes") == Some(0x410),
  );
  t.err(
    &format!(
      "DeleteReparsePoint {} 43 {{92A23BD8-99F5-4FD6-807C-C56F3A063C52}}",
      name
    ),
    "ERROR(4394)",
  );
  t.err(
    &format!(
      "DeleteReparsePoint {} 42 {{02A23BD8-99F5-4FD6-807C-C56F3A063C52}}",
      name
    ),
    "ERROR(4391)",
  );
  t.ok(&format!(
    "DeleteReparsePoint {} 42 {{92A23BD8-99F5-4FD6-807C-C56F3A063C52}}",
    name
  ));
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileAttributes\"] == 0x10",
    |r| r.first_int("FileAttributes") == Some(0x10),
  );
  t.ok(&format!("RemoveDirectory {}", name));
  t.ok(&format!("CreateDirectory {} 0", name));
  t.ok(&format!(
    "CreateFile {}\\1 GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileAttributes\"] == 0x10",
    |r| r.first_int("FileAttributes") == Some(0x10),
  );
  t.err(
    &format!(
      "SetReparsePoint {} 42 {{92A23BD8-99F5-4FD6-807C-C56F3A063C52}} 1A 2B 3C 4D 5F",
      name
    ),
    "ERROR_DIR_NOT_EMPTY",
  );
  t.ok(&format!("DeleteFile {}\\1", name));
  t.ok(&format!(
    "SetReparsePoint {} 42 {{92A23BD8-99F5-4FD6-807C-C56F3A063C52}} 1A 2B 3C 4D 5F",
    name
  ));
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileAttributes\"] == 0x410",
    |r| r.first_int("FileAttributes") == Some(0x410),
  );
  t.check(&format!("GetReparsePoint {}", name), "r[0][\"ReparseTag\"] == 42 and r[0][\"ReparseDataLength\"] == 5 and r[0][\"ReparseGuid\"] == \"{92A23BD8-99F5-4FD6-807C-C56F3A063C52}\" and r[0][\"ReparseData\"] == \"1A 2B 3C 4D 5F\"", |r| r.first_int("ReparseTag") == Some(42) && r.first_int("ReparseDataLength") == Some(5) && r.first_text("ReparseGuid") == Some("{92A23BD8-99F5-4FD6-807C-C56F3A063C52}") && r.first_text("ReparseData") == Some("1A 2B 3C 4D 5F"));
  t.err(
    &format!(
      "CreateFile {}\\2 GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
      name
    ),
    "ERROR_CANT_ACCESS_FILE",
  );
  t.ok(&format!("RemoveDirectory {}", name));
  t.ok(&format!("CreateDirectory {} 0", name));
  t.ok(&format!(
    "SetReparsePoint {} 42 {{92A23BD8-99F5-4FD6-807C-C56F3A063C52}} 1A 2B 3C 4D 5F",
    name
  ));
  t.err(
    &format!("CreateDirectory {} 0", name),
    "ERROR_ALREADY_EXISTS",
  );
  t.err(
    &format!(
      "CreateFile {} GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
      name
    ),
    "ERROR_ACCESS_DENIED",
  );
  t.err(&format!("CreateFile {} GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL+FILE_FLAG_OPEN_REPARSE_POINT 0", name), "ERROR_ACCESS_DENIED");
  t.ok(&format!("RemoveDirectory {}", name));
}

/// `t/reparse/02.t` (sha256 `a7e72c5e4b3be1c2a003168425435b8bb6ccccef1e0c71b2a8fe6914d970c3a5`).
fn reparse_02(t: &mut Script<'_>) {
  // symbolic links (files)
  let srcname = t.name();
  let dstname = t.name();
  let dstpath = t.absolute(&dstname);
  // poor man's cygpath!
  let r = t.ok(&format!("CreateSymbolicLink {} {} 0", srcname, dstname));
  if r.error == "0" {
    t.check(
      &format!("GetFileInformation {}", srcname),
      "r[0][\"FileAttributes\"] == 0x420",
      |r| r.first_int("FileAttributes") == Some(0x420),
    );
    t.check(&format!("GetReparsePoint {}", srcname), "r[0][\"ReparseTag\"] == \"IO_REPARSE_TAG_SYMLINK\" and r[0][\"SubstituteName\"] == dstname and r[0][\"PrintName\"] == dstname and r[0][\"Flags\"] == 1", |r| r.first_text("ReparseTag") == Some("IO_REPARSE_TAG_SYMLINK") && r.first_text("SubstituteName") == Some(dstname.as_str()) && r.first_text("PrintName") == Some(dstname.as_str()) && r.first_int("Flags") == Some(1));
    t.eval(!t.opens(&srcname), "testeval(not safeopen(srcname))");
    t.ok(&format!(
      "CreateFile {} GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
      dstname
    ));
    t.eval(t.opens(&srcname), "testeval(safeopen(srcname))");
    t.ok(&format!("DeleteFile {}", dstname));
    t.ok(&format!(
      "CreateFile {} GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
      srcname
    ));
    t.eval(t.opens(&srcname), "testeval(safeopen(srcname))");
    t.eval(t.opens(&dstname), "testeval(safeopen(dstname))");
    t.ok(&format!("DeleteFile {}", dstname));
    t.eval(!t.opens(&srcname), "testeval(not safeopen(srcname))");
    t.ok(&format!("DeleteFile {}", srcname));
    t.ok(&format!("CreateSymbolicLink {} {} 0", srcname, dstname));
    t.ok(&format!("WriteFile {} 0 41 42 43 44 45", srcname));
    t.check(
      &format!("ReadFile {} 0 10", srcname),
      "r[0][\"Length\"] == 5 and r[0][\"Data\"] == \"41 42 43 44 45\"",
      |r| r.first_int("Length") == Some(5) && r.first_text("Data") == Some("41 42 43 44 45"),
    );
    t.check(
      &format!("GetFileInformation {}", srcname),
      "r[0][\"FileAttributes\"] == 0x420",
      |r| r.first_int("FileAttributes") == Some(0x420),
    );
    t.check(&format!("GetReparsePoint {}", srcname), "r[0][\"ReparseTag\"] == \"IO_REPARSE_TAG_SYMLINK\" and r[0][\"SubstituteName\"] == dstname and r[0][\"PrintName\"] == dstname and r[0][\"Flags\"] == 1", |r| r.first_text("ReparseTag") == Some("IO_REPARSE_TAG_SYMLINK") && r.first_text("SubstituteName") == Some(dstname.as_str()) && r.first_text("PrintName") == Some(dstname.as_str()) && r.first_int("Flags") == Some(1));
    t.eval(!t.opens(&srcname), "testeval(not safeopen(srcname))");
    t.ok(&format!(
      "CreateFile {} GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
      dstname
    ));
    t.eval(t.opens(&srcname), "testeval(safeopen(srcname))");
    t.ok(&format!("DeleteFile {}", dstname));
    t.ok(&format!(
      "CreateFile {} GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
      srcname
    ));
    t.eval(t.opens(&srcname), "testeval(safeopen(srcname))");
    t.eval(t.opens(&dstname), "testeval(safeopen(dstname))");
    t.ok(&format!("DeleteFile {}", dstname));
    t.eval(!t.opens(&srcname), "testeval(not safeopen(srcname))");
    t.ok(&format!("DeleteFile {}", srcname));
    // absolute symlink
    t.ok(&format!("CreateSymbolicLink {} {} 0", srcname, dstpath));
    t.check(
      &format!("GetFileInformation {}", srcname),
      "r[0][\"FileAttributes\"] == 0x420",
      |r| r.first_int("FileAttributes") == Some(0x420),
    );
    t.check(&format!("GetReparsePoint {}", srcname), "r[0][\"ReparseTag\"] == \"IO_REPARSE_TAG_SYMLINK\" and r[0][\"SubstituteName\"] == \"\\\\??\\\\\" + (dstpath if \"\\\\\" != dstpath[:1] else \"UNC\" + dstpath[1:]) and r[0][\"PrintName\"] == (dstpath if \"\\\\\" != dstpath[:1] else dstpath[1:]) and r[0][\"Flags\"] == 0", |r| r.first_text("ReparseTag") == Some("IO_REPARSE_TAG_SYMLINK") && r.first_text("SubstituteName") == Some(nt_substitute_name(&dstpath).as_str()) && r.first_text("PrintName") == Some(print_name(&dstpath)) && r.first_int("Flags") == Some(0));
    t.eval(!t.opens(&srcname), "testeval(not safeopen(srcname))");
    t.ok(&format!(
      "CreateFile {} GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
      dstname
    ));
    t.eval(t.opens(&srcname), "testeval(safeopen(srcname))");
    t.ok(&format!("DeleteFile {}", dstname));
    t.ok(&format!(
      "CreateFile {} GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
      srcname
    ));
    t.eval(t.opens(&srcname), "testeval(safeopen(srcname))");
    t.eval(t.opens(&dstname), "testeval(safeopen(dstname))");
    t.ok(&format!("DeleteFile {}", dstname));
    t.eval(!t.opens(&srcname), "testeval(not safeopen(srcname))");
    t.ok(&format!("DeleteFile {}", srcname));
  }
}

/// `t/reparse/03.t` (sha256 `2a0b63a4f052f6bda94598e7f2b6b06cfc73dff6d3d18b8b6505b917b7e1ab53`).
fn reparse_03(t: &mut Script<'_>) {
  // symbolic links (files)
  let srcname = t.name();
  let dstname = t.name();
  let dstpath = t.absolute(&dstname);
  // poor man's cygpath!
  let r = t.ok(&format!(
    "CreateSymbolicLink {} {} SYMBOLIC_LINK_FLAG_DIRECTORY",
    srcname, dstname
  ));
  if r.error == "0" {
    t.check(
      &format!("GetFileInformation {}", srcname),
      "r[0][\"FileAttributes\"] == 0x410",
      |r| r.first_int("FileAttributes") == Some(0x410),
    );
    t.check(&format!("GetReparsePoint {}", srcname), "r[0][\"ReparseTag\"] == \"IO_REPARSE_TAG_SYMLINK\" and r[0][\"SubstituteName\"] == dstname and r[0][\"PrintName\"] == dstname and r[0][\"Flags\"] == 1", |r| r.first_text("ReparseTag") == Some("IO_REPARSE_TAG_SYMLINK") && r.first_text("SubstituteName") == Some(dstname.as_str()) && r.first_text("PrintName") == Some(dstname.as_str()) && r.first_int("Flags") == Some(1));
    t.eval(
      !t.opens(&format!("{}\\1", srcname)),
      "testeval(not safeopen(os.path.join(srcname, \"1\")))",
    );
    t.ok(&format!("CreateDirectory {} 0", dstname));
    t.ok(&format!(
      "CreateFile {}\\1 GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
      dstname
    ));
    t.eval(
      t.opens(&format!("{}\\1", srcname)),
      "testeval(safeopen(os.path.join(srcname, \"1\")))",
    );
    t.ok(&format!("DeleteFile {}\\1", dstname));
    t.ok(&format!("RemoveDirectory {}", dstname));
    t.eval(
      !t.opens(&format!("{}\\1", srcname)),
      "testeval(not safeopen(os.path.join(srcname, \"1\")))",
    );
    t.err(
      &format!("CreateDirectory {} 0", srcname),
      "ERROR_ALREADY_EXISTS",
    );
    t.err(
      &format!(
        "CreateFile {} GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
        srcname
      ),
      "ERROR_ACCESS_DENIED",
    );
    t.ok(&format!("RemoveDirectory {}", srcname));
    // absolute symlink
    t.ok(&format!(
      "CreateSymbolicLink {} {} SYMBOLIC_LINK_FLAG_DIRECTORY",
      srcname, dstpath
    ));
    t.check(
      &format!("GetFileInformation {}", srcname),
      "r[0][\"FileAttributes\"] == 0x410",
      |r| r.first_int("FileAttributes") == Some(0x410),
    );
    t.check(&format!("GetReparsePoint {}", srcname), "r[0][\"ReparseTag\"] == \"IO_REPARSE_TAG_SYMLINK\" and r[0][\"SubstituteName\"] == \"\\\\??\\\\\" + (dstpath if \"\\\\\" != dstpath[:1] else \"UNC\" + dstpath[1:]) and r[0][\"PrintName\"] == (dstpath if \"\\\\\" != dstpath[:1] else dstpath[1:]) and r[0][\"Flags\"] == 0", |r| r.first_text("ReparseTag") == Some("IO_REPARSE_TAG_SYMLINK") && r.first_text("SubstituteName") == Some(nt_substitute_name(&dstpath).as_str()) && r.first_text("PrintName") == Some(print_name(&dstpath)) && r.first_int("Flags") == Some(0));
    t.eval(
      !t.opens(&format!("{}\\1", srcname)),
      "testeval(not safeopen(os.path.join(srcname, \"1\")))",
    );
    t.ok(&format!("CreateDirectory {} 0", dstname));
    t.ok(&format!(
      "CreateFile {}\\1 GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
      dstname
    ));
    t.eval(
      t.opens(&format!("{}\\1", srcname)),
      "testeval(safeopen(os.path.join(srcname, \"1\")))",
    );
    t.ok(&format!("DeleteFile {}\\1", dstname));
    t.ok(&format!("RemoveDirectory {}", dstname));
    t.eval(
      !t.opens(&format!("{}\\1", srcname)),
      "testeval(not safeopen(os.path.join(srcname, \"1\")))",
    );
    t.err(
      &format!("CreateDirectory {} 0", srcname),
      "ERROR_ALREADY_EXISTS",
    );
    t.err(
      &format!(
        "CreateFile {} GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
        srcname
      ),
      "ERROR_ACCESS_DENIED",
    );
    t.ok(&format!("RemoveDirectory {}", srcname));
  }
}

/// `t/streams/00.t` (sha256 `2de71f510cda49d802df83b715540243f027e4230d82efb3fe78e6b5ee9facda`).
fn streams_00(t: &mut Script<'_>) {
  // CreateFile (file stream)
  // DeleteFile (file stream)
  let name = t.name();
  t.ok(&format!(
    "CreateFile {}:foo GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.err(
    &format!(
      "CreateFile {} GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
      name
    ),
    "ERROR_FILE_EXISTS",
  );
  t.err(
    &format!(
      "CreateFile {}:foo GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
      name
    ),
    "ERROR_FILE_EXISTS",
  );
  let r = t.ok(&format!("GetFileInformation {}", name));
  let s = t.ok(&format!("GetFileInformation {}:foo", name));
  t.eval(
    r.first_int("FileIndex").is_some() && r.first_int("FileIndex") == s.first_int("FileIndex"),
    "testeval(r[0][\"FileIndex\"] == s[0][\"FileIndex\"])",
  );
  t.ok(&format!("DeleteFile {}", name));
  t.err(&format!("DeleteFile {}:foo", name), "ERROR_FILE_NOT_FOUND");
  t.err(&format!("DeleteFile {}", name), "ERROR_FILE_NOT_FOUND");
  t.ok(&format!(
    "CreateFile {} GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.err(
    &format!(
      "CreateFile {} GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
      name
    ),
    "ERROR_FILE_EXISTS",
  );
  t.ok(&format!(
    "CreateFile {}:foo GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.err(
    &format!(
      "CreateFile {}:foo GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
      name
    ),
    "ERROR_FILE_EXISTS",
  );
  let r = t.ok(&format!("GetFileInformation {}", name));
  let s = t.ok(&format!("GetFileInformation {}:foo", name));
  t.eval(
    r.first_int("FileIndex").is_some() && r.first_int("FileIndex") == s.first_int("FileIndex"),
    "testeval(r[0][\"FileIndex\"] == s[0][\"FileIndex\"])",
  );
  t.ok(&format!("DeleteFile {}:foo", name));
  t.err(&format!("DeleteFile {}:foo", name), "ERROR_FILE_NOT_FOUND");
  t.ok(&format!("DeleteFile {}", name));
  t.err(&format!("DeleteFile {}", name), "ERROR_FILE_NOT_FOUND");
  t.ok(&format!(
    "CreateFile {}:foo:$DATA GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  let r = t.ok(&format!("GetFileInformation {}", name));
  let s = t.ok(&format!("GetFileInformation {}:foo", name));
  t.eval(
    r.first_int("FileIndex").is_some() && r.first_int("FileIndex") == s.first_int("FileIndex"),
    "testeval(r[0][\"FileIndex\"] == s[0][\"FileIndex\"])",
  );
  t.ok(&format!("DeleteFile {}", name));
  t.ok(&format!(
    "CreateFile {}:foo:$DATA GENERIC_WRITE 0 0 CREATE_ALWAYS FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.err(
    &format!(
      "-e CreateFile {}:foo:$DATA GENERIC_WRITE 0 0 CREATE_ALWAYS FILE_ATTRIBUTE_NORMAL 0",
      name
    ),
    "ERROR_ALREADY_EXISTS",
  );
  let r = t.ok(&format!("GetFileInformation {}", name));
  let s = t.ok(&format!("GetFileInformation {}:foo", name));
  t.eval(
    r.first_int("FileIndex").is_some() && r.first_int("FileIndex") == s.first_int("FileIndex"),
    "testeval(r[0][\"FileIndex\"] == s[0][\"FileIndex\"])",
  );
  t.ok(&format!("DeleteFile {}", name));
}

/// `t/streams/01.t` (sha256 `fc70867d59b505974ffb93d398aee394134881fbd0e911ce24d6820ce50d5d7f`).
fn streams_01(t: &mut Script<'_>) {
  // CreateFile (directory stream)
  // DeleteFile (directory stream)
  let name = t.name();
  t.err(
    &format!("CreateDirectory {}:foo 0", name),
    "ERROR_DIRECTORY",
  );
  t.ok(&format!("CreateDirectory {} 0", name));
  t.err(
    &format!("CreateDirectory {} 0", name),
    "ERROR_ALREADY_EXISTS",
  );
  t.ok(&format!(
    "CreateFile {}:foo GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.err(
    &format!(
      "CreateFile {}:foo GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
      name
    ),
    "ERROR_FILE_EXISTS",
  );
  let r = t.ok(&format!("GetFileInformation {}", name));
  let s = t.ok(&format!("GetFileInformation {}:foo", name));
  t.eval(
    r.first_int("FileIndex").is_some() && r.first_int("FileIndex") == s.first_int("FileIndex"),
    "testeval(r[0][\"FileIndex\"] == s[0][\"FileIndex\"])",
  );
  t.err(&format!("RemoveDirectory {}:foo", name), "ERROR_DIRECTORY");
  t.ok(&format!("DeleteFile {}:foo", name));
  t.err(&format!("DeleteFile {}:foo", name), "ERROR_FILE_NOT_FOUND");
  t.ok(&format!("RemoveDirectory {}", name));
  t.err(&format!("RemoveDirectory {}", name), "ERROR_FILE_NOT_FOUND");
  t.ok(&format!("CreateDirectory {} 0", name));
  t.ok(&format!(
    "CreateFile {}:foo:$DATA GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  let r = t.ok(&format!("GetFileInformation {}", name));
  let s = t.ok(&format!("GetFileInformation {}:foo", name));
  t.eval(
    r.first_int("FileIndex").is_some() && r.first_int("FileIndex") == s.first_int("FileIndex"),
    "testeval(r[0][\"FileIndex\"] == s[0][\"FileIndex\"])",
  );
  t.ok(&format!("RemoveDirectory {}", name));
}

/// `t/streams/02.t` (sha256 `b3a141c2401a20f30dd552e1b32792dd4a6803c9248b29f2244337f92c5382f4`).
fn streams_02(t: &mut Script<'_>) {
  // FindStreams (FindFirstStreamW, FindNextStreamW, FindClose)
  let name = t.name();
  t.ok(&format!(
    "CreateFile {} GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  let r = t.ok(&format!("FindStreams {}", name));
  let s = r.text_set("StreamName");
  t.eval(s.len() == 1, "testeval(len(s) == 1)");
  t.eval(s.contains("::$DATA"), "testeval(\"::$DATA\" in s)");
  t.ok(&format!("DeleteFile {}", name));
  t.err(&format!("FindStreams {}", name), "ERROR_FILE_NOT_FOUND");
  t.ok(&format!(
    "CreateFile {}:foo GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.ok(&format!(
    "CreateFile {}:bar GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  let r = t.ok(&format!("FindStreams {}", name));
  let s = r.text_set("StreamName");
  t.eval(s.len() == 3, "testeval(len(s) == 3)");
  t.eval(s.contains("::$DATA"), "testeval(\"::$DATA\" in s)");
  t.eval(s.contains(":foo:$DATA"), "testeval(\":foo:$DATA\" in s)");
  t.eval(s.contains(":bar:$DATA"), "testeval(\":bar:$DATA\" in s)");
  t.ok(&format!("DeleteFile {}", name));
  t.err(&format!("FindStreams {}", name), "ERROR_FILE_NOT_FOUND");
  t.ok(&format!(
    "CreateFile {}:foo:$DATA GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.ok(&format!(
    "CreateFile {}:bar:$DATA GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  let r = t.ok(&format!("FindStreams {}", name));
  let s = r.text_set("StreamName");
  t.eval(s.len() == 3, "testeval(len(s) == 3)");
  t.eval(s.contains("::$DATA"), "testeval(\"::$DATA\" in s)");
  t.eval(s.contains(":foo:$DATA"), "testeval(\":foo:$DATA\" in s)");
  t.eval(s.contains(":bar:$DATA"), "testeval(\":bar:$DATA\" in s)");
  t.ok(&format!("DeleteFile {}", name));
  t.err(&format!("FindStreams {}", name), "ERROR_FILE_NOT_FOUND");
  t.ok(&format!("CreateDirectory {} 0", name));
  t.err(&format!("FindStreams {}", name), "ERROR_HANDLE_EOF");
  t.ok(&format!("RemoveDirectory {}", name));
  t.err(&format!("FindStreams {}", name), "ERROR_FILE_NOT_FOUND");
  t.ok(&format!("CreateDirectory {} 0", name));
  t.ok(&format!(
    "CreateFile {}:foo GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.ok(&format!(
    "CreateFile {}:bar GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  let r = t.ok(&format!("FindStreams {}", name));
  let s = r.text_set("StreamName");
  t.eval(s.len() == 2, "testeval(len(s) == 2)");
  t.eval(s.contains(":foo:$DATA"), "testeval(\":foo:$DATA\" in s)");
  t.eval(s.contains(":bar:$DATA"), "testeval(\":bar:$DATA\" in s)");
  t.ok(&format!("RemoveDirectory {}", name));
  t.err(&format!("FindStreams {}", name), "ERROR_FILE_NOT_FOUND");
  t.ok(&format!("CreateDirectory {} 0", name));
  t.ok(&format!(
    "CreateFile {}:foo:$DATA GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.ok(&format!(
    "CreateFile {}:bar:$DATA GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  let r = t.ok(&format!("FindStreams {}", name));
  let s = r.text_set("StreamName");
  t.eval(s.len() == 2, "testeval(len(s) == 2)");
  t.eval(s.contains(":foo:$DATA"), "testeval(\":foo:$DATA\" in s)");
  t.eval(s.contains(":bar:$DATA"), "testeval(\":bar:$DATA\" in s)");
  t.ok(&format!("RemoveDirectory {}", name));
  t.err(&format!("FindStreams {}", name), "ERROR_FILE_NOT_FOUND");
  for i in 0..100 {
    t.ok(&format!(
      "CreateFile {}:strm{} GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
      name, i
    ));
  }
  let r = t.ok(&format!("FindStreams {}", name));
  let s = r.text_set("StreamName");
  t.eval(s.len() == 101, "testeval(len(s) == 101)");
  for i in 0..100 {
    t.eval(
      s.contains(&format!(":strm{}:$DATA", i)),
      "testeval(\":strm%s:$DATA\" % i in s)",
    );
  }
  t.ok(&format!("DeleteFile {}", name));
  t.err(&format!("FindStreams {}", name), "ERROR_FILE_NOT_FOUND");
}

/// `t/streams/03.t` (sha256 `d36ceb03cb4d85876c65fe78d8950a76da2e516f12223b9646556363fb4fe04b`).
fn streams_03(t: &mut Script<'_>) {
  // CreateFile (file stream) security
  // CreateDirectory (file stream) security
  // GetFileSecurity (file stream)
  // SetFileSecurity (file stream)
  let name = t.name();
  t.ok(&format!(
    "CreateFile {}:strm GENERIC_WRITE 0 D:P(A;;GA;;;WD) CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.check(
    &format!("GetFileSecurity {} DACL_SECURITY_INFORMATION", name),
    "r[0][\"Sddl\"] == \"D:P(A;;FA;;;WD)\"",
    |r| r.first_text("Sddl") == Some("D:P(A;;FA;;;WD)"),
  );
  t.check(
    &format!("GetFileSecurity {}:strm DACL_SECURITY_INFORMATION", name),
    "r[0][\"Sddl\"] == \"D:P(A;;FA;;;WD)\"",
    |r| r.first_text("Sddl") == Some("D:P(A;;FA;;;WD)"),
  );
  t.ok(&format!(
    "SetFileSecurity {}:strm DACL_SECURITY_INFORMATION D:P(A;;GA;;;WD)(A;;GR;;;SY)",
    name
  ));
  t.check(
    &format!("GetFileSecurity {} DACL_SECURITY_INFORMATION", name),
    "r[0][\"Sddl\"] == \"D:P(A;;FA;;;WD)(A;;FR;;;SY)\"",
    |r| r.first_text("Sddl") == Some("D:P(A;;FA;;;WD)(A;;FR;;;SY)"),
  );
  t.check(
    &format!("GetFileSecurity {}:strm DACL_SECURITY_INFORMATION", name),
    "r[0][\"Sddl\"] == \"D:P(A;;FA;;;WD)(A;;FR;;;SY)\"",
    |r| r.first_text("Sddl") == Some("D:P(A;;FA;;;WD)(A;;FR;;;SY)"),
  );
  t.ok(&format!("DeleteFile {}", name));
  t.ok(&format!(
    "CreateFile {}:strm GENERIC_WRITE 0 D:P(A;;GA;;;WD) CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.ok(&format!(
    "CreateFile {} GENERIC_READ 0 0 OPEN_EXISTING 0 0",
    name
  ));
  t.ok(&format!(
    "CreateFile {}:strm GENERIC_READ 0 0 OPEN_EXISTING 0 0",
    name
  ));
  t.ok(&format!(
    "SetFileSecurity {} DACL_SECURITY_INFORMATION D:P(D;;GR;;;WD)",
    name
  ));
  t.err(
    &format!("CreateFile {} GENERIC_READ 0 0 OPEN_EXISTING 0 0", name),
    "ERROR_ACCESS_DENIED",
  );
  t.err(
    &format!(
      "CreateFile {}:strm GENERIC_READ 0 0 OPEN_EXISTING 0 0",
      name
    ),
    "ERROR_ACCESS_DENIED",
  );
  t.ok(&format!("DeleteFile {}", name));
  t.ok(&format!("CreateDirectory {} D:P(A;;GA;;;WD)", name));
  t.ok(&format!(
    "CreateFile {}:strm GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.check(
    &format!("GetFileSecurity {} DACL_SECURITY_INFORMATION", name),
    "r[0][\"Sddl\"] == \"D:P(A;;FA;;;WD)\"",
    |r| r.first_text("Sddl") == Some("D:P(A;;FA;;;WD)"),
  );
  t.check(
    &format!("GetFileSecurity {}:strm DACL_SECURITY_INFORMATION", name),
    "r[0][\"Sddl\"] == \"D:P(A;;FA;;;WD)\"",
    |r| r.first_text("Sddl") == Some("D:P(A;;FA;;;WD)"),
  );
  t.ok(&format!(
    "SetFileSecurity {}:strm DACL_SECURITY_INFORMATION D:P(A;;GA;;;WD)(A;;GR;;;SY)",
    name
  ));
  t.check(
    &format!("GetFileSecurity {} DACL_SECURITY_INFORMATION", name),
    "r[0][\"Sddl\"] == \"D:P(A;;FA;;;WD)(A;;FR;;;SY)\"",
    |r| r.first_text("Sddl") == Some("D:P(A;;FA;;;WD)(A;;FR;;;SY)"),
  );
  t.check(
    &format!("GetFileSecurity {}:strm DACL_SECURITY_INFORMATION", name),
    "r[0][\"Sddl\"] == \"D:P(A;;FA;;;WD)(A;;FR;;;SY)\"",
    |r| r.first_text("Sddl") == Some("D:P(A;;FA;;;WD)(A;;FR;;;SY)"),
  );
  t.ok(&format!("RemoveDirectory {}", name));
  t.ok(&format!("CreateDirectory {} D:P(A;;GA;;;WD)", name));
  t.ok(&format!(
    "CreateFile {}\\foobar GENERIC_READ 0 D:P(A;;FR;;;WD) CREATE_NEW 0 0",
    name
  ));
  t.err(
    &format!(
      "CreateFile {}\\foobar:strm GENERIC_READ 0 0 CREATE_NEW 0 0",
      name
    ),
    "ERROR_ACCESS_DENIED",
  );
  t.ok(&format!(
    "SetFileSecurity {}\\foobar DACL_SECURITY_INFORMATION D:P(A;;FRFW;;;WD)",
    name
  ));
  t.ok(&format!(
    "CreateFile {}\\foobar:strm GENERIC_READ 0 0 CREATE_NEW 0 0",
    name
  ));
  t.ok(&format!(
    "SetFileSecurity {} DACL_SECURITY_INFORMATION D:P(A;;FR;;;WD)",
    name
  ));
  t.ok(&format!(
    "SetFileSecurity {}\\foobar DACL_SECURITY_INFORMATION D:P(A;;FR;;;WD)",
    name
  ));
  t.err(
    &format!("DeleteFile {}\\foobar:strm", name),
    "ERROR_ACCESS_DENIED",
  );
  t.ok(&format!(
    "SetFileSecurity {}\\foobar DACL_SECURITY_INFORMATION D:P(A;;FRSD;;;WD)",
    name
  ));
  t.ok(&format!("DeleteFile {}\\foobar:strm", name));
  t.ok(&format!(
    "SetFileSecurity {}\\foobar DACL_SECURITY_INFORMATION D:P(A;;FRFW;;;WD)",
    name
  ));
  t.ok(&format!(
    "CreateFile {}\\foobar:strm GENERIC_READ 0 0 CREATE_NEW 0 0",
    name
  ));
  t.err(
    &format!("DeleteFile {}\\foobar:strm", name),
    "ERROR_ACCESS_DENIED",
  );
  t.ok(&format!(
    "SetFileSecurity {} DACL_SECURITY_INFORMATION D:P(A;;GA;;;WD)",
    name
  ));
  t.ok(&format!("DeleteFile {}\\foobar:strm", name));
  t.ok(&format!("DeleteFile {}\\foobar", name));
  t.ok(&format!("RemoveDirectory {}", name));
}

/// `t/streams/04.t` (sha256 `feaf88b595ee5f7e409b2c12ee340eec629a2c52aba6278c72276c4ccda6729f`).
fn streams_04(t: &mut Script<'_>) {
  // reparse points (streams)
  let name = t.name();
  let srcname = t.name();
  let dstname = t.name();
  t.ok(&format!(
    "CreateFile {}:strm GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
    name
  ));
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileAttributes\"] == 0x20",
    |r| r.first_int("FileAttributes") == Some(0x20),
  );
  t.ok(&format!(
    "SetReparsePoint {}:strm 42 {{92A23BD8-99F5-4FD6-807C-C56F3A063C52}} 1A 2B 3C 4D 5F",
    name
  ));
  t.check(
    &format!("GetFileInformation {}", name),
    "r[0][\"FileAttributes\"] == 0x420",
    |r| r.first_int("FileAttributes") == Some(0x420),
  );
  t.check(&format!("GetReparsePoint {}", name), "r[0][\"ReparseTag\"] == 42 and r[0][\"ReparseDataLength\"] == 5 and r[0][\"ReparseGuid\"] == \"{92A23BD8-99F5-4FD6-807C-C56F3A063C52}\" and r[0][\"ReparseData\"] == \"1A 2B 3C 4D 5F\"", |r| r.first_int("ReparseTag") == Some(42) && r.first_int("ReparseDataLength") == Some(5) && r.first_text("ReparseGuid") == Some("{92A23BD8-99F5-4FD6-807C-C56F3A063C52}") && r.first_text("ReparseData") == Some("1A 2B 3C 4D 5F"));
  t.ok(&format!(
    "SetReparsePoint {} 42 {{92A23BD8-99F5-4FD6-807C-C56F3A063C52}} A1 B2 C3 D4 F5 1A 2B 3C 4D 5F",
    name
  ));
  t.check(&format!("GetReparsePoint {}:strm", name), "r[0][\"ReparseTag\"] == 42 and r[0][\"ReparseDataLength\"] == 10 and r[0][\"ReparseGuid\"] == \"{92A23BD8-99F5-4FD6-807C-C56F3A063C52}\" and r[0][\"ReparseData\"] == \"A1 B2 C3 D4 F5 1A 2B 3C 4D 5F\"", |r| r.first_int("ReparseTag") == Some(42) && r.first_int("ReparseDataLength") == Some(10) && r.first_text("ReparseGuid") == Some("{92A23BD8-99F5-4FD6-807C-C56F3A063C52}") && r.first_text("ReparseData") == Some("A1 B2 C3 D4 F5 1A 2B 3C 4D 5F"));
  t.ok(&format!("DeleteFile {}", name));
  let r = t.ok(&format!("CreateSymbolicLink {} {} 0", srcname, dstname));
  if r.error == "0" {
    t.ok(&format!(
      "CreateFile {}:strm GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
      srcname
    ));
    t.ok(&format!(
      "CreateFile {}:strm GENERIC_WRITE 0 0 OPEN_EXISTING FILE_ATTRIBUTE_NORMAL 0",
      srcname
    ));
    t.ok(&format!(
      "CreateFile {}:strm GENERIC_WRITE 0 0 OPEN_EXISTING FILE_ATTRIBUTE_NORMAL 0",
      dstname
    ));
    t.ok(&format!("CreateFile {}:symstrm GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL+FILE_FLAG_OPEN_REPARSE_POINT 0", srcname));
    t.ok(&format!("CreateFile {}:symstrm GENERIC_WRITE 0 0 OPEN_EXISTING FILE_ATTRIBUTE_NORMAL+FILE_FLAG_OPEN_REPARSE_POINT 0", srcname));
    t.err(
      &format!(
        "CreateFile {}:symstrm GENERIC_WRITE 0 0 OPEN_EXISTING FILE_ATTRIBUTE_NORMAL 0",
        srcname
      ),
      "ERROR_FILE_NOT_FOUND",
    );
    t.err(
      &format!(
        "CreateFile {}:symstrm GENERIC_WRITE 0 0 OPEN_EXISTING FILE_ATTRIBUTE_NORMAL 0",
        dstname
      ),
      "ERROR_FILE_NOT_FOUND",
    );
    t.ok(&format!("DeleteFile {}", dstname));
    t.ok(&format!("DeleteFile {}", srcname));
    t.ok(&format!(
      "CreateSymbolicLink {}:strm {} 0",
      srcname, dstname
    ));
    t.check(
      &format!("GetFileInformation {}", srcname),
      "r[0][\"FileAttributes\"] == 0x420",
      |r| r.first_int("FileAttributes") == Some(0x420),
    );
    t.check(&format!("GetReparsePoint {}", srcname), "r[0][\"ReparseTag\"] == \"IO_REPARSE_TAG_SYMLINK\" and r[0][\"SubstituteName\"] == dstname and r[0][\"PrintName\"] == dstname and r[0][\"Flags\"] == 1", |r| r.first_text("ReparseTag") == Some("IO_REPARSE_TAG_SYMLINK") && r.first_text("SubstituteName") == Some(dstname.as_str()) && r.first_text("PrintName") == Some(dstname.as_str()) && r.first_int("Flags") == Some(1));
    let r = t.ok(&format!("FindStreams {}", srcname));
    let s = r.text_set("StreamName");
    t.eval(s.len() == 2, "testeval(len(s) == 2)");
    t.eval(s.contains("::$DATA"), "testeval(\"::$DATA\" in s)");
    t.eval(s.contains(":strm:$DATA"), "testeval(\":strm:$DATA\" in s)");
    t.ok(&format!("DeleteFile {}", srcname));
    t.ok(&format!(
      "CreateSymbolicLink {} {}:strm 0",
      srcname, dstname
    ));
    t.err(
      &format!(
        "CreateFile {}:strm GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
        srcname
      ),
      "ERROR_INVALID_NAME",
    );
    t.ok(&format!(
      "CreateFile {} GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
      srcname
    ));
    t.ok(&format!(
      "CreateFile {} GENERIC_WRITE 0 0 OPEN_EXISTING FILE_ATTRIBUTE_NORMAL 0",
      srcname
    ));
    t.ok(&format!(
      "CreateFile {} GENERIC_WRITE 0 0 OPEN_EXISTING FILE_ATTRIBUTE_NORMAL 0",
      dstname
    ));
    t.ok(&format!(
      "CreateFile {}:strm GENERIC_WRITE 0 0 OPEN_EXISTING FILE_ATTRIBUTE_NORMAL 0",
      dstname
    ));
    t.ok(&format!("DeleteFile {}", dstname));
    t.ok(&format!("DeleteFile {}", srcname));
    t.ok(&format!(
      "CreateSymbolicLink {} {} SYMBOLIC_LINK_FLAG_DIRECTORY",
      srcname, dstname
    ));
    t.err(
      &format!("CreateDirectory {} 0", srcname),
      "ERROR_ALREADY_EXISTS",
    );
    t.err(
      &format!(
        "CreateFile {} GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
        srcname
      ),
      "ERROR_ACCESS_DENIED",
    );
    t.ok(&format!(
      "CreateFile {}:strm GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
      srcname
    ));
    t.ok(&format!(
      "CreateFile {}:strm GENERIC_WRITE 0 0 OPEN_EXISTING FILE_ATTRIBUTE_NORMAL 0",
      srcname
    ));
    t.ok(&format!(
      "CreateFile {}:strm GENERIC_WRITE 0 0 OPEN_EXISTING FILE_ATTRIBUTE_NORMAL 0",
      dstname
    ));
    t.ok(&format!("DeleteFile {}", dstname));
    t.ok(&format!("RemoveDirectory {}", srcname));
    t.ok(&format!("CreateDirectory {} 0", dstname));
    t.ok(&format!(
      "CreateSymbolicLink {} {} SYMBOLIC_LINK_FLAG_DIRECTORY",
      srcname, dstname
    ));
    t.ok(&format!(
      "CreateFile {}:strm GENERIC_WRITE 0 0 CREATE_NEW FILE_ATTRIBUTE_NORMAL 0",
      srcname
    ));
    t.ok(&format!(
      "CreateFile {}:strm GENERIC_WRITE 0 0 OPEN_EXISTING FILE_ATTRIBUTE_NORMAL 0",
      srcname
    ));
    t.ok(&format!(
      "CreateFile {}:strm GENERIC_WRITE 0 0 OPEN_EXISTING FILE_ATTRIBUTE_NORMAL 0",
      dstname
    ));
    t.ok(&format!("RemoveDirectory {}", dstname));
    t.ok(&format!("RemoveDirectory {}", srcname));
  }
}

/// Every script, in upstream order: its path, its digest and its transcription.
pub(crate) const SCRIPTS: &[Transcribed] = &[
  Transcribed {
    path: "t/base/00.t",
    sha256: "80606107049d57fe87037a9264fc33d1a9b009cef80bd35fb1573b9268f53ba4",
    run: base_00,
  },
  Transcribed {
    path: "t/base/01.t",
    sha256: "08a40af704003632a1db28ded4705ea2960057f3ca528ac1c111028bf262ca25",
    run: base_01,
  },
  Transcribed {
    path: "t/base/02.t",
    sha256: "2c8a132c33e57c5e371d4c30d6712ece5325985450982f129b74522a11b81b0b",
    run: base_02,
  },
  Transcribed {
    path: "t/base/03.t",
    sha256: "f75cba61239badc357262fcc98bad483092d31437ae2aca07008182888c934c1",
    run: base_03,
  },
  Transcribed {
    path: "t/base/04.t",
    sha256: "4480fcaab742123b4ce22acf00b2ee214da3198a57c1c32e77ee9f98bafa00c6",
    run: base_04,
  },
  Transcribed {
    path: "t/base/05.t",
    sha256: "6cf2e237456f3e91924125db35b6977b690d80182d4e04c4db3a574473b1cfc0",
    run: base_05,
  },
  Transcribed {
    path: "t/base/06.t",
    sha256: "ae4c60fc3f3bc916fab1507de73e9eaf4590340b55f13dbb61fcce48691e411c",
    run: base_06,
  },
  Transcribed {
    path: "t/base/07.t",
    sha256: "8d96716a8500197a4460bc75b8a1b57423546c050450a7b6225dd89a834b7ee6",
    run: base_07,
  },
  Transcribed {
    path: "t/base/08.t",
    sha256: "44d0bbcc2e3b6c4e9202e0544bfe0f33d4f441fce7ae97361a39e48e0cf80ee6",
    run: base_08,
  },
  Transcribed {
    path: "t/base/09.t",
    sha256: "9add1871777f6645ab3a1865afe3aab22f1ec4240270de5dfa2305295009bf46",
    run: base_09,
  },
  Transcribed {
    path: "t/base/10.t",
    sha256: "e9fe95f3c1a13a73e753bf731d8237fe95a1164c2338db1430aaba221d75863d",
    run: base_10,
  },
  Transcribed {
    path: "t/reparse/00.t",
    sha256: "ec993a043b1e6efc8af282ba0f273628793613daee271b621e4f776c3d39cc4f",
    run: reparse_00,
  },
  Transcribed {
    path: "t/reparse/01.t",
    sha256: "33bc9c9160d188aa3329dd71a97425c6316f153d447952150d0e60568f8efd47",
    run: reparse_01,
  },
  Transcribed {
    path: "t/reparse/02.t",
    sha256: "a7e72c5e4b3be1c2a003168425435b8bb6ccccef1e0c71b2a8fe6914d970c3a5",
    run: reparse_02,
  },
  Transcribed {
    path: "t/reparse/03.t",
    sha256: "2a0b63a4f052f6bda94598e7f2b6b06cfc73dff6d3d18b8b6505b917b7e1ab53",
    run: reparse_03,
  },
  Transcribed {
    path: "t/streams/00.t",
    sha256: "2de71f510cda49d802df83b715540243f027e4230d82efb3fe78e6b5ee9facda",
    run: streams_00,
  },
  Transcribed {
    path: "t/streams/01.t",
    sha256: "fc70867d59b505974ffb93d398aee394134881fbd0e911ce24d6820ce50d5d7f",
    run: streams_01,
  },
  Transcribed {
    path: "t/streams/02.t",
    sha256: "b3a141c2401a20f30dd552e1b32792dd4a6803c9248b29f2244337f92c5382f4",
    run: streams_02,
  },
  Transcribed {
    path: "t/streams/03.t",
    sha256: "d36ceb03cb4d85876c65fe78d8950a76da2e516f12223b9646556363fb4fe04b",
    run: streams_03,
  },
  Transcribed {
    path: "t/streams/04.t",
    sha256: "feaf88b595ee5f7e409b2c12ee340eec629a2c52aba6278c72276c4ccda6729f",
    run: streams_04,
  },
];
