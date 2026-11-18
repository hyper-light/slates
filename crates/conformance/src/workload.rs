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
//!
//! On Windows (A-300) the lane runs the version-control workloads ([`Workload::runs_on_windows`]) with Git for
//! Windows' POSIX shell, in an NTFS folder and on the WinFsp drive. Windows tools print a directory in several
//! spellings (`D:\a`, `D:/a`, the shell's `/d/a`), each of which becomes the root token ([`normalize_output`]). And a
//! route may state a difference from its host that is a property of the route, never a defect: the drive makes no hard
//! link, since WinFsp's driver refuses one, so a local clone copies there where NTFS links. Those lines are named per
//! route ([`stated_differences`]) and recorded with both sides ([`WorkloadStatus::Stated`]); every other byte is
//! compared as on every other leg, and a stated line that no longer differs fails, so the list only shrinks.

use serde::{Deserialize, Serialize};

use crate::record::{Transport, WorkloadStatus, detail};

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
  /// Whether the Windows lane runs it over the WinFsp drive (A-300): the version-control workloads, whose tools a
  /// Windows runner carries (Git for Windows with its shell, Mercurial's wheel, Subversion's command-line build). The
  /// others are owed a Windows form: their scripts lean on POSIX tools and behaviour a Windows runner lacks (rsync, a
  /// watcher's inotify or FSEvents, rg, sqlite3, vim) or have not been run there (cargo, npm, python).
  pub runs_on_windows: bool,
}

/// The git workload: a repository with files, a directory, a symlink, a mode change and two
/// commits; the tree hash proves byte-identical content and modes, `git status` must be clean. The
/// object count is compared, not `git count-objects`' kilobytes: those are `st_blocks`, the host
/// filesystem's allocation unit (APFS rounds every loose object up to 4 KiB, the export reports the
/// bytes held), a property of the filesystem under the repository, not of git (the CI macOS runner,
/// 2026-09-16: "10 objects, 40 kilobytes" on the host, "10 objects, 5 kilobytes" on the mount).
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
git count-objects -v | grep '^count:'
"#;

/// The git history workload: the operations an agent's repository lives by, over a bare upstream and two clones
/// (the goal of 2026-11-18). A local clone links the upstream's loose objects rather than copying them (`git clone`'s
/// `--local`, the default for a path), so the output counts the clone's objects with two names: a mount that cannot
/// link, or reports the wrong link count, differs there. Then a branch, a merge, a rebase, a stash and its pop, a
/// second worktree (made, committed in and removed, so no file names an absolute path), an annotated tag, a push and
/// a pull, and the maintenance that rewrites every object file: `gc`, a full `repack`, the upstream's `gc` (a bare
/// repository writes a reachability bitmap too), `verify-pack` and `fsck`. `pack.threads=1` makes the packs
/// byte-identical between runs: with more threads the delta search hands work between threads by timing. The clone's
/// origin is set back to the relative path, since `git clone` records a local source by its absolute path.
/// `count-objects` prints its counts only, not its kilobytes (the `git` workload's reason).
const GIT_HISTORY: &str = r#"
git init -q --bare -b main upstream.git
git init -q -b main seed
cd seed
printf 'one\n' > a.txt
mkdir -p d
printf 'two\n' > d/b.txt
git add -A
git commit -q -m base
git remote add origin ../upstream.git
git push -q origin main
cd ..
git clone -q upstream.git work
cd work
git remote set-url origin ../upstream.git
echo "objects with two names after the clone: $(find .git/objects -type f -links 2 | wc -l | tr -d ' ') of $(find .git/objects -type f | wc -l | tr -d ' ')"
git switch -q -c feature
printf 'feature\n' > f.txt
git add f.txt
git commit -q -m feature
git switch -q main
printf 'main\n' >> a.txt
git commit -q -am 'main change'
git merge -q --no-ff -m 'merge feature' feature
git switch -q -c topic HEAD~1
printf 'topic\n' > t.txt
git add t.txt
git commit -q -m topic
git rebase -q main
git switch -q main
git merge -q --ff-only topic
printf 'wip\n' >> d/b.txt
git stash -q
git status --porcelain
git stash pop -q
git status --porcelain
git checkout -q -- d/b.txt
git worktree add -q -b hotfix ../wt main~1
cd ../wt
printf 'fix\n' > fix.txt
git add fix.txt
git commit -q -m fix
cd ../work
git worktree list --porcelain
git merge -q --no-ff -m 'merge hotfix' hotfix
git worktree remove ../wt
git tag -a -m 'release one' v1
git push -q origin main topic v1
cd ../seed
git pull -q --ff-only origin main
git log --format='%H %T %P %s'
cd ../work
git -c pack.threads=1 -c gc.autoDetach=false gc -q --prune=now
git -c pack.threads=1 repack -q -a -d -f
git -C ../upstream.git -c pack.threads=1 -c gc.autoDetach=false gc -q --prune=now
git count-objects -v | grep -E '^(count|in-pack|packs|prune-packable|garbage):'
git verify-pack -v .git/objects/pack/pack-*.idx | grep -v '^chain length'
git fsck --full --strict --no-progress
git -C ../upstream.git fsck --full --strict --no-progress
git log --graph --all --format='%H %T %P%d %s'
git status --porcelain=v2 --branch
"#;

