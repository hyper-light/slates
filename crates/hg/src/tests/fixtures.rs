//! The golden vectors (A-514): the bundles stock Mercurial 7.2 made of one repository, as changegroup versions `01`
//! (a bundle1) and `02` to `04` (bundle2 streams with the changegroup, `cache:rev-branch-cache`, `obsmarkers` and
//! `phase-heads` parts), with 7.2's own `hg debugbundle` listing of each and the repository's log. Made by
//! `fixtures/make-fixtures.sh` (its header names the exact command and the machine), compiled in, never read at run
//! time.

use crate::Node;

/// The bundle1 (`hg bundle --all --type none-v1`).
pub(crate) const CG01: &[u8] = include_bytes!("../../fixtures/cg01.hg");
/// The bundle2 with changegroup 02 (`none-v2;cg.version=02;phases=true;obsolescence=true`).
pub(crate) const CG02: &[u8] = include_bytes!("../../fixtures/cg02.hg");
/// The same with changegroup 03.
pub(crate) const CG03: &[u8] = include_bytes!("../../fixtures/cg03.hg");
/// The same with changegroup 04.
pub(crate) const CG04: &[u8] = include_bytes!("../../fixtures/cg04.hg");
/// 7.2's `debugbundle --all` of [`CG01`].
pub(crate) const CG01_LISTING: &str = include_str!("../../fixtures/cg01.hg.debug");
/// 7.2's `debugbundle --all` of [`CG02`].
pub(crate) const CG02_LISTING: &str = include_str!("../../fixtures/cg02.hg.debug");
/// 7.2's `debugbundle --all` of [`CG03`]: the changelog and the manifest only (7.2's listing stops at the empty tree
/// list).
pub(crate) const CG03_LISTING: &str = include_str!("../../fixtures/cg03.hg.debug");
/// 7.2's plain `debugbundle` of [`CG04`]: the changesets' nodes (its `--all` fails on version 04's full texts).
pub(crate) const CG04_LISTING: &str = include_str!("../../fixtures/cg04.hg.debug");
/// `hg log -T '{rev} {node} {phase} {branch} {p1node} {p2node} {desc|firstline}\n'`, newest first.
pub(crate) const LOG: &str = include_str!("../../fixtures/log.txt");

/// Every file revision's text, in the order its group holds them, as `make-fixtures.sh` wrote the files (and
/// `texts.txt`, `hg cat`'s record, holds them). `c` is a copy of `b`, so its text begins with Mercurial's metadata
/// block naming the source and the source's file node.
pub(crate) const FILE_TEXTS: &[(&str, &[&[u8]])] = &[
  ("a", &[b"a\n", b"a\nb\n", b"a\nb\nc\n"]),
  ("b", &[b"x\n"]),
  (
    "c",
    &[b"\x01\ncopy: b\ncopyrev: 1406e74118627694268417491f018a4a883152f0\n\x01\nx\n"],
  ),
  ("dir/f", &[b"deep\n", b"deep\nmore, amended\n"]),
];

/// One revision as 7.2's `debugbundle --all` lists it: "id, p1, p2, cset, delta base, len(delta)".
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Row {
  pub(crate) node: Node,
  pub(crate) p1: Node,
  pub(crate) p2: Node,
  pub(crate) link: Node,
  pub(crate) base: Node,
  pub(crate) len: usize,
}

/// A node from its 40 hexadecimal digits.
pub(crate) fn node(hex: &str) -> Node {
  assert_eq!(hex.len(), 40, "a node is 40 hex digits: {hex}");
  let (pairs, _) = hex.as_bytes().as_chunks::<2>();
  let mut out = [0u8; 20];
  for (byte, pair) in out.iter_mut().zip(pairs) {
    let digits = core::str::from_utf8(pair).unwrap();
    *byte = u8::from_str_radix(digits, 16).unwrap();
  }
  out
}

