//! The workload roster and its byte-identity comparison (Part 6 "Real workloads": "vendored git,
//! cargo, npm, python, rg, rsync, sqlite, editors, watchers … outputs byte-identical to the host
//! run; `git status` clean; incremental builds reuse artifacts"; AC-3.2, AC-4.2). Each workload is
//! a short POSIX shell script the harness runs twice with the same environment — once in a host
//! directory, once in a directory inside the mount — capturing the exit code, the output and a
//! manifest of the resulting tree. "Byte-identical" is then exactly: equal exit codes, equal
//! outputs after the two directory roots are replaced by one token, and equal manifests (path,
//! kind, mode bits, size, BLAKE3 of the bytes, symlink target) under the reviewed exclusions the
//! roster names per workload with the reason (`EQUIVALENCE.md` §2: timestamps and inode numbers
//! are not compared; the exclusions here are the files that embed a timestamp or an absolute
//! path by the tool's own design).

use serde::{Deserialize, Serialize};

use crate::record::{WorkloadStatus, detail};

/// One workload of the roster.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Workload {
  /// The name (the record's `tools[].name`).
  pub name: &'static str,
  /// The tool the harness probes on the `PATH`; absent means the cell is skipped naming it.
  pub tool: &'static str,
  /// The POSIX shell script, run with `sh -e` inside a fresh directory.
  pub script: &'static str,
  /// Path prefixes excluded from the manifest comparison, each with its reason.
  pub excluded: &'static [(&'static str, &'static str)],
}

/// The git workload: a repository with files, a directory, a symlink, a mode change and two
/// commits; the tree hash proves byte-identical content and modes, `git status` must be clean.
const GIT: &str = r#"
git init -q .
printf 'hello\n' > a.txt
mkdir -p d
printf 'world\n' > d/b.txt
ln -s a.txt link
chmod u=rwx,go=rx d/b.txt
git add -A
git commit -q -m first
printf 'more\n' >> a.txt
git mv d/b.txt d/c.txt
git add -A
git commit -q -m second
git status --porcelain
git log --format='%H %T %P'
git ls-files -s
git fsck --strict --no-progress
git count-objects
"#;

/// The cargo workload: a dependency-free crate built twice; the second build must report the
/// crate `Fresh` (AC-3.2 "incremental builds reuse artifacts") and the binary must run.
const CARGO: &str = r#"
mkdir -p mini/src
printf '[package]\nname = "mini"\nversion = "0.1.0"\nedition = "2024"\n\n[dependencies]\n' > mini/Cargo.toml
printf 'fn main() { println!("mini says hello with {} args", std::env::args().count()); }\n' > mini/src/main.rs
cd mini
cargo build -q --offline
./target/debug/mini
cargo build -v --offline 2>&1 | grep -c '^ *Fresh mini'
"#;

/// The npm workload: a local package installed into an application and required from it.
const NPM: &str = r#"
mkdir -p pkg app
printf '{"name":"slates-pkg","version":"1.0.0","main":"index.js","license":"MIT"}\n' > pkg/package.json
printf 'module.exports = { greet: () => "hello from slates-pkg" };\n' > pkg/index.js
printf '{"name":"app","version":"1.0.0","private":true,"license":"MIT"}\n' > app/package.json
cd app
npm install ../pkg --no-audit --no-fund --loglevel=silent
node -e 'console.log(require("slates-pkg").greet())'
npm ls --depth=0
"#;

/// The python workload: a package imported, files written and read back, a bytecode compile.
const PYTHON: &str = r#"
mkdir -p pkg
printf 'def greet():\n    return "hello from pkg"\n' > pkg/__init__.py
printf 'import json, os, pkg\nprint(pkg.greet())\nwith open("out.json", "w") as f:\n    json.dump({"names": sorted(os.listdir("."))}, f)\nprint(open("out.json").read())\n' > main.py
python3 -B main.py
python3 -m py_compile pkg/__init__.py
python3 -c 'import pkg; print(pkg.greet())'
"#;

/// The ripgrep workload: a tree searched, counted and listed in path order.
const RIPGREP: &str = r#"
mkdir -p src/nested
printf 'alpha needle\nbeta\n' > src/a.txt
printf 'needle gamma\n' > src/nested/b.txt
printf 'delta\n' > src/c.txt
rg -n --sort path needle src
rg -c --sort path needle src
rg --files --sort path src
"#;