/// The Mercurial workload (the goal of 2026-11-18): a repository with a named branch, a merge, a tag and a bookmark;
/// a local clone, whose store files are hard links to the source's (`hg clone` links a local store when it can); a
/// commit in the clone, before which Mercurial copies each store file it appends to and still shares, judging that by
/// the file's link count; a rebase (an extension shipped with Mercurial; it strips the old commit into a backup
/// bundle); a push back; a share of the source's store (`hg share --relative`, so no file names an absolute path);
/// then `verify` of all three. The output counts the store files with two names after the clone and after the
/// clone's commit, so a mount that cannot link, or reports a stale or wrong link count, differs there; and one that
/// let the clone append into a store file it still shared would fail the source's `verify`. Every commit carries one
/// fixed date, as the git workloads' do, and the clone's `hgrc` is written back with the source's relative path,
/// since `hg clone` records a local source by its absolute path.
const MERCURIAL: &str = r#"
# Format: every commit's one fixed date, as the git workloads fix theirs.
d='2026-09-14 00:00:00 +0000'
hg init src
cd src
printf 'one\n' > a.txt
mkdir -p d
printf 'two\n' > d/b.txt
hg add -q
hg commit -q -d "$d" -m base
hg branch -q feature
printf 'feature\n' > f.txt
hg add -q f.txt
hg commit -q -d "$d" -m feature
hg update -q default
printf 'main\n' >> a.txt
hg commit -q -d "$d" -m 'main change'
hg merge -q feature
hg commit -q -d "$d" -m 'merge feature'
hg tag -d "$d" v1
hg bookmark -r 'desc("main change")' stable
hg status --all
cd ..
hg clone -q src clone
echo "store files with two names after the clone: $(find clone/.hg/store -type f -links 2 | wc -l | tr -d ' ') of $(find clone/.hg/store -type f | wc -l | tr -d ' ')"
cd clone
printf '[paths]\ndefault = ../src\n' > .hg/hgrc
hg update -q stable
printf 'clone\n' > c.txt
hg add -q c.txt
hg commit -q -d "$d" -m 'clone change'
echo "store files with two names after the clone's commit: $(find .hg/store -type f -links 2 | wc -l | tr -d ' ')"
hg --config extensions.rebase= rebase -q -d 'children(v1)'
hg log -G -T '{rev} {node} {branch} {bookmarks} {desc}\n'
hg push -q -r . ../src
cd ../src
hg update -q tip
hg --config extensions.share= share -q --relative . ../shared
cd ../shared
printf 'shared\n' > s.txt
hg add -q s.txt
hg commit -q -d "$d" -m 'shared change'
cd ../src
hg update -q tip
hg diff --git -r 'desc("base")' -r tip
hg log -G -T '{rev} {node} {branch} {p1node} {p2node} {tags} {bookmarks} {desc}\n'
hg annotate a.txt
hg verify -q
hg -R ../clone verify -q
hg -R ../shared verify -q
hg -R ../clone status --all
hg -R ../shared status --all
"#;