/// A node's 40 hexadecimal digits.
pub(crate) fn hex(node: &Node) -> String {
  node.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The groups of the changegroup a listing shows, in order: each group's name ("changelog", "manifest", a file's
/// path) and its rows.
pub(crate) fn listing(text: &str) -> Vec<(String, Vec<Row>)> {
  let mut groups: Vec<(String, Vec<Row>)> = Vec::new();
  let mut in_changegroup = !text.contains(" -- ");
  for line in text.lines() {
    let line = line.trim();
    if line.contains(" -- ") {
      in_changegroup = line.starts_with("changegroup -- ");
      continue;
    }
    if !in_changegroup || line.is_empty() || line.starts_with("format:") {
      continue;
    }
    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() == 6 && fields[0].len() == 40 {
      groups.last_mut().unwrap().1.push(Row {
        node: node(fields[0]),
        p1: node(fields[1]),
        p2: node(fields[2]),
        link: node(fields[3]),
        base: node(fields[4]),
        len: fields[5].parse().unwrap(),
      });
    } else {
      groups.push((line.to_string(), Vec::new()));
    }
  }
  groups
}

/// The part lines of a bundle2 listing: "<type> -- {<key>: <value>, ...} (mandatory: <True|False>)".
pub(crate) fn part_lines(text: &str) -> Vec<String> {
  text
    .lines()
    .filter(|line| line.contains(" -- ") && line.contains("(mandatory: "))
    .map(str::to_string)
    .collect()
}

/// The changeset nodes of a plain `debugbundle` listing (version 04's).
pub(crate) fn plain_nodes(text: &str) -> Vec<Node> {
  let mut out = Vec::new();
  let mut in_changegroup = false;
  for line in text.lines() {
    let line = line.trim();
    if line.contains(" -- ") {
      in_changegroup = line.starts_with("changegroup -- ");
    } else if in_changegroup && line.len() == 40 {
      out.push(node(line));
    }
  }
  out
}

/// The changesets' first description lines, oldest first, from [`LOG`] (whose newest-first lines end with them).
pub(crate) fn descriptions() -> Vec<String> {
  let mut out: Vec<String> = LOG
    .lines()
    .map(|line| line.splitn(7, ' ').nth(6).unwrap().to_string())
    .collect();
  out.reverse();
  out
}

/// The part payloads' golden vectors (A-545): what 7.2's client sent with `unbundle` in three pushes over SSH, the
/// server's replies and its answers to the `listkeys phases` that followed each, 7.2's `debugbundle --all` of each
/// stream, two obsstores 7.2 wrote, and the logs and server state they are judged by. Made by
/// `fixtures/payloads/make-payload-fixtures.sh` (its header names the exact command).
pub(crate) mod payloads {
  /// The modern push: bookmarks, check:bookmarks, check:phases, check:updated-heads, changegroup 03, obsmarkers.
  pub(crate) const PUSH_MODERN: &[u8] = include_bytes!("../../fixtures/payloads/push-modern.hg");
  /// The server's reply to it.
  pub(crate) const PUSH_MODERN_REPLY: &[u8] =
    include_bytes!("../../fixtures/payloads/push-modern-reply.hg");
  /// The legacy push: a bookmark and a phase moved by pushkey parts.
  pub(crate) const PUSH_LEGACY: &[u8] = include_bytes!("../../fixtures/payloads/push-legacy.hg");
  /// The server's reply to it.
  pub(crate) const PUSH_LEGACY_REPLY: &[u8] =
    include_bytes!("../../fixtures/payloads/push-legacy-reply.hg");
  /// The push to the strict server: check:heads.
  pub(crate) const PUSH_STRICT: &[u8] = include_bytes!("../../fixtures/payloads/push-strict.hg");
  /// The server's reply to it.
  pub(crate) const PUSH_STRICT_REPLY: &[u8] =
    include_bytes!("../../fixtures/payloads/push-strict-reply.hg");
  /// 7.2's `debugbundle --all` of the legacy push.
  pub(crate) const PUSH_LEGACY_LISTING: &str =
    include_str!("../../fixtures/payloads/push-legacy.hg.debug");
  /// 7.2's `debugbundle --all` of the modern push's reply.
  pub(crate) const PUSH_MODERN_REPLY_LISTING: &str =
    include_str!("../../fixtures/payloads/push-modern-reply.hg.debug");
  /// 7.2's `debugbundle --all` of the legacy push's reply.
  pub(crate) const PUSH_LEGACY_REPLY_LISTING: &str =
    include_str!("../../fixtures/payloads/push-legacy-reply.hg.debug");
  /// An obsstore 7.2 wrote in format 0 (`format.obsstore-version=0`).
  pub(crate) const OBSSTORE_V0: &[u8] = include_bytes!("../../fixtures/payloads/obsstore-v0.bin");
  /// The same markers 7.2 wrote in format 1.
  pub(crate) const OBSSTORE_V1: &[u8] = include_bytes!("../../fixtures/payloads/obsstore-v1.bin");
  /// 7.2's `debugobsolete` of each (the two listings are the same bytes).
  pub(crate) const OBSSTORE_LISTING: &str =
    include_str!("../../fixtures/payloads/obsstore-v1.debug");
  /// The format 0 listing.
  pub(crate) const OBSSTORE_V0_LISTING: &str =
    include_str!("../../fixtures/payloads/obsstore-v0.debug");
  /// The client's log after the three pushes, newest first: rev, node, phase, description.
  pub(crate) const CLIENT_LOG: &str = include_str!("../../fixtures/payloads/client-log-2.txt");
  /// The server's bookmarks before the first push (`debugpushkey`).
  pub(crate) const SERVER_BOOKMARKS_BEFORE: &str =
    include_str!("../../fixtures/payloads/server-bookmarks-before.txt");
  /// The namespaces the server serves (`debugpushkey`).
  pub(crate) const SERVER_NAMESPACES: &str =
    include_str!("../../fixtures/payloads/server-namespaces.txt");
  /// The server's answer to the `listkeys phases` the client sent after the modern push, its SSH v1 framing removed.
  pub(crate) const PUSH_MODERN_LISTKEYS_PHASES: &[u8] =
    include_bytes!("../../fixtures/payloads/push-modern-listkeys-phases.bin");
  /// The same after the legacy push.
  pub(crate) const PUSH_LEGACY_LISTKEYS_PHASES: &[u8] =
    include_bytes!("../../fixtures/payloads/push-legacy-listkeys-phases.bin");
  /// The same after the strict push.
  pub(crate) const PUSH_STRICT_LISTKEYS_PHASES: &[u8] =
    include_bytes!("../../fixtures/payloads/push-strict-listkeys-phases.bin");
}

/// The wire protocol's golden vectors (A-569): what stock 7.2's clients sent and its servers answered over SSH and
/// HTTP, each file as `fixtures/wire/make-wire-fixtures.sh` wrote it (its header names the exact command), compiled
/// in by name.
pub(crate) mod wire {
  /// Lists each capture's name with its bytes.
  macro_rules! captures {
    ($($name:literal),* $(,)?) => {
      /// Every capture, by its file name.
      const FILES: &[(&str, &[u8])] = &[$(($name, include_bytes!(concat!("../../fixtures/wire/", $name)))),*];
    };
  }

  captures![
    "http-clone.01.request",
    "http-clone.01.response-body",
    "http-clone.01.response-head",
    "http-clone.02.request",
    "http-clone.02.response-body",
    "http-clone.02.response-head",
    "http-clone.03.request",
    "http-clone.03.response-body",
    "http-clone.03.response-head",
    "http-none.01.request",
    "http-none.01.response-body",
    "http-none.01.response-head",
    "http-none.02.request",
    "http-none.02.response-body",
    "http-none.02.response-head",
    "http-none.03.request",
    "http-none.03.response-body",
    "http-none.03.response-head",
    "http-oob.01.request",
    "http-oob.01.response-body",
    "http-oob.01.response-head",
    "http-oob.02.request",
    "http-oob.02.response-body",
    "http-oob.02.response-head",
    "http-oob.03.request",
    "http-oob.03.response-body",
    "http-oob.03.response-head",
    "http-oob.04.request",
    "http-oob.04.response-body",
    "http-oob.04.response-head",
    "http-oob.client-stderr",
    "http-postargs.01.request",
    "http-postargs.01.response-body",
    "http-postargs.01.response-head",
    "http-postargs.02.request",
    "http-postargs.02.response-body",
    "http-postargs.02.response-head",
    "http-postargs.03.request",
    "http-postargs.03.response-body",
    "http-postargs.03.response-head",
    "http-postargs.answer",
    "http-push.01.request",
    "http-push.01.response-body",
    "http-push.01.response-head",
    "http-push.02.request",
    "http-push.02.response-body",
    "http-push.02.response-head",
    "http-push.03.request",
    "http-push.03.response-body",
    "http-push.03.response-head",
    "http-push.04.request",
    "http-push.04.response-body",
    "http-push.04.response-head",
    "http-push.05.request",
    "http-push.05.response-body",
    "http-push.05.response-head",
    "http-push.06.request",
    "http-push.06.response-body",
    "http-push.06.response-head",
    "http-push.07.request",
    "http-push.07.response-body",
    "http-push.07.response-head",
    "http-push.08.request",
    "http-push.08.response-body",
    "http-push.08.response-head",
    "http-query.01.request",
    "http-query.01.response-body",
    "http-query.01.response-head",
    "http-query.02.request",
    "http-query.02.response-body",
    "http-query.02.response-head",
    "http-query.03.request",
    "http-query.03.response-body",
    "http-query.03.response-head",
    "http-query.answer",
    "http-wireargs.01.request",
    "http-wireargs.01.response-body",
    "http-wireargs.01.response-head",
    "http-wireargs.02.request",
    "http-wireargs.02.response-body",
    "http-wireargs.02.response-head",
    "http-wireargs.03.request",
    "http-wireargs.03.response-body",
    "http-wireargs.03.response-head",
    "http-wireargs.answer",
    "server-log.txt",
    "ssh-clone.client",
    "ssh-clone.server",
    "ssh-clone.stderr",
    "ssh-oob.client",
    "ssh-oob.client-stderr",
    "ssh-oob.server",
    "ssh-oob.stderr",
    "ssh-pull.client",
    "ssh-pull.server",
    "ssh-pull.stderr",
    "ssh-push.client",
    "ssh-push.server",
    "ssh-push.stderr",
    "ssh-wireargs.answer",
    "ssh-wireargs.client",
    "ssh-wireargs.server",
    "ssh-wireargs.stderr",
  ];

  /// The capture named `name`.
  pub(crate) fn file(name: &str) -> &'static [u8] {
    FILES
      .iter()
      .find(|(known, _)| *known == name)
      .map(|(_, bytes)| *bytes)
      .unwrap_or_else(|| panic!("no capture {name}"))
  }
}