/// The rsync workload: a tree with a symlink and a mode copied, then re-synced (nothing to do).
const RSYNC: &str = r#"
mkdir -p src/sub
printf 'one\n' > src/one.txt
printf 'two\n' > src/sub/two.txt
ln -s one.txt src/link
chmod u=rw,g=r,o= src/one.txt
rsync -a src/ dst/
rsync -ai src/ dst/ | wc -l | tr -d ' '
diff -r src dst && echo same
"#;

/// The sqlite workload (T-3.3): a WAL-mode database written by two processes at once (each waiting out
/// the other's lock for [`ENV_SQLITE_BUSY_MS`]), then checked and checkpointed back to rollback mode. The
/// two `journal_mode` PRAGMA outputs are discarded, not compared: WAL needs shared-memory mmap, which a
/// network mount does not provide, so sqlite correctly falls back to a rollback journal there and
/// `PRAGMA journal_mode=WAL` answers `delete` on the mount and `wal` on the host — a property of the fs's
/// mmap support, not of the workload. What is compared is the data the concurrent writers produced (the
/// six rows) and the integrity check; the database file itself is excluded (its bytes embed the journal
/// history and page-allocation order, which the fallback changes).
const SQLITE: &str = r#"
sqlite3 db.sqlite 'PRAGMA journal_mode=WAL;' > /dev/null
sqlite3 db.sqlite 'CREATE TABLE t(id INTEGER PRIMARY KEY, x TEXT);'
sqlite3 -cmd ".timeout $SLATES_SQLITE_BUSY_MS" db.sqlite "INSERT INTO t(x) VALUES ('a'),('b'),('c');" &
sqlite3 -cmd ".timeout $SLATES_SQLITE_BUSY_MS" db.sqlite "INSERT INTO t(x) VALUES ('d'),('e'),('f');" &
wait
sqlite3 db.sqlite 'SELECT count(*), group_concat(x) FROM (SELECT x FROM t ORDER BY x);'
sqlite3 db.sqlite 'PRAGMA integrity_check;'
sqlite3 db.sqlite 'PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;' > /dev/null
"#;

/// The editor workload: vim's save pattern in batch mode — the original renamed to a backup and
/// a new file written (`backupcopy=no`), the write-new-then-rename shape editors use.
const EDITOR: &str = r#"
printf 'draft\n' > note.txt
vim -u NONE -N -i NONE -es -c 'set backup backupdir=. writebackup backupcopy=no' -c 'normal! ihello ' -c 'wq' note.txt
cat note.txt
ls -A
"#;

/// The macOS watcher workload: fswatch reports the creation of a file (FSEvents/kqueue), given
/// [`ENV_WATCH_SECONDS`] to notice it. The assertion is that the watcher *observed* the creation, not
/// the raw `events.txt` — its exact bytes depend on the fs's event delivery (a network mount coalesces
/// differently and fires an extra event for the AppleDouble sidecar the NFS client writes), which is the
/// transport's behaviour, not the workload's; `events.txt` is excluded from the tree for the same reason.
const WATCHER_MACOS: &str = r#"
mkdir -p watched
fswatch -1 --event Created watched > events.txt &
watcher=$!
sleep 1
printf 'x\n' > watched/new.txt
sleep "$SLATES_WATCH_SECONDS"
kill $watcher 2>/dev/null || true
wait $watcher 2>/dev/null || true
if grep -q 'new\.txt' events.txt; then echo "observed new.txt created"; else echo "missed new.txt"; fi
"#;

/// The Linux watcher workload: inotifywait reports the creation of a file within [`ENV_WATCH_SECONDS`].
/// As with the macOS form, the assertion is that the creation was observed, not the raw `events.txt`.
const WATCHER_LINUX: &str = r#"
mkdir -p watched
inotifywait -q -e create -t "$SLATES_WATCH_SECONDS" --format '%e %f' watched > events.txt &
watcher=$!
sleep 1
printf 'x\n' > watched/new.txt
wait $watcher || true
if grep -q 'new\.txt' events.txt; then echo "observed new.txt created"; else echo "missed new.txt"; fi
"#;

/// Format: the environment variable the sqlite script reads its busy timeout from, in
/// milliseconds; the harness sets it from its recorded bound.
pub const ENV_SQLITE_BUSY_MS: &str = "SLATES_SQLITE_BUSY_MS";
/// Format: the environment variable the watcher scripts read their wait from, in seconds; the
/// harness sets it from its recorded bound.
pub const ENV_WATCH_SECONDS: &str = "SLATES_WATCH_SECONDS";