/// The Subversion workload (the goal of 2026-11-18): an FSFS repository made with a fixed UUID; a working copy of its
/// trunk, whose state is SQLite (`.svn/wc.db`, journalled and locked as SQLite does over the file system under it);
/// two commits, a branch by a server-side copy, a switch to it and a commit there, a switch back and a commit on
/// trunk, an update back to an older revision and forward again, a merge of the branch into trunk (Subversion's own
/// merge, recorded in `svn:mergeinfo`), a tag, a cleanup, a second working copy of the branch, then the repository
/// verified and dumped. Every revision's date is set to one fixed instant before the history is printed (`svnadmin
/// setrevprop`, which runs no hook), as the git and Mercurial workloads fix theirs; the client's configuration is the
/// workload's own (`--config-dir`), so nothing outside the tree is read or written. Every repository URL holds the
/// workload's absolute path, which the comparison replaces by the root token. Under Git for Windows' shell (A-300) the
/// directory is taken in its drive form (`pwd -W`, `D:/a`; other shells refuse the option and the plain `pwd` answers),
/// since a file URL names a Windows directory as `file:///D:/a`, and `svnlook youngest` ends its line with Windows'
/// carriage return there, which the revision loop's count must not carry.
///
/// No commit sends more than one file's text. Subversion's client sends a commit's texts after its tree, in the order
/// of a hash table (`svn_client__do_commit`'s `file_mods`, `subversion/libsvn_client/commit_util.c` of 1.14), and APR
/// seeds each hash table from the clock and its addresses (`apr_hash_make`, `tables/apr_hash.c` of 1.7), so the order
/// is drawn anew in every process; FSFS numbers a revision's items in the order they arrive. One commit of two new
/// files therefore wrote its revision file, and the representation cache's row for each text, in either of two orders:
/// CI's host gate saw the cache's item number 3 in one run and 4 in the other (runs 44252249999 and 44252854695,
/// 2026-11-18), and on Subversion 1.14.5 (kiln run r956) twelve runs of the script with that commit wrote
/// `db/revs/0/2` two ways, seven and five times, every other file alike, while twelve runs with its two files
/// committed apart were identical in every revision file, the cache's rows and its bytes, the output and the dump.
/// The representation cache (`db/rep-cache.db`) is compared by what Subversion keeps in it, its rows, printed where
/// `sqlite3` is on the `PATH`, rather than by SQLite's bytes for them; its bytes had differed between two runs (CI run
/// 44247840584, ubuntu) only because its rows had. Everywhere, `svnadmin verify` checks each row against the
/// revision it names (`verify_rep_cache`, `subversion/libsvn_fs_fs/verify.c` of 1.14). The revisions it caches
/// (`db/revs`, `db/revprops`, `db/current`) are compared byte for byte, and `svnadmin verify` and the dump's checksum
/// print the repository whole.
const SUBVERSION: &str = r#"
top=$(pwd -W 2>/dev/null || pwd)
case $top in
  /*) url="file://$top/repo" ;;
  *) url="file:///$top/repo" ;;
esac
svn() { command svn --config-dir "$top/svn-config" --non-interactive --no-auth-cache "$@"; }
# Format: every revision's one fixed date, and the repository's fixed UUID.
printf '2026-09-14T00:00:00.000000Z' > date
svnadmin create repo
svnadmin setuuid repo 00000000-0000-4000-8000-00000000c0de
svn mkdir -q -m layout "$url/trunk" "$url/branches" "$url/tags"
svn checkout -q "$url/trunk" wc
cd wc
printf 'one\n' > a.txt
svn add -q a.txt
svn commit -q -m base
mkdir d
printf 'two\n' > d/b.txt
svn add -q d
svn commit -q -m 'base directory'
svn copy -q -m 'branch feature' '^/trunk' '^/branches/feature'
svn switch -q '^/branches/feature'
printf 'feature\n' > f.txt
svn add -q f.txt
svn commit -q -m feature
svn switch -q '^/trunk'
printf 'main\n' >> a.txt
svn commit -q -m 'main change'
# Format: revision 3 is the base, its two files each committed alone.
svn update -q -r 3
cat a.txt
svn update -q
svn merge -q '^/branches/feature'
svn status
svn commit -q -m 'merge feature'
svn update -q
svn copy -q -m 'tag v1' '^/trunk' '^/tags/v1'
svn update -q
svn cleanup
svn status
cd ..
svn checkout -q "$url/branches/feature" other
svn status other
youngest=$(svnlook youngest repo | tr -d '\r')
rev=0
while [ "$rev" -le "$youngest" ]; do
  svnadmin setrevprop repo -r "$rev" svn:date date
  rev=$((rev + 1))
done
cd wc
svn info --show-item revision
svn info --show-item last-changed-revision
svn info --show-item url
svn info --show-item repos-uuid
svn log -v -r 1:HEAD '^/'
# Format: revision 6 is the change on trunk, 7 the branch's merge into it.
svn diff -c 6 '^/'
svn propget svn:mergeinfo .
svn cat -r 2 a.txt
svn list -R '^/'
cd ..
svnadmin verify repo
if command -v sqlite3 >/dev/null 2>&1; then
  sqlite3 repo/db/rep-cache.db 'SELECT hash, revision, offset, size, expanded_size FROM rep_cache ORDER BY hash'
fi
svnlook tree repo
# Format: revision 7, the merge, as the repository records it.
svnlook changed -r 7 repo
svnadmin dump -q repo | cksum
"#;

/// The cargo workload: a dependency-free crate built twice; the second build must report the
/// crate `Fresh` (AC-3.2 "incremental builds reuse artifacts") and the binary must run. The crate is its
/// own workspace root (`[workspace]`), so it builds the same wherever its directory sits — a volume or a
/// scratch inside another workspace included (the harness's scratch moved into the repository's `target/`
/// on 2026-09-30, and cargo then took the repository's manifest for this crate's workspace).
const CARGO: &str = r#"
mkdir -p mini/src
printf '[package]\nname = "mini"\nversion = "0.1.0"\nedition = "2024"\n\n[dependencies]\n\n[workspace]\n' > mini/Cargo.toml
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
/// Clear the temporary-path exclusion so both roots exercise backup creation; reading the backup
/// makes a missing backup fail even when both roots would otherwise skip it (T-3.3).
const EDITOR: &str = r#"
printf 'draft\n' > note.txt
vim -u NONE -N -i NONE -es -c 'set backup backupdir=. writebackup backupcopy=no backupskip=' -c 'normal! ihello ' -c 'wq' note.txt
cat note.txt
cat note.txt~
ls -A
"#;

/// The macOS watcher workload: fswatch reports the creation of a file (FSEvents/kqueue). The assertion
/// is that the watcher *observed* the creation, not the raw `events.txt` — its exact bytes depend on the
/// fs's event delivery (a network mount coalesces differently and fires an extra event for the AppleDouble
/// sidecar the NFS client writes), which is the transport's behaviour, not the workload's; `events.txt` is
/// excluded from the tree for the same reason.
///
/// The watcher's readiness is observed, never assumed: FSEvents gives no signal that a stream is live,
/// so probe files are created until the watcher reports one, and only then is `new.txt` created; each
/// wait is bounded by [`ENV_WATCH_SECONDS`]. (A fixed one-second sleep before the creation let a loaded
/// runner's host watcher miss it: CI 36281600448.) The probes are removed before the tree is compared.
const WATCHER_MACOS: &str = r#"
mkdir -p watched
# Shape: FSEvents' coalescing latency (fswatch --latency), the resolution the watcher reports at, so
# the waits poll at it.
latency=0.1
# The output file exists before the watcher starts, so no poll reads a file not yet created.
: > events.txt
fswatch --event Created --latency "$latency" watched > events.txt &
watcher=$!
deadline=$(( $(date +%s) + SLATES_WATCH_SECONDS ))
probe=0
until grep -qs 'ready\.[0-9]' events.txt; do
  if [ "$(date +%s)" -ge "$deadline" ]; then break; fi
  probe=$((probe + 1))
  : > "watched/ready.$probe"
  sleep "$latency"
done
rm -f watched/ready.*
printf 'x\n' > watched/new.txt
deadline=$(( $(date +%s) + SLATES_WATCH_SECONDS ))
until grep -qs 'new\.txt' events.txt; do
  if [ "$(date +%s)" -ge "$deadline" ]; then break; fi
  sleep "$latency"
done
kill $watcher 2>/dev/null || true
wait $watcher 2>/dev/null || true
if grep -q 'new\.txt' events.txt; then echo "observed new.txt created"; else echo "missed new.txt"; fi
"#;

/// The Linux watcher workload: inotifywait reports the creation of a file within [`ENV_WATCH_SECONDS`].
/// As with the macOS form, the assertion is that the creation was observed, not the raw `events.txt`.
/// The watcher's readiness is observed: inotifywait prints `Watches established.` once its watch is
/// in place, and the file is created only after that line (bounded by the same wait). The output files
/// are created before the watcher starts: over a network mount the background job's redirect can land
/// after the first poll (CI 36284404440: `grep: watch.log: No such file or directory` in the output).
const WATCHER_LINUX: &str = r#"
mkdir -p watched
# Shape: how often the watcher's readiness is checked, far below the recorded wait.
poll=0.1
# The output files exist before the watcher starts, so no poll reads a file not yet created.
: > events.txt
: > watch.log
inotifywait -e create -t "$SLATES_WATCH_SECONDS" --format '%e %f' watched > events.txt 2> watch.log &
watcher=$!
deadline=$(( $(date +%s) + SLATES_WATCH_SECONDS ))
until grep -qs 'Watches established' watch.log; do
  if [ "$(date +%s)" -ge "$deadline" ]; then break; fi
  sleep "$poll"
done
printf 'x\n' > watched/new.txt
wait $watcher || true
if grep -q 'new\.txt' events.txt; then echo "observed new.txt created"; else echo "missed new.txt"; fi
"#;

/// Format: the environment every leg runs a workload under, on both of its sides (the host and the mount, a container
/// and the guest), so the two runs of a script differ only by the file system under them:
/// - the one identity and date every git commit is made with, and none of the configuration a parent passes (kiln's
///   agent sessions sign every commit through `GIT_CONFIG_COUNT`, and a signature carries its signing time);
/// - Mercurial reading no configuration but a repository's own (`HGRCPATH` empty, per `hg help environment`), its
///   plain output (`HGPLAIN`) and one committer; its commits' dates are the script's own `-d`;
/// - one Python hash seed: Mercurial writes its store's file list (`fncache`) from a set, in the order the
///   interpreter's string hashing gives, which is drawn afresh in every process unless fixed (measured 2026-11-17:
///   two runs of the Mercurial workload wrote one list in two orders);
/// - one time zone and locale, and npm with no update check, funding or audit messages and no log files (their path,
///   in the cache, would be the one line that differs between runs);
/// - cargo's plain output and its own target directory (`CARGO_TARGET_DIR=target`, which cargo takes relative to the
///   directory it runs in, so the crate's own), whatever a parent sets: every CI job sets `CARGO_TERM_COLOR=always`,
///   whose escape sequence before `Fresh` made the cargo workload's count of fresh builds read 0, failing it on both
///   sides (CI run 44247840584, macOS, 2026-11-18), and kiln's runs set a target directory outside the tree, where the
///   workload's `./target/debug/mini` would not be;
/// - under Git for Windows' shell (A-300), `ln -s` making a Windows symbolic link and failing where it cannot
///   (`winsymlinks:nativestrict`), as POSIX's does; by default that shell copies the target instead, which would hide
///   whether the drive makes links (A-275). Every other shell ignores the variable.
pub const ENVIRONMENT: &[(&str, &str)] = &[
  ("GIT_AUTHOR_NAME", "slates"),
  ("GIT_AUTHOR_EMAIL", "slates@example.invalid"),
  ("GIT_COMMITTER_NAME", "slates"),
  ("GIT_COMMITTER_EMAIL", "slates@example.invalid"),
  ("GIT_AUTHOR_DATE", "2026-09-14T00:00:00Z"),
  ("GIT_COMMITTER_DATE", "2026-09-14T00:00:00Z"),
  ("GIT_CONFIG_GLOBAL", "/dev/null"),
  ("GIT_CONFIG_NOSYSTEM", "1"),
  ("GIT_CONFIG_COUNT", "0"),
  ("HGRCPATH", ""),
  ("HGPLAIN", "1"),
  ("HGUSER", "slates <slates@example.invalid>"),
  ("PYTHONHASHSEED", "0"),
  ("TZ", "UTC"),
  ("LC_ALL", "C"),
  ("npm_config_update_notifier", "false"),
  ("npm_config_fund", "false"),
  ("npm_config_audit", "false"),
  ("npm_config_logs_max", "0"),
  ("CARGO_TERM_COLOR", "never"),
  ("CARGO_TARGET_DIR", "target"),
  ("MSYS", "winsymlinks:nativestrict"),
];

/// [`ENVIRONMENT`] as POSIX shell `export` lines ([`export_line`]), for a leg that sets it inside a script (the live
/// guest's).
pub fn environment_exports() -> String {
  ENVIRONMENT
    .iter()
    .map(|(key, value)| export_line(key, value))
    .collect()
}

/// One POSIX shell `export` line with the value single-quoted, so the shell takes every byte as it is; a single quote
/// in the value closes the quoting, is escaped, and reopens it (`'\''`).
pub fn export_line(key: &str, value: &str) -> String {
  format!("export {key}='{}'\n", value.replace('\'', "'\\''"))
}

/// Format: the environment variable the sqlite script reads its busy timeout from, in
/// milliseconds; the harness sets it from its recorded bound.
pub const ENV_SQLITE_BUSY_MS: &str = "SLATES_SQLITE_BUSY_MS";
/// Format: the environment variable the watcher scripts read their wait from, in seconds; the
/// harness sets it from its recorded bound.
pub const ENV_WATCH_SECONDS: &str = "SLATES_WATCH_SECONDS";
/// Shape: the busy timeout the sqlite workload waits out a sibling's lock with, milliseconds; the two inserts
/// race by design and the loser must wait rather than fail. One value for every leg that runs the roster (host,
/// container, guest), so their runs are judged under one bound.
pub const SQLITE_BUSY_MS: u64 = 5_000;
/// Shape: how long the watcher workloads wait for their event, seconds; one value for every leg, as above.
pub const WATCH_SECONDS: u64 = 3;

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
    runs_on_windows: true,
  },
  Workload {
    name: "git-history",
    tool: "git",
    script: GIT_HISTORY,
    excluded: &[
      (
        "seed/.git/index",
        "the index caches stat data (mtime, inode, device) by design",
      ),
      (
        "work/.git/index",
        "the index caches stat data (mtime, inode, device) by design",
      ),
      (
        "seed/.git/logs/",
        "a clone's reflog names its source by absolute path, and gc expires entries by their age against the clock",
      ),
      (
        "work/.git/logs/",
        "a clone's reflog names its source by absolute path, and gc expires entries by their age against the clock",
      ),
    ],
    runs_on_windows: true,
  },
  Workload {
    name: "mercurial",
    tool: "hg",
    script: MERCURIAL,
    excluded: &[
      (
        "src/.hg/dirstate",
        "the dirstate caches each file's size and modification time by design",
      ),
      (
        "clone/.hg/dirstate",
        "the dirstate caches each file's size and modification time by design",
      ),
      (
        "shared/.hg/dirstate",
        "the dirstate caches each file's size and modification time by design",
      ),
      (
        "src/.hg/undo.backup.dirstate",
        "the last transaction's copy of the dirstate (Mercurial 6.4 and later)",
      ),
      (
        "clone/.hg/undo.backup.dirstate",
        "the last transaction's copy of the dirstate (Mercurial 6.4 and later)",
      ),
      (
        "shared/.hg/undo.backup.dirstate",
        "the last transaction's copy of the dirstate (Mercurial 6.4 and later)",
      ),
      (
        "src/.hg/undo.dirstate",
        "the last transaction's copy of the dirstate (before Mercurial 6.4)",
      ),
      (
        "clone/.hg/undo.dirstate",
        "the last transaction's copy of the dirstate (before Mercurial 6.4)",
      ),
      (
        "shared/.hg/undo.dirstate",
        "the last transaction's copy of the dirstate (before Mercurial 6.4)",
      ),
      (
        "clone/.hg/undo.desc",
        "the last transaction's description names the repository a push answered by its absolute URL",
      ),
    ],
    runs_on_windows: true,
  },
  Workload {
    name: "subversion",
    tool: "svn",
    script: SUBVERSION,
    excluded: &[
      (
        "wc/.svn/wc.db",
        "the working copy's database (and its journal) caches each file's size and modification time and each \
         commit's server time by design; `svn status` and `svn info` print what it holds",
      ),
      (
        "other/.svn/wc.db",
        "the working copy's database (and its journal) caches each file's size and modification time and each \
         commit's server time by design; `svn status` prints what it holds",
      ),
      (
        "repo/db/uuid",
        "FSFS keeps a random instance id beside the repository's UUID, drawn anew by `svnadmin setuuid` by design; \
         the UUID itself is printed (`svn info --show-item repos-uuid`)",
      ),
      (
        "repo/db/rep-cache.db",
        "FSFS's representation cache is an SQLite file, judged by what Subversion keeps in it, its rows, which are \
         printed where sqlite3 is on the PATH and each checked against the revision it names by `svnadmin verify`, \
         rather than by SQLite's bytes for them; the revisions it caches are compared byte for byte",
      ),
    ],
    runs_on_windows: true,
  },
  Workload {
    name: "cargo",
    tool: "cargo",
    script: CARGO,
    excluded: &[(
      "mini/target/",
      "fingerprints and dep-info embed the absolute source path and mtimes by cargo's design",
    )],
    runs_on_windows: false,
  },
  Workload {
    name: "npm",
    tool: "npm",
    script: NPM,
    excluded: &[],
    runs_on_windows: false,
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
    runs_on_windows: false,
  },
  Workload {
    name: "rg",
    tool: "rg",
    script: RIPGREP,
    excluded: &[],
    runs_on_windows: false,
  },
  Workload {
    name: "rsync",
    tool: "rsync",
    script: RSYNC,
    excluded: &[],
    runs_on_windows: false,
  },
  Workload {
    name: "sqlite",
    tool: "sqlite3",
    script: SQLITE,
    excluded: &[(
      "db.sqlite",
      "the database file's bytes embed the journal mode and page-allocation history, which the mount's WAL fallback changes; the six rows and the integrity check are verified in the output",
    )],
    runs_on_windows: false,
  },
  Workload {
    name: "editor",
    tool: "vim",
    script: EDITOR,
    excluded: &[],
    runs_on_windows: false,
  },
  Workload {
    name: "watcher",
    tool: "fswatch",
    script: WATCHER_MACOS,
    excluded: &[(
      "events.txt",
      "the watcher's raw event log; its bytes depend on the fs's event delivery (coalescing, ordering, sidecar events), so the workload asserts the creation was observed instead",
    )],
    runs_on_windows: false,
  },
  Workload {
    name: "watcher",
    tool: "inotifywait",
    script: WATCHER_LINUX,
    excluded: &[(
      "events.txt",
      "the watcher's raw event log; its bytes depend on the fs's event delivery (coalescing, ordering, sidecar events), so the workload asserts the creation was observed instead",
    )],
    runs_on_windows: false,
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

/// The output with the run's directory replaced by one token, in each spelling a tool prints it:
/// as given, with its `/private` twin on macOS, and with every UUID-shaped path segment as `***`,
/// which is how npm prints a path (it redacts UUIDs from its output; seen 2026-09-26 when the scratch
/// directory's name was a UUID: `app@1.0.0 /private/tmp/.../***/scratchpad/...`). A Windows drive path
/// is replaced in each spelling Windows tools print it too ([`windows_spellings`], A-300).
pub fn normalize_output(output: &str, directory: &str) -> String {
  let trimmed = directory.trim_end_matches(['/', '\\']);
  let redacted = redact_uuids(trimmed);
  let mut spellings = windows_spellings(trimmed);
  if redacted != trimmed {
    spellings.extend([format!("/private{redacted}"), redacted]);
  }
  spellings.extend([format!("/private{trimmed}"), trimmed.to_owned()]);
  spellings
    .iter()
    .filter(|spelling| !spelling.is_empty())
    .fold(output.to_owned(), |text, spelling| {
      text.replace(spelling, ROOT_TOKEN)
    })
}

/// The spellings Windows tools print a drive path in (A-300), for a directory given as `D:\a\b` or in its verbatim form
/// `\\?\D:\a\b`, longest first so none is replaced inside another: the verbatim form; the drive form with `\`
/// separators (Python's and Mercurial's) and with `/` (git's, and a Subversion file URL's `file:///D:/a/b`), each with
/// its letter in either case; and the MSYS shell's `/d/a/b` (Git for Windows' `pwd`). Empty for any other directory,
/// and for a drive's root, whose shell spelling (`/d`) would match inside unrelated text; so a POSIX directory is
/// normalized as it always was.
fn windows_spellings(directory: &str) -> Vec<String> {
  let plain = directory.strip_prefix(r"\\?\").unwrap_or(directory);
  let mut characters = plain.chars();
  let (Some(letter), Some(':')) = (characters.next(), characters.next()) else {
    return Vec::new();
  };
  let rest = characters.as_str();
  let below_the_root = rest
    .strip_prefix(['\\', '/'])
    .is_some_and(|below| !below.is_empty());
  if !letter.is_ascii_alphabetic() || !below_the_root {
    return Vec::new();
  }
  let backslashed = rest.replace('/', "\\");
  let slashed = rest.replace('\\', "/");
  let mut spellings = Vec::new();
  for drive in [letter.to_ascii_uppercase(), letter.to_ascii_lowercase()] {
    spellings.push(format!(r"\\?\{drive}:{backslashed}"));
    spellings.push(format!("{drive}:{backslashed}"));
    spellings.push(format!("{drive}:{slashed}"));
  }
  spellings.push(format!("/{}{slashed}", letter.to_ascii_lowercase()));
  spellings.sort_by_key(|spelling| std::cmp::Reverse(spelling.len()));
  spellings
}

/// A path with every UUID-shaped segment (8-4-4-4-12 hexadecimal digits) replaced by `***`.
fn redact_uuids(path: &str) -> String {
  /// Format: the lengths of a UUID's hyphen-separated groups (RFC 9562 §4).
  const UUID_GROUPS: [usize; 5] = [8, 4, 4, 4, 12];
  let is_uuid = |segment: &str| {
    let groups: Vec<&str> = segment.split('-').collect();
    groups.len() == UUID_GROUPS.len()
      && groups
        .iter()
        .zip(UUID_GROUPS)
        .all(|(group, len)| group.len() == len && group.chars().all(|c| c.is_ascii_hexdigit()))
  };
  path
    .split('/')
    .map(|segment| if is_uuid(segment) { "***" } else { segment })
    .collect::<Vec<_>>()
    .join("/")
}

/// A line of a workload's output that a route prints otherwise than its host by a property of the route the design
/// states, never a defect (A-300). It is matched by the start the line has on both sides, and judged only on its route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatedDifference {
  /// The workload whose output holds the line.
  pub workload: &'static str,
  /// What the line starts with, on both sides.
  pub line_start: &'static str,
  /// Why the route prints it otherwise.
  pub reason: &'static str,
}