/// The objects' golden vectors (A-588): every revision of one repository stock 7.2 stored, each log's index as 7.2's
/// `debugindex --debug` lists it, every revision's text as its `debugdata` writes it, and what 7.2 reads from each, as
/// `fixtures/objects/make-object-fixtures.sh` wrote them (its header names the exact command and the run), compiled in
/// by name.
pub(crate) mod objects {
  /// Lists each file's name with its bytes.
  macro_rules! stored {
    ($($name:literal),* $(,)?) => {
      /// Every file, by its name.
      const FILES: &[(&str, &[u8])] = &[$(($name, include_bytes!(concat!("../../fixtures/objects/", $name)))),*];
    };
  }

  stored![
    "changeset-0.txt",
    "changeset-1.txt",
    "changeset-10.txt",
    "changeset-2.txt",
    "changeset-3.txt",
    "changeset-4.txt",
    "changeset-5.txt",
    "changeset-6.txt",
    "changeset-7.txt",
    "changeset-8.txt",
    "changeset-9.txt",
    "expected.txt",
    "file-a-0.txt",
    "file-a-1.txt",
    "file-c-0.txt",
    "file-c-1.txt",
    "file-d-0.txt",
    "file-e-0.txt",
    "file-link-0.txt",
    "file-meta-0.txt",
    "file-run-0.txt",
    "file-u-0.txt",
    "index-changelog.txt",
    "index-file-a.txt",
    "index-file-c.txt",
    "index-file-d.txt",
    "index-file-e.txt",
    "index-file-link.txt",
    "index-file-meta.txt",
    "index-file-run.txt",
    "index-file-u.txt",
    "index-manifest.txt",
    "log.txt",
    "manifest-0.txt",
    "manifest-1.txt",
    "manifest-2.txt",
    "manifest-3.txt",
    "manifest-4.txt",
    "manifest-5.txt",
    "manifest-6.txt",
    "manifest-7.txt",
    "manifest-8.txt",
  ];