/// The roster, in the design's order (Part 6 "Real workloads"); both watcher forms are listed
/// and the harness keeps the one for its operating system.
pub const ROSTER: &[Workload] = &[
  Workload {
    name: "git",
    tool: "git",
    script: GIT,
    excluded: &[
      (
        ".git/index",
        "the index caches stat data (mtime, inode, device) by design",
      ),
      (
        ".git/logs/",
        "reflog lines carry the wall-clock time of each update",
      ),
      (".git/COMMIT_EDITMSG", "a scratch file of the last commit"),
      (".git/ORIG_HEAD", "a scratch ref"),
    ],
  },
  Workload {
    name: "cargo",
    tool: "cargo",
    script: CARGO,
    excluded: &[(
      "mini/target/",
      "fingerprints and dep-info embed the absolute source path and mtimes by cargo's design",
    )],
  },
  Workload {
    name: "npm",
    tool: "npm",
    script: NPM,
    excluded: &[],
  },
  Workload {
    name: "python",
    tool: "python3",
    script: PYTHON,
    excluded: &[
      (
        "pkg/__pycache__/",
        "a .pyc header embeds the source's mtime and size by design",
      ),
      ("__pycache__/", "same"),
    ],
  },
  Workload {
    name: "rg",
    tool: "rg",
    script: RIPGREP,
    excluded: &[],
  },
  Workload {
    name: "rsync",
    tool: "rsync",
    script: RSYNC,
    excluded: &[],
  },
  Workload {
    name: "sqlite",
    tool: "sqlite3",
    script: SQLITE,
    excluded: &[(
      "db.sqlite",
      "the database file's bytes embed the journal mode and page-allocation history, which the mount's WAL fallback changes; the six rows and the integrity check are verified in the output",
    )],
  },
  Workload {
    name: "editor",
    tool: "vim",
    script: EDITOR,
    excluded: &[],
  },
  Workload {
    name: "watcher",
    tool: "fswatch",
    script: WATCHER_MACOS,
    excluded: &[(
      "events.txt",
      "the watcher's raw event log; its bytes depend on the fs's event delivery (coalescing, ordering, sidecar events), so the workload asserts the creation was observed instead",
    )],
  },
  Workload {
    name: "watcher",
    tool: "inotifywait",
    script: WATCHER_LINUX,
    excluded: &[(
      "events.txt",
      "the watcher's raw event log; its bytes depend on the fs's event delivery (coalescing, ordering, sidecar events), so the workload asserts the creation was observed instead",
    )],
  },
];

/// The kind of a manifest entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum EntryKind {
  /// A regular file.
  File,
  /// A directory.
  Directory,
  /// A symbolic link with its target text.
  Symlink {
    /// The link's target, as stored.
    target: String,
  },
  /// A fifo, socket or device.
  Other,
}

/// One entry of a tree manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
  /// The path relative to the workload directory, `/`-separated.
  pub path: String,
  /// The kind.
  pub kind: EntryKind,
  /// The permission bits (the low twelve of `st_mode`).
  pub mode: u32,
  /// The size in bytes (files only; 0 otherwise).
  pub size: u64,
  /// The BLAKE3 of the bytes (files only; empty otherwise), hexadecimal.
  pub digest: String,
}

/// A tree manifest: the entries sorted by path.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
  /// The entries.
  pub entries: Vec<Entry>,
}

impl Manifest {
  /// The manifest without the excluded prefixes.
  pub fn without(&self, excluded: &[(&str, &str)]) -> Manifest {
    Manifest {
      entries: self
        .entries
        .iter()
        .filter(|e| {
          !excluded
            .iter()
            .any(|(prefix, _)| e.path.starts_with(prefix) || e.path == prefix.trim_end_matches('/'))
        })
        .cloned()
        .collect(),
    }
  }
}

/// One run of a workload script.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
  /// The directory the script ran in (replaced in the output before comparison).
  pub directory: String,
  /// The script's exit code.
  pub exit_code: i32,
  /// The script's combined output.
  pub output: String,
  /// The tree afterwards.
  pub manifest: Manifest,
}

/// Format: the token both directory roots become in a normalized output.
const ROOT_TOKEN: &str = "<dir>";