/// Why the WinFsp drive's link counts are not NTFS's (A-300).
const NO_HARD_LINK_ON_THE_DRIVE: &str = concat!(
  "the WinFsp drive makes no hard link: WinFsp's driver answers ",
  "FileLinkInformation STATUS_NOT_SUPPORTED before any file system is asked (v2.1, src/sys/fileinfo.c) and its NTFS ",
  "compatibility page lists hard links unsupported, so a local clone copies on the drive where NTFS links; the bytes ",
  "are the same (A-300)",
);

/// The WinFsp drive's stated differences: how many files of a local clone have two names (A-300). git and Mercurial
/// link a local clone's files where the file system can and copy them where it cannot, so these lines count links on
/// NTFS and none on the drive; every other line, and every byte of the trees, is compared.
const WINFSP_STATED: &[StatedDifference] = &[
  StatedDifference {
    workload: "git-history",
    line_start: "objects with two names after the clone: ",
    reason: NO_HARD_LINK_ON_THE_DRIVE,
  },
  StatedDifference {
    workload: "mercurial",
    line_start: "store files with two names after the clone: ",
    reason: NO_HARD_LINK_ON_THE_DRIVE,
  },
  StatedDifference {
    workload: "mercurial",
    line_start: "store files with two names after the clone's commit: ",
    reason: NO_HARD_LINK_ON_THE_DRIVE,
  },
];