  /// The file named `name`.
  pub(crate) fn file(name: &str) -> &'static [u8] {
    FILES
      .iter()
      .find(|(known, _)| *known == name)
      .map(|(_, bytes)| *bytes)
      .unwrap_or_else(|| panic!("no stored file {name}"))
  }

  /// Every file's name, in order.
  pub(crate) fn names() -> impl Iterator<Item = &'static str> {
    FILES.iter().map(|(name, _)| *name)
  }
}

/// The revlog golden vectors (A-611): one history stock 7.2 stored in five repositories (zstd, zlib, split, revlog v2,
/// changelog v2), each revlog's files as the store holds them, its `debugindex --debug` and `debugdeltachain`, and every
/// revision's `debugdata` once, as `fixtures/revlogs/make-revlog-fixtures.sh` wrote them (its header names the exact
/// command and the run), compiled in by name.
pub(crate) mod revlogs {
  /// Lists each file's name with its bytes.
  macro_rules! stored {
    ($($name:literal),* $(,)?) => {
      /// Every file, by its name.
      const FILES: &[(&str, &[u8])] = &[$(($name, include_bytes!(concat!("../../fixtures/revlogs/", $name)))),*];
    };
  }

  stored![
    "00changelog-0.txt",
    "00changelog-1.txt",
    "00changelog-10.txt",
    "00changelog-11.txt",
    "00changelog-12.txt",
    "00changelog-2.txt",
    "00changelog-3.txt",
    "00changelog-4.txt",
    "00changelog-5.txt",
    "00changelog-6.txt",
    "00changelog-7.txt",
    "00changelog-8.txt",
    "00changelog-9.txt",
    "00manifest-0.txt",
    "00manifest-1.txt",
    "00manifest-10.txt",
    "00manifest-11.txt",
    "00manifest-12.txt",
    "00manifest-2.txt",
    "00manifest-3.txt",
    "00manifest-4.txt",
    "00manifest-5.txt",
    "00manifest-6.txt",
    "00manifest-7.txt",
    "00manifest-8.txt",
    "00manifest-9.txt",
    "clv2-00changelog.chain",
    "clv2-00changelog.dat",
    "clv2-00changelog.i",
    "clv2-00changelog.idx",
    "clv2-00changelog.index",
    "clv2-00changelog.sda",
    "clv2-00manifest.chain",
    "clv2-00manifest.dat",
    "clv2-00manifest.i",
    "clv2-00manifest.idx",
    "clv2-00manifest.index",
    "clv2-00manifest.sda",
    "clv2-data_empty.dat",
    "clv2-data_empty.i",
    "clv2-data_empty.idx",
    "clv2-data_empty.sda",
    "clv2-data_grow.dat",
    "clv2-data_grow.i",
    "clv2-data_grow.idx",
    "clv2-data_grow.sda",
    "clv2-data_rand.dat",
    "clv2-data_rand.i",
    "clv2-data_rand.idx",
    "clv2-data_rand.sda",
    "clv2-empty.chain",
    "clv2-empty.index",
    "clv2-grow.chain",
    "clv2-grow.index",
    "clv2-rand.chain",
    "clv2-rand.index",
    "clv2-requires.txt",
    "empty-0.txt",
    "grow-0.txt",
    "grow-1.txt",
    "grow-10.txt",
    "grow-11.txt",
    "grow-2.txt",
    "grow-3.txt",
    "grow-4.txt",
    "grow-5.txt",
    "grow-6.txt",
    "grow-7.txt",
    "grow-8.txt",
    "grow-9.txt",
    "log.txt",
    "rand-0.txt",
    "rand-1.txt",
    "split-00changelog.chain",
    "split-00changelog.d",
    "split-00changelog.i",
    "split-00changelog.index",
    "split-00manifest.chain",
    "split-00manifest.d",
    "split-00manifest.i",
    "split-00manifest.index",
    "split-data_empty.i",
    "split-data_grow.d",
    "split-data_grow.i",
    "split-data_rand.d",
    "split-data_rand.i",
    "split-empty.chain",
    "split-empty.index",
    "split-grow.chain",
    "split-grow.index",
    "split-rand.chain",
    "split-rand.index",
    "split-requires.txt",
    "v2-00changelog.chain",
    "v2-00changelog.dat",
    "v2-00changelog.i",
    "v2-00changelog.idx",
    "v2-00changelog.index",
    "v2-00changelog.sda",
    "v2-00manifest.chain",
    "v2-00manifest.dat",
    "v2-00manifest.i",
    "v2-00manifest.idx",
    "v2-00manifest.index",
    "v2-00manifest.sda",
    "v2-data_empty.dat",
    "v2-data_empty.i",
    "v2-data_empty.idx",
    "v2-data_empty.sda",
    "v2-data_grow.dat",
    "v2-data_grow.i",
    "v2-data_grow.idx",
    "v2-data_grow.sda",
    "v2-data_rand.dat",
    "v2-data_rand.i",
    "v2-data_rand.idx",
    "v2-data_rand.sda",
    "v2-empty.chain",
    "v2-empty.index",
    "v2-grow.chain",
    "v2-grow.index",
    "v2-rand.chain",
    "v2-rand.index",
    "v2-requires.txt",
    "zlib-00changelog.chain",
    "zlib-00changelog.d",
    "zlib-00changelog.i",
    "zlib-00changelog.index",
    "zlib-00manifest.chain",
    "zlib-00manifest.i",
    "zlib-00manifest.index",
    "zlib-data_empty.i",
    "zlib-data_grow.i",
    "zlib-data_rand.i",
    "zlib-empty.chain",
    "zlib-empty.index",
    "zlib-grow.chain",
    "zlib-grow.index",
    "zlib-rand.chain",
    "zlib-rand.index",
    "zlib-requires.txt",
    "zstd-00changelog.chain",
    "zstd-00changelog.d",
    "zstd-00changelog.i",
    "zstd-00changelog.index",
    "zstd-00manifest.chain",
    "zstd-00manifest.i",
    "zstd-00manifest.index",
    "zstd-data_empty.i",
    "zstd-data_grow.i",
    "zstd-data_rand.i",
    "zstd-empty.chain",
    "zstd-empty.index",
    "zstd-grow.chain",
    "zstd-grow.index",
    "zstd-rand.chain",
    "zstd-rand.index",
    "zstd-requires.txt",
  ];