/// The output with the run's directory (and its `/private` twin on macOS) replaced by one token.
pub fn normalize_output(output: &str, directory: &str) -> String {
  let trimmed = directory.trim_end_matches('/');
  let private = format!("/private{trimmed}");
  output
    .replace(&private, ROOT_TOKEN)
    .replace(trimmed, ROOT_TOKEN)
}

/// The first differing line of two texts, as a bounded detail.
fn first_output_difference(host: &str, mount: &str) -> Option<String> {
  let mut host_lines = host.lines();
  let mut mount_lines = mount.lines();
  let mut number = 0usize;
  loop {
    number += 1;
    match (host_lines.next(), mount_lines.next()) {
      (None, None) => return None,
      (h, m) if h == m => {}
      (h, m) => {
        return Some(detail(&format!(
          "output line {number}: host {:?}, mount {:?}",
          h.unwrap_or("<end>"),
          m.unwrap_or("<end>")
        )));
      }
    }
  }
}

/// The first differing entry of two manifests, as a bounded detail.
fn first_manifest_difference(host: &Manifest, mount: &Manifest) -> Option<String> {
  let mut host_entries = host.entries.iter();
  let mut mount_entries = mount.entries.iter();
  loop {
    match (host_entries.next(), mount_entries.next()) {
      (None, None) => return None,
      (Some(h), Some(m)) if h == m => {}
      (h, m) => {
        return Some(detail(&format!(
          "tree entry: host {}, mount {}",
          h.map_or("<end>".to_owned(), describe),
          m.map_or("<end>".to_owned(), describe)
        )));
      }
    }
  }
}

/// Shape: the leading hexadecimal characters of a digest a difference names (enough to tell two
/// digests apart by eye; the record never carries the whole 64).
const DIGEST_PREVIEW_CHARS: usize = 12;

fn describe(entry: &Entry) -> String {
  format!(
    "{} ({:?}, mode {:o}, {} bytes, {})",
    entry.path,
    entry.kind,
    entry.mode,
    entry.size,
    if entry.digest.is_empty() {
      "-"
    } else {
      &entry.digest[..entry.digest.len().min(DIGEST_PREVIEW_CHARS)]
    }
  )
}

/// Compares a workload's host run with its mounted run.
pub fn compare(workload: &Workload, host: &Run, mount: &Run) -> WorkloadStatus {
  if host.exit_code != mount.exit_code {
    return WorkloadStatus::Differs {
      detail: detail(&format!(
        "exit code: host {}, mount {}; mount output tail: {}",
        host.exit_code,
        mount.exit_code,
        crate::exerciser::tail(&mount.output)
      )),
    };
  }
  let host_output = normalize_output(&host.output, &host.directory);
  let mount_output = normalize_output(&mount.output, &mount.directory);
  if let Some(difference) = first_output_difference(&host_output, &mount_output) {
    return WorkloadStatus::Differs { detail: difference };
  }
  let host_tree = without_sidecars(host.manifest.without(workload.excluded));
  let mount_tree = without_sidecars(mount.manifest.without(workload.excluded));
  match first_manifest_difference(&host_tree, &mount_tree) {
    Some(difference) => WorkloadStatus::Differs { detail: difference },
    None => WorkloadStatus::Identical,
  }
}

/// Format: the prefix of a macOS AppleDouble sidecar (`._name`), the file the NFS client keeps an
/// extended attribute in when the server cannot (docs/bugs/2026-09-14-nfs-appledouble-sidecars.md).
const APPLEDOUBLE_PREFIX: &str = "._";

/// Whether an entry is an AppleDouble sidecar by its base name.
fn is_sidecar(entry: &Entry) -> bool {
  entry
    .path
    .rsplit('/')
    .next()
    .is_some_and(|name| name.starts_with(APPLEDOUBLE_PREFIX))
}