/// The lines a route's workloads may print otherwise than its host (A-300): the WinFsp drive's link counts, and none
/// on any other route.
pub fn stated_differences(transport: Transport) -> &'static [StatedDifference] {
  match transport {
    Transport::NativeWindowsWinfsp => WINFSP_STATED,
    _ => &[],
  }
}

/// The output lines of a host and a mounted run compared in order, under the stated differences of the run's
/// workload: the stated lines that differed, each as its start and a bounded detail, or the first other differing line
/// as a bounded detail. A pair of lines is stated only when both start with one stated start, so a stated line never
/// excuses a missing or an extra one.
fn output_difference(
  host: &str,
  mount: &str,
  stated: &[&StatedDifference],
) -> Result<Vec<(&'static str, String)>, String> {
  let mut host_lines = host.lines();
  let mut mount_lines = mount.lines();
  let mut stated_lines = Vec::new();
  let mut number = 0usize;
  loop {
    number = number.saturating_add(1);
    match (host_lines.next(), mount_lines.next()) {
      (None, None) => return Ok(stated_lines),
      (h, m) if h == m => {}
      (h, m) => stated_lines.push(stated_line(number, h, m, stated)?),
    }
  }
}

/// A differing pair of output lines judged against the stated differences: the stated start both lines share with its
/// bounded detail, or the bounded detail as the difference it is.
fn stated_line(
  number: usize,
  host: Option<&str>,
  mount: Option<&str>,
  stated: &[&StatedDifference],
) -> Result<(&'static str, String), String> {
  let line = detail(&format!(
    "output line {number}: host {:?}, mount {:?}",
    host.unwrap_or("<end>"),
    mount.unwrap_or("<end>")
  ));
  let (Some(host), Some(mount)) = (host, mount) else {
    return Err(line);
  };
  match stated
    .iter()
    .find(|entry| host.starts_with(entry.line_start) && mount.starts_with(entry.line_start))
  {
    Some(entry) => Ok((entry.line_start, line)),
    None => Err(line),
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
      entry
        .digest
        .get(..DIGEST_PREVIEW_CHARS)
        .unwrap_or(&entry.digest)
    }
  )
}