  /// The file named `name`, if there is one.
  pub(crate) fn find(name: &str) -> Option<&'static [u8]> {
    FILES
      .iter()
      .find(|(known, _)| *known == name)
      .map(|(_, bytes)| *bytes)
  }

  /// The file named `name`.
  pub(crate) fn file(name: &str) -> &'static [u8] {
    find(name).unwrap_or_else(|| panic!("no revlog fixture {name}"))
  }
}

/// The store golden vectors (A-630): one history stock 7.2 stored twice, in a dotencode store and a hybrid one, each
/// store's files whole, its file list, requirements and paths, and every revision's `debugdata` once, as
/// `fixtures/store/make-store-fixtures.sh` wrote them (its header names the exact command and the run), compiled in
/// by name.
pub(crate) mod store {
  /// Lists each file's name with its bytes.
  macro_rules! stored {
    ($($name:literal),* $(,)?) => {
      /// Every file, by its name.
      const FILES: &[(&str, &[u8])] = &[$(($name, include_bytes!(concat!("../../fixtures/store/", $name)))),*];
    };
  }

  stored![
    "00changelog-0.txt",
    "00changelog-1.txt",
    "00changelog-2.txt",
    "00changelog-3.txt",
    "00changelog-4.txt",
    "00manifest-0.txt",
    "00manifest-1.txt",
    "00manifest-2.txt",
    "00manifest-3.txt",
    "00manifest-4.txt",
    "dot-files.txt",
    "dot-paths.txt",
    "dot-requires.txt",
    "dot/00changelog.d",
    "dot/00changelog.i",
    "dot/00manifest.i",
    "dot/data/_capital___under.txt.i",
    "dot/data/_f_o_o.i",
    "dot/data/au~78.txt.i",
    "dot/data/colon~3aquest~3fstar~2a.txt.i",
    "dot/data/co~6d1.i",
    "dot/data/co~6e.i",
    "dot/data/foo.i",
    "dot/data/lp~749.x.i",
    "dot/data/plain.txt.i",
    "dot/data/pr~6e.d.i",
    "dot/data/quote~22lt~3cgt~3epipe~7cback~5cslash.txt.i",
    "dot/data/tilde~7e~c3~a9.txt.i",
    "dot/data/trailing .i",
    "dot/data/trailing..i",
    "dot/data/x.i.hg/inside.txt.i",
    "dot/data/y.d.hg/inside.txt.i",
    "dot/data/z.hg.hg/inside.txt.i",
    "dot/data/~20space.i",
    "dot/data/~2eleading.i",
    "dot/dh/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaab1068971bbd8477250f462eb099cbb09d78312dc.i",
    "dot/dh/director/seventh_/third.di/fourth.d/fifth.di/sixth.di/eighth.d/deep-file-na8045308eb5772f8d366bd883527c27741dfa75db.i",
    "dot/dh/upper_zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz.txt.i20053374f434dbfe7d9cbc451df115259a188c19.i",
    "dot/fncache",
    "dot/requires",
    "f0-0.txt",
    "f1-0.txt",
    "f10-0.txt",
    "f11-0.txt",
    "f12-0.txt",
    "f13-0.txt",
    "f13-1.txt",
    "f14-0.txt",
    "f15-0.txt",
    "f16-0.txt",
    "f17-0.txt",
    "f18-0.txt",
    "f19-0.txt",
    "f2-0.txt",
    "f20-0.txt",
    "f21-0.txt",
    "f3-0.txt",
    "f4-0.txt",
    "f5-0.txt",
    "f5-1.txt",
    "f6-0.txt",
    "f7-0.txt",
    "f8-0.txt",
    "f9-0.txt",
    "hybrid-files.txt",
    "hybrid-paths.txt",
    "hybrid-requires.txt",
    "hybrid/00changelog.d",
    "hybrid/00changelog.i",
    "hybrid/00manifest.i",
    "hybrid/data/ space.i",
    "hybrid/data/.leading.i",
    "hybrid/data/_capital___under.txt.i",
    "hybrid/data/_f_o_o.i",
    "hybrid/data/au~78.txt.i",
    "hybrid/data/colon~3aquest~3fstar~2a.txt.i",
    "hybrid/data/co~6d1.i",
    "hybrid/data/co~6e.i",
    "hybrid/data/foo.i",
    "hybrid/data/lp~749.x.i",
    "hybrid/data/plain.txt.i",
    "hybrid/data/pr~6e.d.i",
    "hybrid/data/quote~22lt~3cgt~3epipe~7cback~5cslash.txt.i",
    "hybrid/data/tilde~7e~c3~a9.txt.i",
    "hybrid/data/trailing .i",
    "hybrid/data/trailing..i",
    "hybrid/data/x.i.hg/inside.txt.i",
    "hybrid/data/y.d.hg/inside.txt.i",
    "hybrid/data/z.hg.hg/inside.txt.i",
    "hybrid/dh/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaab1068971bbd8477250f462eb099cbb09d78312dc.i",
    "hybrid/dh/director/seventh_/third.di/fourth.d/fifth.di/sixth.di/eighth.d/deep-file-na8045308eb5772f8d366bd883527c27741dfa75db.i",
    "hybrid/dh/upper_zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz.txt.i20053374f434dbfe7d9cbc451df115259a188c19.i",
    "hybrid/fncache",
    "hybrid/requires",
  ];

  /// The file named `name`, if there is one.
  pub(crate) fn find(name: &str) -> Option<&'static [u8]> {
    FILES.iter().find(|(known, _)| *known == name).map(|(_, bytes)| *bytes)
  }

  /// The file named `name`.
  pub(crate) fn file(name: &str) -> &'static [u8] {
    find(name).unwrap_or_else(|| panic!("no store fixture {name}"))
  }
}