/// A manifest with the macOS AppleDouble sidecars removed. The macOS NFS client writes an extended
/// attribute into a `._name` file whenever the server cannot store it inline
/// (docs/bugs/2026-09-14-nfs-appledouble-sidecars.md), so these files are an artifact of the mount
/// transport, not workload behaviour or slates-fs state; like the per-workload excluded prefixes they are
/// removed from both sides before the whole-tree comparison.
fn without_sidecars(manifest: Manifest) -> Manifest {
  Manifest {
    entries: manifest
      .entries
      .into_iter()
      .filter(|e| !is_sidecar(e))
      .collect(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn file(path: &str, digest: &str) -> Entry {
    Entry {
      path: path.to_owned(),
      kind: EntryKind::File,
      mode: 0o644,
      size: 5,
      digest: digest.to_owned(),
    }
  }

  fn run(directory: &str, output: &str, entries: Vec<Entry>) -> Run {
    Run {
      directory: directory.to_owned(),
      exit_code: 0,
      output: output.to_owned(),
      manifest: Manifest { entries },
    }
  }

  /// Identical runs whose outputs differ only by their directory roots (including macOS's
  /// `/private` twin) compare identical; an excluded prefix hides a difference by design.
  #[test]
  fn identical_runs_compare_identical_under_the_exclusions() {
    let workload = ROSTER[0];
    let host = run(
      "/h/git",
      "app /h/git\nok\n",
      vec![file("a.txt", "aa"), file(".git/index", "11")],
    );
    let mount = run(
      "/m/git",
      "app /private/m/git\nok\n",
      vec![file("a.txt", "aa"), file(".git/index", "22")],
    );
    assert_eq!(compare(&workload, &host, &mount), WorkloadStatus::Identical);
  }

  /// The detail of a differing comparison; a test that expected a difference fails otherwise.
  fn difference(status: WorkloadStatus) -> String {
    match status {
      WorkloadStatus::Differs { detail } => detail,
      other => panic!("expected a difference, got {other:?}"),
    }
  }

  /// A differing exit code is named first, with the mounted output's tail.
  #[test]
  fn a_differing_exit_code_is_named() {
    let workload = ROSTER[4];
    let host = run("/h", "same\n", vec![file("a", "aa")]);
    let mut mount = run("/m", "same\n", vec![file("a", "aa")]);
    mount.exit_code = 1;
    let detail = difference(compare(&workload, &host, &mount));
    assert!(detail.starts_with("exit code: host 0, mount 1"), "{detail}");
  }

  /// A differing output line is named with its number.
  #[test]
  fn a_differing_output_line_is_named() {
    let workload = ROSTER[4];
    let host = run("/h", "same\n", vec![file("a", "aa")]);
    let mount = run("/m", "other\n", vec![file("a", "aa")]);
    let detail = difference(compare(&workload, &host, &mount));
    assert!(detail.contains("output line 1"), "{detail}");
  }

  /// A tree that differs only by AppleDouble sidecars compares Identical — the sidecars are a mount
  /// artifact, stripped from both sides — while a difference beyond them is still named.
  #[test]
  fn appledouble_sidecars_are_excluded_from_the_comparison() {
    let workload = ROSTER[4];
    let host = run("/h", "same\n", vec![file("a", "aa")]);
    let mount = run(
      "/m",
      "same\n",
      vec![file("._a", "sidecar"), file("a", "aa")],
    );
    assert_eq!(compare(&workload, &host, &mount), WorkloadStatus::Identical);
    let mount = run(
      "/m",
      "same\n",
      vec![file("._a", "sidecar"), file("a", "bb")],
    );
    let detail = difference(compare(&workload, &host, &mount));
    assert!(detail.starts_with("tree entry"), "{detail}");
  }

  /// A differing tree entry, or an extra one, is named with both sides.
  #[test]
  fn a_differing_tree_entry_is_named() {
    let workload = ROSTER[4];
    let host = run("/h", "same\n", vec![file("a", "aa")]);
    let mount = run("/m", "same\n", vec![file("a", "bb")]);
    let detail = difference(compare(&workload, &host, &mount));
    assert!(detail.starts_with("tree entry"), "{detail}");
    let mount = run("/m", "same\n", vec![file("a", "aa"), file("b", "bb")]);
    let detail = difference(compare(&workload, &host, &mount));
    assert!(detail.contains("<end>"), "{detail}");
  }

  /// Every roster entry has a name, a tool and a script, and every exclusion carries its reason.
  #[test]
  fn the_roster_is_complete() {
    for workload in ROSTER {
      assert!(!workload.name.is_empty() && !workload.tool.is_empty());
      assert!(
        workload.script.trim().lines().count() >= 2,
        "{}",
        workload.name
      );
      for (prefix, reason) in workload.excluded {
        assert!(
          !prefix.is_empty() && !reason.is_empty(),
          "{}",
          workload.name
        );
      }
    }
    let names: Vec<&str> = ROSTER.iter().map(|w| w.name).collect();
    for expected in [
      "git", "cargo", "npm", "python", "rg", "rsync", "sqlite", "editor", "watcher",
    ] {
      assert!(names.contains(&expected), "{expected}");
    }
  }
}