/// Compares a workload's host run with its mounted run on a route with no stated difference ([`compare_with`]).
pub fn compare(workload: &Workload, host: &Run, mount: &Run) -> WorkloadStatus {
  compare_with(workload, host, mount, &[])
}

/// Compares a workload's host run with its mounted run, under the stated differences of the route the mounted run took
/// ([`stated_differences`]). Exit codes come first: differing ones are a difference, and one nonzero code on both sides
/// is a failed workload, never an identical one, since a script that stopped early on both sides (a refused option, a
/// missing extension) proves nothing about the mount. Then the outputs, line by line, where a pair of lines that differ
/// and both start with one of the workload's stated starts is recorded rather than refused; a stated start none of
/// whose lines differed is a difference, so a route's list only shrinks. Then the trees, under the exclusions.
pub fn compare_with(
  workload: &Workload,
  host: &Run,
  mount: &Run,
  stated: &[StatedDifference],
) -> WorkloadStatus {
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
  if host.exit_code != 0 {
    return WorkloadStatus::Failed {
      exit_code: host.exit_code,
      detail: crate::exerciser::tail(&normalize_output(&host.output, &host.directory)),
    };
  }
  let stated_lines = match judged_output(workload, host, mount, stated) {
    Ok(lines) => lines,
    Err(difference) => return WorkloadStatus::Differs { detail: difference },
  };
  let host_tree = host.manifest.without(workload.excluded);
  let mount_tree = mount.manifest.without(workload.excluded);
  match first_manifest_difference(&host_tree, &mount_tree) {
    Some(difference) => WorkloadStatus::Differs { detail: difference },
    None if stated_lines.is_empty() => WorkloadStatus::Identical,
    None => WorkloadStatus::Stated {
      lines: stated_lines,
    },
  }
}

/// Two runs' outputs, each with its directory as the root token, judged under the workload's own stated differences:
/// the stated lines that differed, each a bounded detail, or the difference that fails the workload. That is the first
/// line differing otherwise, or a stated start none of whose lines differed.
fn judged_output(
  workload: &Workload,
  host: &Run,
  mount: &Run,
  stated: &[StatedDifference],
) -> Result<Vec<String>, String> {
  let mine: Vec<&StatedDifference> = stated
    .iter()
    .filter(|entry| entry.workload == workload.name)
    .collect();
  let stated_lines = output_difference(
    &normalize_output(&host.output, &host.directory),
    &normalize_output(&mount.output, &mount.directory),
    &mine,
  )?;
  match mine.iter().find(|entry| {
    !stated_lines
      .iter()
      .any(|(start, _)| *start == entry.line_start)
  }) {
    Some(unmet) => Err(detail(&format!(
      "a stated difference no longer differs: no line starting {:?} differed, so it comes off the route's list ({})",
      unmet.line_start, unmet.reason
    ))),
    None => Ok(stated_lines.into_iter().map(|(_, line)| line).collect()),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// AC-4.2 (workloads): npm prints a path with its UUID-shaped segments as `***`. Do normalize npm's
  /// `ls` line from a run directory whose name is a UUID; expect the root token, as for any tool.
  #[test]
  fn a_path_npm_redacted_normalizes_to_the_root_token() {
    let directory = "/tmp/claude/5bcbc28d-64b0-4c59-8918-79563c747af3/scratch/npm";
    let npm = "app@1.0.0 /private/tmp/claude/***/scratch/npm/app\n";
    assert_eq!(normalize_output(npm, directory), "app@1.0.0 <dir>/app\n");
    assert_eq!(
      normalize_output(
        "x /tmp/claude/5bcbc28d-64b0-4c59-8918-79563c747af3/scratch/npm/y",
        directory
      ),
      "x <dir>/y"
    );
    assert_eq!(redact_uuids("/a/not-a-uuid/b"), "/a/not-a-uuid/b");
  }

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

  /// The roster's workload named `name`.
  fn named(name: &str) -> Workload {
    *ROSTER
      .iter()
      .find(|workload| workload.name == name)
      .unwrap_or_else(|| panic!("no workload named {name}"))
  }

  /// Identical runs whose outputs differ only by their directory roots (including macOS's
  /// `/private` twin) compare identical; an excluded prefix hides a difference by design.
  #[test]
  fn identical_runs_compare_identical_under_the_exclusions() {
    let workload = named("git");
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
    let workload = named("rg");
    let host = run("/h", "same\n", vec![file("a", "aa")]);
    let mut mount = run("/m", "same\n", vec![file("a", "aa")]);
    mount.exit_code = 1;
    let detail = difference(compare(&workload, &host, &mount));
    assert!(detail.starts_with("exit code: host 0, mount 1"), "{detail}");
  }

  /// Part 6 "Real workloads" ("Exit codes 0"): do compare a host run and a mounted run that both exit 129 with the
  /// same output and the same tree, as a script does when its tool refuses an option on both sides; expect the
  /// workload judged failed, with the exit code and the host output's tail (its directory as the root token). Before
  /// 2026-11-17 it was judged identical: `compare` asked only that the two exit codes be equal.
  #[test]
  fn a_workload_that_fails_alike_on_both_sides_is_failed_not_identical() {
    let workload = named("git");
    let mut host = run("/h", "fatal: /h/x: no such option\n", vec![file("a", "aa")]);
    let mut mount = run("/m", "fatal: /m/x: no such option\n", vec![file("a", "aa")]);
    host.exit_code = 129;
    mount.exit_code = 129;
    assert_eq!(
      compare(&workload, &host, &mount),
      WorkloadStatus::Failed {
        exit_code: 129,
        detail: "fatal: <dir>/x: no such option".to_owned(),
      }
    );
  }

  /// A differing output line is named with its number.
  #[test]
  fn a_differing_output_line_is_named() {
    let workload = named("rg");
    let host = run("/h", "same\n", vec![file("a", "aa")]);
    let mount = run("/m", "other\n", vec![file("a", "aa")]);
    let detail = difference(compare(&workload, &host, &mount));
    assert!(detail.contains("output line 1"), "{detail}");
  }

  /// A-300 (the Windows lane). Do: normalize what Windows tools print about a workload directory given in its verbatim
  /// form, as the harness's canonical scratch is: Python's and Mercurial's `D:\…`, git's `D:/…`, a Subversion file URL,
  /// Git for Windows' shell `pwd` (`/d/…`), a lowercase drive letter, and the verbatim form itself. Expect each to be
  /// the root token, the drive's other folders left alone, and a trailing separator on the directory ignored. A POSIX
  /// directory and a drive's root gain no Windows spelling.
  #[test]
  fn a_windows_directory_is_the_root_token_in_every_spelling_its_tools_print() {
    let directory = r"\\?\D:\a\scratch\workloads-host\subversion";
    let printed = [
      r"repo D:\a\scratch\workloads-host\subversion\repo",
      "worktree D:/a/scratch/workloads-host/subversion/wt",
      "url file:///D:/a/scratch/workloads-host/subversion/repo/trunk",
      "pwd /d/a/scratch/workloads-host/subversion",
      r"lower d:\a\scratch\workloads-host\subversion\x",
      r"verbatim \\?\D:\a\scratch\workloads-host\subversion\y",
      r"other D:\a\scratch\elsewhere",
    ]
    .join("\n");
    assert_eq!(
      normalize_output(&printed, directory),
      [
        r"repo <dir>\repo",
        "worktree <dir>/wt",
        "url file:///<dir>/repo/trunk",
        "pwd <dir>",
        r"lower <dir>\x",
        r"verbatim <dir>\y",
        r"other D:\a\scratch\elsewhere",
      ]
      .join("\n")
    );
    assert_eq!(
      normalize_output(
        r"E:\conformance-7\workloads\git\.git",
        r"E:\conformance-7\workloads\git\"
      ),
      r"<dir>\.git"
    );
    assert!(windows_spellings("/h/git").is_empty());
    assert!(windows_spellings(r"D:\").is_empty() && windows_spellings("D:").is_empty());
    assert_eq!(normalize_output("/d/x /h/git", "/h/git"), "/d/x <dir>");
  }

  /// A host run and a WinFsp drive run of the git history workload: the drive's clone copied what NTFS's linked, so
  /// only its link count differs (A-300).
  fn clone_runs(mount_count: &str) -> (Run, Run) {
    let host = run(
      r"D:\h\git-history",
      "objects with two names after the clone: 9 of 9\nmerged D:/h/git-history/work\n",
      vec![file("work/a.txt", "aa")],
    );
    let mount = run(
      r"E:\m\git-history",
      &format!(
        "objects with two names after the clone: {mount_count} of 9\nmerged E:/m/git-history/work\n"
      ),
      vec![file("work/a.txt", "aa")],
    );
    (host, mount)
  }

  /// A-300. Do: compare the git history workload's NTFS run with its drive run, whose clone copied every object NTFS
  /// linked (`0 of 9` against `9 of 9`): on the drive's route, then as if on a route that states nothing. Expect the
  /// first identical but for the stated line, recorded with both sides, and the second a difference naming that line.
  #[test]
  fn the_drives_link_count_is_a_stated_difference_on_its_route_alone() {
    let workload = named("git-history");
    let (host, mount) = clone_runs("0");
    let on_the_drive = compare_with(
      &workload,
      &host,
      &mount,
      stated_differences(Transport::NativeWindowsWinfsp),
    );
    match &on_the_drive {
      WorkloadStatus::Stated { lines } => {
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
          lines[0].contains("9 of 9") && lines[0].contains("0 of 9"),
          "{lines:?}"
        );
      }
      other => panic!("expected the stated difference, got {other:?}"),
    }
    assert!(!on_the_drive.fails_the_suite());
    assert!(
      on_the_drive
        .stated_note("git-history")
        .is_some_and(|note| note.contains("0 of 9"))
    );
    let elsewhere = difference(compare_with(
      &workload,
      &host,
      &mount,
      stated_differences(Transport::NativeLinuxFuse),
    ));
    assert!(elsewhere.contains("output line 1"), "{elsewhere}");
  }

  /// A-300. Do: on the drive's route, compare runs that differ beyond the stated line: another line, a line missing on
  /// the drive, a tree entry, and a stated line on one side only. Expect each a difference: a stated line excuses only
  /// a pair of lines that both start with its start, and never the trees.
  #[test]
  fn a_stated_line_excuses_nothing_else() {
    let workload = named("git-history");
    let stated = stated_differences(Transport::NativeWindowsWinfsp);
    let (host, mut mount) = clone_runs("0");
    mount.output = mount.output.replace("merged", "MERGED");
    assert!(difference(compare_with(&workload, &host, &mount, stated)).contains("output line 2"));
    let (host, mut mount) = clone_runs("0");
    mount.output = "objects with two names after the clone: 0 of 9\n".to_owned();
    assert!(difference(compare_with(&workload, &host, &mount, stated)).contains("<end>"));
    let (host, mut mount) = clone_runs("0");
    mount.manifest = Manifest {
      entries: vec![file("work/a.txt", "bb")],
    };
    assert!(difference(compare_with(&workload, &host, &mount, stated)).starts_with("tree entry"));
    let (host, mut mount) = clone_runs("0");
    mount.output = mount
      .output
      .replace("objects with two names", "objects with one name");
    assert!(difference(compare_with(&workload, &host, &mount, stated)).contains("output line 1"));
  }

  /// A-300 ("the list only shrinks"). Do: on the drive's route, compare runs whose stated line did not differ, as when
  /// a drive makes links. Expect a difference saying the stated difference no longer differs, so it is struck from
  /// the list rather than left to excuse a later regression.
  #[test]
  fn a_stated_difference_that_no_longer_differs_fails() {
    let workload = named("git-history");
    let (host, mount) = clone_runs("9");
    let detail = difference(compare_with(
      &workload,
      &host,
      &mount,
      stated_differences(Transport::NativeWindowsWinfsp),
    ));
    assert!(
      detail.starts_with("a stated difference no longer differs"),
      "{detail}"
    );
    assert_eq!(compare(&workload, &host, &mount), WorkloadStatus::Identical);
  }

  /// AC-3.2 / AC-4.2: every visible name participates in the comparison, including `._` names.
  /// A transport-created sidecar changes the tree; an application can also own such a name.
  #[test]
  fn dot_underscore_names_are_compared_like_other_visible_files() {
    let workload = named("rg");
    let host = run("/h", "same\n", vec![file("a", "aa")]);
    let mount = run(
      "/m",
      "same\n",
      vec![file("._a", "sidecar"), file("a", "aa")],
    );
    assert!(difference(compare(&workload, &host, &mount)).starts_with("tree entry"));
    let host = run("/h", "same\n", vec![file("._user-file", "original")]);
    let mount = run("/m", "same\n", vec![file("._user-file", "changed")]);
    assert!(difference(compare(&workload, &host, &mount)).starts_with("tree entry"));
    let host = run("/h", "same\n", vec![file("a", "aa")]);
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
    let workload = named("rg");
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
      "git",
      "git-history",
      "mercurial",
      "subversion",
      "cargo",
      "npm",
      "python",
      "rg",
      "rsync",
      "sqlite",
      "editor",
      "watcher",
    ] {
      assert!(names.contains(&expected), "{expected}");
    }
  }

  /// The goal of 2026-11-18 (the guest's environment): do source [`environment_exports`] in a real POSIX shell and
  /// print each variable back; expect every value exactly as [`ENVIRONMENT`] holds it, the empty `HGRCPATH` set but
  /// empty, and `HGUSER`'s spaces and angle brackets intact. A value with a single quote, double quotes and a `$`
  /// survives [`export_line`] too.
  #[cfg(unix)]
  #[test]
  fn the_environment_exports_reach_a_shell_exactly() {
    let mut script = environment_exports();
    script.push_str(&export_line("QUOTED", "it's \"here\" $HOME"));
    for (key, _) in ENVIRONMENT {
      script.push_str(&format!("printf '%s=[%s]\\n' {key} \"${{{key}-unset}}\"\n"));
    }
    script.push_str("printf 'QUOTED=[%s]\\n' \"$QUOTED\"\n");
    let output = std::process::Command::new("sh")
      .args(["-c", &script])
      .env_clear()
      .output()
      .unwrap();
    assert!(output.status.success(), "{output:?}");
    let printed = String::from_utf8(output.stdout).unwrap();
    let mut expected: String = ENVIRONMENT
      .iter()
      .map(|(key, value)| format!("{key}=[{value}]\n"))
      .collect();
    expected.push_str("QUOTED=[it's \"here\" $HOME]\n");
    assert_eq!(printed, expected);
    assert!(printed.contains("HGRCPATH=[]\n"), "{printed}");
  }
}
