# A Mercurial server on slates' operations-based merge: the reading notes behind §4.17.3 (A-499)

Status: complete for the design piece (g291, 2026-11-18, kiln UI goal 91). Every fact below was read on 2026-11-18 from
a primary source, cited to its file. Evidence tiers as in the design's §0.3: Mercurial's own documents (its `helptext`)
are **[B]**, its source **[C]**. Nothing here is from memory; where Mercurial's documents and its code disagree, both
are cited and the disagreement is stated (§3.3).

## 0. The sources and how they were read

- **Mercurial 7.2** (the version the workload roster pins, A-289), read from its Homebrew installation on the MacBook
  (`/opt/homebrew/Cellar/mercurial/7.2/lib/python3.14/site-packages/`), read only:
  - `mercurial/helptext/internals/`: `wireprotocol.txt`, `bundle2.txt`, `bundles.txt`, `changegroups.txt`,
    `revlogs.txt`, `linelog.txt` (and `wireprotocolv2.txt`, `wireprotocolrpc.txt`, read for their status only);
  - `mercurial/helptext/`: `phases.txt`, `evolution.txt`, `config.txt` (the `hooks`, `server` and `web` sections),
    `hgweb.txt`;
  - code: `wireprotov1server.py` (the command table), `wireprotoserver.py` (the HTTP and SSH servers),
    `wireprototypes.py` (getbundle's argument types), `exchanges/bundle_caps.py` (the bundle2 capabilities),
    `bundle2_part_handlers.py` (what a receiver accepts), `exchange.py` (getbundle's parts, the push race check),
    `changegroup.py` (the changegroup versions), `obsolete.py`, `phases.py`, `bookmarks.py`, `pushkey.py`,
    `streamclone.py`, `hgweb/common.py` (the permission checks), `hgweb/webcommands.py`, `scmutil.py`,
    `requirements.py`;
  - extensions shipped with core (`hgext/`): `narrow/`, `remotefilelog/`, `largefiles/proto.py`,
    `lfs/wireprotolfsserver.py`, `acl.py`, `fastannotate/`.
- **hg-evolve 12.0.0** (`testedwith = b'6.7 6.8 6.9 7.0 7.1 7.2'`), the topic and evolve extensions, from its PyPI
  source distribution: `hgext3rd/topic/__init__.py`, `topic/server.py`, `topic/discovery.py`,
  `evolve/obsexchange.py`, `evolve/obsdiscovery.py`. Read with `tar -O` into the terminal; the archive was deleted
  after.
- **Mercurial 7.2's test list**, from the `tests/` names in the PyPI source distribution
  (`mercurial-7.2.tar.gz`), listed by streaming the archive through `tar t` with nothing unpacked. The repository
  pages at repo.mercurial-scm.org and foss.heptapod.net answer automated clients with a bot check, so the sdist was the
  route. The 247 wire-relevant names are in §9.

## 1. The wire protocol (`internals.wireprotocol` [B]; `wireprotov1server.py`, `wireprotoserver.py` [C])

- **Shape.** "Each request is modeled as a command name, a dictionary of arguments, and optional raw input." "The
  protocol is synchronous and does not support multiplexing (concurrent commands)."
- **HTTP version 1.** The command is the `cmd` query parameter on the repository's URL, `GET` or `POST`. Arguments
  travel in the query string, in numbered `X-HgArg-<N>` headers (when the server advertises `httpheader=<max header
  length>`; "defaults to 1024"), or in the body of a `POST` with `X-HgArgs-Post: <length of the arguments>` when the
  server advertises `httppostargs`. Request and response media types: `application/mercurial-0.1` (raw),
  `application/mercurial-0.2` ("The first byte of the payload indicates the length of the compression format
  identifier that follows. Next are N bytes indicating the compression format ... The remaining bytes are compressed"),
  `application/hg-error`. The client states what it can read in `X-HgProto-<N>`: `0.1`, `0.2`, `comp=<list>`; with
  no `comp`, the server reads `zlib,none`. `getbundle`'s answer on version 1 "is zlib compressed".
- **Response type chosen by the server** (`_httpresponsetype` [C]): with `0.2` offered, the first engine in the
  server's order that the client named; `prefer_uncompressed` answers `none`; otherwise `0.1` with zlib at
  `server.zliblevel`. The code's comment: settings are never taken from an untrusted configuration "because disabling
  compression or setting a very high compression level could lead to flooding the server's network or CPU".
- **Permissions** (`hgweb/common.py` `checkauthz` [C]): `deny_read`, then `allow_read` (both 401 "read not authorized"),
  `allow-pull` (401); for a push, a `POST` is required (405), `push_ssl` requires https (403 "ssl required"),
  `deny_push`, then `allow-push` (401 "push not authorized"). Every command carries a permission, `pull` or `push`
  (`wireprotocommand(..., permission=)`); `pushkey` and `unbundle` are `push`, every other command `pull`.
- **HTTP version 2** (`/api/*`, `hgrpc` frames) is documented as "Experimental - feature under active development".
  The 7.2 HTTP server has no handler for it: `wireprotoserver.py` holds no reference to `X-HgUpgrade` or `apibase`, and
  there is no `wireprotov2server` module. A 7.2 server does not offer it, so neither does slates.
- **SSH version 1.** `hg serve --stdio` on the remote end, stdin and stdout the channel. Handshake: `hello` then
  `between` with `pairs` = forty zeros, a dash, forty zeros; an unknown command is answered with an empty string
  (`0\n`), so a client learns the protocol from the newest answer. A command is its name and `\n`; an argument is
  `<name> <length>\n<value>`; a dictionary argument `<name> <count>\n` then that many `<key> <length>\n<value>`; raw
  input follows as chunks `<length>\n<data>` ending with `0\n`. A `string` answer is `<length>\n<value>`; a `stream`
  answer is raw bytes; an error is the message on stderr followed by `\n-\n`, and `\n` on stdout
  (`_sshv1respondooberror` [C]). An empty command ends the session. Banners before the first answer are tolerated by
  clients.
- **Capabilities a 7.2 server sends** (`_capabilities` [C]): `lookup branchmap pushkey known getbundle unbundlehash`,
  `changegroupsubset` (where the transport exposes it), `stream` or `streamreqs=<sorted requirements>` and
  `stream-preferred` (when `server.preferuncompressed`), `bundle2=<URL-quoted capability blob>`,
  `unbundle=<bundle types in preference order>`, `exp-narrow-1` and `exp-ellipses-2` (narrow serving). The HTTP
  transport adds `batch`, `httpheader=<server.maxhttpheaderlen>`, `httppostargs` (experimental),
  `httpmediatype=0.1rx,0.1tx,0.2tx` and `compression=<engines>`; SSH adds `protocaps` and `batch`.
- **Commands** (the 7.2 table, with their argument specs): `batch` (`cmds *`: `;`-separated `<command> <args>`,
  args `,`-separated `k=v`, escaped `:`→`:c`, `,`→`:o`, `;`→`:s`, `=`→`:e`), `between` (`pairs`), `branchmap`,
  `branches` (`nodes`), `capabilities`, `changegroup` (`roots`, legacy), `changegroupsubset` (`bases heads`, legacy),
  `clonebundles`, `clonebundles_manifest` (`*`), `get_cached_bundle_inline` (`path`), `debugwireargs`
  (`one two *`, a test command), `getbundle` (`*`), `heads`, `hello`, `listkeys` (`namespace`), `lookup` (`key`),
  `known` (`nodes *`), `protocaps` (`caps`), `pushkey` (`namespace key old new`), `stream_out`, `unbundle` (`heads`).
- **Answers that matter for a server.** `branchmap`: lines `<URL-quoted branch> <hex head> ...`, no trailing newline.
  `heads`: hex nodes, space-separated, then `\n`. `known`: one `0` or `1` per node, no newline. `lookup`:
  `1 <hex>\n` or `0 <error>\n`. `listkeys`: `<key>\t<value>` lines. `pushkey`: an integer (SSH: `<int>\n`; HTTP:
  `<int>\n<output>`). `unbundle` without bundle2: the push response, an integer for the change in heads (`0` nothing
  changed, `1` the same number of heads, `n ≥ 2` that `n - 1` heads were added, negative values fewer, "also off by
  1"); over SSH two strings, an empty one and the integer; over HTTP `<int>\n<output>`. With bundle2 the answer is a
  bundle2 stream. `unbundle`'s `heads` is the client's view of the server's heads: a list, `force`, or `hashed` and
  the SHA-1 of the sorted heads (`exchange.check_heads` [C]); a mismatch is `PushRaced`.
- **getbundle's arguments** (`wireprototypes.GETBUNDLE_ARGUMENTS` [C]): `heads` and `common` (node lists),
  `bookmarks`, `obsmarkers`, `phases`, `cg`, `cbattempted`, `stream` (booleans), `bundlecaps` (set), `listkeys`,
  `includepats`, `excludepats` (lists); narrow adds `narrow` (boolean), `depth` (plain), `oldincludepats`,
  `oldexcludepats`, `known` (`narrowbundle2.py` [C]).

## 2. Bundles and bundle2 (`internals.bundles`, `internals.bundle2` [B]; `bundle2_part_handlers.py`, `exchange.py` [C])

- **Bundle headers.** `HG10` + `BZ`/`GZ`/`UN` (bundle1: a changegroup, compressed; bzip2's own `BZ` doubles as the
  header), `HG20` (bundle2), `HGS1UN` (a stream clone bundle).
- **Bundle2 stream.** `HG20`; a 32-bit unsigned big-endian length and that many bytes of space-separated, URL-quoted
  stream parameters (`name` or `name=value`; an upper-case first letter is mandatory, lower-case advisory; the defined
  one is `Compression` = `GZ`, `BZ` or `ZS`, after which every byte is compressed); then parts. A part: a 32-bit
  unsigned big-endian header length (`0` ends the stream); the header: an 8-bit name length, the name (any upper-case
  letter makes the part mandatory), a 32-bit part id, one byte of mandatory and one of advisory parameter counts, a
  (key size, value size) byte pair per parameter, then the keys and values, mandatory first; then the payload as
  32-bit big-endian chunk sizes and chunks, `0` ending the part and `-1` an *interrupt* (a whole part follows, after
  which the interrupted part resumes).
- **Bundle2 capabilities a 7.2 server holds** (`bundle_caps.py` [C]): `HG20`; `bookmarks`; `changegroup` = the
  incoming versions; `delta-compression` (when `storage.revlog.exchange-compressed-delta`); `digests` (`md5`, `sha1`,
  `sha512`); `error` = `abort`, `unsupportedcontent`, `pushraced`, `pushkey`; `hgtagsfnodes`; `listkeys`; `obsmarkers`
  = `V0`, `V1` (when evolution exchange is on); `phases` = `heads`; `pushkey`; `pushback` (when allowed);
  `remote-changegroup` = `http`, `https`; `stream` = `v2` (and `v3-exp` when asked); `checkheads` = `related` under
  `concurrent-push-mode = check-related`, the default since 5.4 (`config.txt` `server` [B]). `rev-branch-cache` "is
  no longer advertised, but still supported for legacy clients".
- **Parts a receiver handles** (`@parthandler` [C]): `changegroup` (params `version`, `nbchanges`, `exp-sidedata`,
  `exp-wanted-sidedata`, `treemanifest`, `targetphase`), `remote-changegroup`, `reply:changegroup`,
  `check:bookmarks`, `check:heads`, `check:updated-heads`, `check:phases`, `output`, `replycaps`, `error:abort`,
  `error:pushkey`, `error:unsupportedcontent`, `error:pushraced`, `listkeys`, `pushkey`, `bookmarks`, `phase-heads`,
  `reply:pushkey`, `obsmarkers`, `reply:obsmarkers`, `hgtagsfnodes`, `cache:rev-branch-cache`, `pushvars`,
  `stream2`, `stream3-exp`; narrow adds `narrow:responsespec`, `narrow:changespec` (and `narrow:spec`).
- **Push race checks.** `check:heads`: 20-byte nodes; the sorted list must equal the server's heads.
  `check:updated-heads`: the nodes must all still be branch heads ("This allow server with high traffic to avoid push
  contention as long as unrelated parts of the graph are involved"). `check:phases`: 24-byte entries (`>i20s`: phase,
  node), each phase must hold. `check:bookmarks`: entries `>20sH` + name, all-ones node meaning absent. Each mismatch
  raises `PushRaced`.
- **What getbundle sends, in order** (`getbundle2partsorder`, the generators in `exchange.py` [C]): `stream2` (when
  asked), `changegroup` (version = the highest the client listed and the server can produce; `nbchanges`,
  `treemanifest`, sidedata parameters; a narrow-ACL response adds `Narrow:responsespec`), `bookmarks`, `listkeys`
  (one part per requested namespace), `obsmarkers` (the markers relevant to the ancestors of the requested heads,
  sorted), `phase-heads` (a publishing server marks every head public; otherwise draft heads plus the public heads
  between them), `hgtagsfnodes` (changeset node, `.hgtags` file node pairs for the outgoing heads),
  `cache:rev-branch-cache` (legacy clients only, never with narrow).

## 3. Changegroups and revlogs (`internals.changegroups`, `internals.revlogs` [B]; `changegroup.py` [C])

### 3.1 The changegroup

- Segments: the changelog's delta group, the manifest's, (tree manifests' for version 3+ with the `treemanifest`
  parameter), then a file segment per file (a chunk holding the file name, then its delta group), an empty chunk
  ending the whole. A chunk is a 32-bit big-endian length including itself, then data; an empty chunk is length `0`.
- A delta is a series of `(start, end, new length, content)` hunks (`>lll`), applied to the base's full text.
- Version 1 deltas are against the previous revision in the group (or the first parent for the first); version 2 and
  later name their base, so a delta may be against any revision the receiver has or receives earlier, and the null
  base sends a full text.
- Revision flags (version 3+): `32768` censored, `16384` ellipsis ("Revision hash does not match data (likely due to
  rewritten parents)"), `8192` externally stored (LFS), `4096` has copy information.

### 3.2 The revlog

- A 64-byte index entry per revision: a 6-byte offset and 2-byte flags, compressed length, uncompressed length, delta
  base, link revision, both parents (`-1` none), a 32-byte node field of which 20 bytes are used. The first entry's
  first four bytes are the header: version and feature flags (`00 03 00 01` = v1, inline, generaldelta).
- Chunk headers: `\0` (the entry is the text), `(` zstd, `u` raw, `x` zlib.
- **The node.** "The 20 byte node ids of the parents are fed into the hasher in ascending order", then the full text,
  SHA-1. So a changeset's, manifest's or file revision's node is fixed by its exact bytes and its parents: a server
  that keeps the bytes and parents keeps the nodes; anything else changes them.
- Delta chains are bounded so reads are bounded: "limited to ~2x the length of the revision's data", and by span. These
  are the *writer's* rules (the client's, when a stream clone hands it a store it will then extend).
- Store paths are encoded (`_direncode`, `_pathencode`, the `dh/` hashed form for long paths) by the `fncache` and
  `dotencode` requirements.

### 3.3 Mercurial's documents and its code disagree on changegroup "04" (stated, both cited)

`internals.changegroups` [B] describes version 4 as "support for exchanging sidedata", a 103-byte header with a
`pflags` byte. The 7.2 code [C] says otherwise: `_packermap` has `04` as "more advances flags" and a new `05` as "adds
support for exchanging sidedata". In the code, `_CHANGEGROUPV4_DELTA_HEADER = ">20s20s20s20s20sHbIBB20sb"` (node, p1,
p2, base, link, flags, snapshot level, raw size, wire compression, protocol flags, the stored delta base, its snapshot
level: 130 bytes, protocol flag `2` "full text", flag `1` "sidedata"), and `05` is the old version 4
(`">B20s20s20s20s20sH"`). A 7.2 repository lists `04` as incoming only with the `delta-info-revlog` requirement
(`use_delta_info`); `05` only with revlog v2 / changelog v2 or `experimental.changegroup5`. The code is what clients
run, so slates follows the code. **Hazard (inferred, to be checked):** the document describes the layout earlier
releases wrote under `04`, so a 6.x client with experimental sidedata would list `04` meaning that layout, and a
version string alone would not say which layout a peer means. This is read from the document, not from 6.x code; the
older-client run of §4.17.3 "The interop proof" checks it, and the rule §4.17.3 "The wire surface" states is safe
either way.

`WireDeltaCompression` [C] (`04` only): a delta may travel still compressed as its store holds it (`1` uncompressed,
`2` zlib, `3` zstd), accepted per the client's `delta-compression` bundle2 capability. A server whose deltas are kept
compressed ships them without recompressing.

## 4. Phases, bookmarks, pushkey and obsolescence

- **Phases** (`phases.txt` [B]; `phases.py` [C]): public `0`, draft `1`, secret `2`, archived `32`, internal `96`
  ("non-continuous for compatibility"); "no changeset can be in a lower phase than its ancestors"; "changeset phases
  should only be changed towards the public phase". A publishing server (the default) turns every pushed changeset
  public; "pull never changes server states"; secret changesets "are not exchanged". The 49-case exchange table is in
  the module's docstring. Binary encoding `>i20s` (phase, node). The `phases` pushkey namespace lists the draft roots as
  `<hex>\t1`, and `publishing\tTrue` on a publishing server ("Publishing repo can't just pretend they are old repo").
- **Bookmarks** (`bookmarks.py` [C]): binary entries `>20sH` (node, name length) then the name. The `bookmarks` pushkey
  namespace maps names to hex nodes; a push "should succeed iff value was old" (`pushkey.push` [C]): a compare and
  swap.
- **Pushkey namespaces** (`pushkey.py` [C]): `namespaces`, `bookmarks`, `phases`, `obsolete` (listed only when
  evolution exchange is on); `encodekeys` joins `key\tvalue` lines with `\n`.
- **Obsolescence markers** (`obsolete.py` [C]; `evolution.txt` [B]): "An obsolete marker maps an old changeset to a
  list of new changesets"; an empty list prunes. Format 0: `>BIB20s` (successor count, metadata size, flags,
  predecessor), the successors, metadata as NUL-terminated `key:value`. Format 1: `>IdhHBBB` (total size, date as a
  float64, timezone in minutes, flags, successor count, parent count with `3` meaning "no parent data", metadata
  count), the predecessor, successors and parents (20 or 32 bytes each, by the `usingsha256` flag), then `(key size,
  value size)` byte pairs and the bytes (each at most 255). Over pushkey, markers travel as format 0, base85, split
  into keys `dump0`, `dump1`, ... by `_maxpayload = 5300` ("arbitrary picked to fit into 8K limit from HTTP server");
  a push of an `obsolete` key with an old value is refused. Instabilities: orphans, content-divergence ("independent
  rewrites of the same changesets lead to different results"), phase-divergence ("the old (obsolete) version of a
  changeset becomes public").
- **evolve's exchange** (hg-evolve 12.0.0 [C]): `_evoext_getbundle_obscommon` and `evoext_obshashrange_v1` (marker
  discovery by hash ranges, cached in `evoext_obshashrange_v2.sqlite`).

## 5. Topics (hg-evolve 12.0.0, `hgext3rd/topic` [C])

- A topic is the changeset extra `topic` (with `topic-namespace`); "Topic are quite similar to the way named branch
  work, except they eventually fade away when the changeset becomes part of the immutable history"; the
  fully-qualified branch name is `branch//namespace/topic`.
- Publishing: `[phases] publish = no` with `experimental.topic.publish-bare-branch = yes` publishes changesets without
  a topic and keeps topics draft; `hg push --publish`; `experimental.topic.allow-publish = no` refuses publishing;
  `experimental.topic.server-gate-topic-changesets` serves topic changesets only to topic-aware clients.
- Wire: capabilities `topics`, `topics-namespaces`, `ext-topics-publish=<mode>`, `ext-topics-tns-heads`,
  `_exttopics_heads`; commands `_exttopics_heads` and `tns_heads` (`namespaces`); a topic-aware `branchmap`. Head
  checks on a non-publishing server count heads per topic ("pushing a new topic will not requires any specific flag").

## 6. Hooks and access control

- **Hooks** (`config.txt` `hooks` [B]): gate hooks `prechangegroup`, `pretxnchangegroup`, `pretxnopen`, `pretxnclose`,
  `pretxnclose-bookmark`, `pretxnclose-phase` ("called multiple times, once for each revision affected"), `prepushkey`,
  `prelistkeys`, `preoutgoing`, `pretransmit-inline-clone-bundle`; notification hooks `changegroup`, `incoming`
  (once per changeset), `outgoing`, `txnclose`, `txnclose-bookmark`, `txnclose-phase`, `txnabort`, `pushkey`,
  `listkeys`. The environment carries `HG_NODE`, `HG_NODE_LAST`, `HG_URL`, `HG_SOURCE`, `HG_TXNNAME`, `HG_TXNID`,
  `HG_BOOKMARK_MOVED`, `HG_PHASES_MOVED`, `HG_NEW_OBSMARKERS`, `HG_NAMESPACE`, `HG_KEY`, `HG_OLD`, `HG_NEW`,
  `HG_HOOKTYPE`, `HG_HOOKNAME`. `pushvars` parts add `USERVAR_*` variables. Core ships hook functions in
  `hgext/hooklib`: `changeset_obsoleted`, `changeset_published`, `enforce_draft_commits`, `reject_merge_commits`,
  `reject_new_heads` (each with a test, §9).
- **acl** (`hgext/acl.py` [C]): run as `pretxnchangegroup`/`pretxncommit`; "The authorization is matched based on the
  local user name on the system where the hook runs, and not the committer of the original changeset"; order: deny
  branches, allow branches, deny paths, allow paths; bookmark rules (`acl.deny.bookmarks`, `acl.allow.bookmarks`);
  groups `@name`; `sources = serve`.
- **Narrow ACL** (`exchange.applynarrowacl`, `narrow_widen` [C]): a `[narrowacl]` section forces includes and
  excludes per user; a widening that would remove a required exclude is refused, naming the user.

## 7. Narrow, shallow and large files

- **Narrow** (`hgext/narrow` [C], "EXPERIMENTAL" in its docstring, yet shipped): getbundle with `narrow`,
  `includepats`, `excludepats`, `oldincludepats`, `oldexcludepats`, `known`, `depth`; the `narrow_widen` command
  (`oldincludes oldexcludes newincludes newexcludes commonheads cgversion known ellipses`); a narrow repository uses
  changegroup 03 or later (01 and 02 cannot carry flags). Ellipsis (depth-limited) serving is behind
  `narrow.serveellipses`, whose old name is `narrowservebrokenellipses`; capability `exp-ellipses-2`; ellipsis
  revisions carry flag `16384`.
- **Shallow file history** (`hgext/remotefilelog` [C]): "This extension is HIGHLY EXPERIMENTAL. There are NO
  BACKWARDS COMPATIBILITY GUARANTEES"; commands `x_rfl_getflogheads` (`path`), `x_rfl_getfile` (`file node`),
  `x_rfl_getfiles` (raw, SSH); capabilities `x_rfl_getfile`, `x_rfl_getflogheads`,
  `exp-remotefilelog-ssh-getfiles-1`; requirement `exp-remotefilelog-repo-req-1`.
- **largefiles** (`hgext/largefiles/proto.py` [C]): capability `largefiles=serve`; `putlfile` (the server checks the
  SHA-1 of what it received: "largefile contents do not match hash"), `getlfile` (the length on the first line, then
  the bytes), `statlfile` (`0` present, `2` missing, `1` reserved for a mismatch "too expensive to be verified on every
  stat"), and `lheads`.
- **lfs** (`hgext/lfs/wireprotolfsserver.py` [C]; capability notes in `internals.wireprotocol` [B]): the Git LFS batch
  API at `<repo>/.git/info/lfs/objects/batch` (`application/vnd.git-lfs+json`, `basic` transfer only, operations
  `upload` and `download`) and basic transfers under `<repo>/.hg/lfs/objects/<oid>`; capabilities `lfs` (extension
  on) and `lfs-serve` (blobs committed, the `lfs` requirement); file revisions carry flag `8192`.

## 8. Stream clones, clone bundles, hgweb and annotate

- **Stream clone** (`streamclone.py` [C]; `config.txt` `server.uncompressed` [B]): version 1 is `stream_out`
  (`<file count> <byte count>\n` then `<store path>\0<size>\n` + bytes per file; `1\n` refused, `2\n` lock failed);
  version 2 is the `stream2` part (`requirements`, `filecount`, `bytecount`) whose payload is, per file, one source
  byte (`s` store, `c` cache), the name's length and the size as unsigned varints, the name and the bytes. Mercurial's
  own guidance: a stream clone "transfers about 40% more data than a regular clone, but uses less memory and CPU on both
  server and client. Over a LAN (100 Mbps or better) or a very fast WAN, an uncompressed streaming clone is a lot faster
  (~10x) than a regular clone. Over most WAN connections (anything slower than about 6 Mbps), uncompressed streaming is
  slower". Those are fixed figures for one choice that slates learns per connection instead (§4.17.3 "Learned
  tunables").
- **Clone bundles** (`wireprotov1server.py` [C]): `clonebundles` (legacy, filtering `peer-bundle-cache://` and
  fingerprinted entries), `clonebundles_manifest`, and `get_cached_bundle_inline` (a bundle sent inline over the
  connection, its length as a varint first).
- **Server settings with fixed numbers** (`config.txt` [B]): `maxhttpheaderlen` 1024, `zliblevel` -1 ("likely
  equivalent to 6"), `zstdlevel` 3, `bundle1*` switches, `concurrent-push-mode`, `validate` (off by default: "Whether
  to validate the completeness of pushed changesets"), `view` (`served`: "excludes secret and hidden changesets").
- **hgweb** (`hgweb/webcommands.py` [C]; `hgweb.txt` [B]): `log`, `rawfile`, `file`, `changelog`, `shortlog`,
  `changeset`, `manifest`, `tags`, `bookmarks`, `branches`, `summary`, `filediff`, `comparison`, `annotate`
  (`/annotate/{revision}/{path}`), `filelog`, `archive`, `static`, `graph`, `help`; styles include `json` and `raw`,
  chosen by a hyphenated prefix (`/json-log`, "preferred") or `?style=json`.
- **Annotate** (`hgext/fastannotate` [C]; `internals.linelog` [B]): core annotate walks the file's history per request;
  fastannotate keeps a linelog cache and "is expected to be faster than the vanilla 'annotate' if the cache is
  present", and works best when "mainbranch is some pointer that always moves forward". Linelog is a weave over a
  linear history ("This enforces linear history"), which is what a green's version chain is.

## 9. Mercurial 7.2's own tests for the wire protocol (names from the 7.2 sdist; the interop runs main makes)

Wire framing and commands: `test-wireproto.t`, `test-wireproto.py`, `test-wireproto-framing.py`,
`test-wireproto-clientreactor.py`, `test-wireproto-serverreactor.py`, `test-batching.py`, `test-ssh-proto.t`,
`test-ssh-proto-unbundle.t`, `test-ssh.t`, `test-ssh-batch.t`, `test-ssh-bundle1.t`, `test-ssh-clone-r.t`,
`test-ssh-repoerror.t`, `test-sshserver.py`, `test-http.t`, `test-http-protocol.t`, `test-http-permissions.t`,
`test-http-bad-server.t`, `test-http-branchmap.t`, `test-http-bundle1.t`, `test-http-clone-r.t`, `test-http-proxy.t`,
`test-https.t`, `test-known.t`, `test-unbundlehash.t`, `test-getbundle.t`, `test-setdiscovery.t`,
`test-treediscovery.t`, `test-treediscovery-legacy.t`, `test-serve.t`, `test-server-view.t`.
Bundle2: `test-bundle2-format.t`, `test-bundle2-exchange.t`, `test-bundle2-multiple-changegroups.t`,
`test-bundle2-pushback.t`, `test-bundle2-remote-changegroup.t`, `test-bundle2.py`.
Clone, pull and push: `test-clone.t`, `test-clone-r.t`, `test-clone-stream.t`, `test-clone-stream-format.t`,
`test-clone-stream-revlog-split.t`, `test-stream-bundle-v2.t`, `test-clonebundles.t`, `test-clonebundles-autogen.t`,
`test-pull-http.t`, `test-pull-permission.t`, `test-pull-bundle.t`, `test-pull-r.t`, `test-push.t`, `test-push-http.t`,
`test-push-race.t`, `test-push-warn.t`, `test-pushvars.t`, the 30 `test-push-checkheads-*.t`.
Phases, bookmarks and obsolescence: `test-phases-exchange.t`, `test-bookmarks-pushpull.t`, `test-bookmarks-push-race.t`,
`test-obsolete-changeset-exchange.t`, `test-obsolete-checkheads.t`, `test-obsolete-distributed.t`,
`test-obsolete-bundle-strip.t`, the 22 `test-exchange-obsmarkers-case-*.t`.
Hooks and access: `test-hook.t`, `test-acl.t`, the five `test-hooklib-*.t`, `test-lfs-serve-access.t`.
Narrow and shallow: the 30 `test-narrow-*.t`, the 32 `test-remotefilelog-*`.
Large files: `test-largefiles-wireproto.t`, `test-largefiles.t`, `test-lfs-serve.t`, `test-lfs-test-server.t`,
`test-lfs-bundle.t`.
hgweb: `test-hgweb.t`, `test-hgweb-commands.t`, `test-hgweb-json.t`, `test-hgweb-annotate-whitespace.t`,
`test-hgweb-raw.t`, `test-hgweb-filelog.t`, `test-hgweb-auth.py`.

The tests start their own servers (`hg serve -p $HGPORT -d --pid-file=hg.pid`; SSH through the suite's `dummyssh`,
which runs `hg -R <repo> serve --stdio` locally) and then read the server's repository directory with local `hg`
commands. So a run against slates needs the shim §4.17.3 "The interop proof" describes, and the cases that read the
server's directory are the ones its reviewed expected-failure list starts from.

## 10. Further facts the design cites (read 2026-11-18, 7.2)

- **Words a client prints.** A push race over bundle2: "remote repository changed while pushing - please try again"
  (`bundle2_part_handlers.py` [C]); over bundle1: "repository changed while %s - please try again", the context
  `uploading changes` (`exchange.check_heads` [C]). A refusing hook: `HookAbort("%s hook %s")` with
  `explainexit`'s "exited with status %d" or "killed by signal %d" (`hook.py`, `utils/procutil.py` [C]).
- **Pending changes shown to hooks.** `hook.py` sets `HG_PENDING` to the repository's root for a hook run inside a
  transaction, and `txnutil.mayhavepending` reads pending files only when `HG_PENDING` names the root [C].
- **Markers a client applies.** A receiver without obsolescence enabled answers an `obsmarkers` part with "ignoring
  obsolescence markers, feature not enabled" (`bundle2_part_handlers.py` [C]); a client advertises `obsmarkers` in its
  bundle2 capabilities only with evolution exchange on (`bundle_caps.get_repo_caps` [C]), and `pushback` only with
  `experimental.bundle2.pushback` (`exchange.py` [C]).
- **Rebase.** Mercurial's rebase writes the extra `rebase_source` naming the replaced changeset (`hgext/rebase.py`
  [C]); its file revisions are written by commit's `_filecommit` (`mercurial/commit.py` [C]).
- **Paths.** `pathutil.pathauditor` refuses a path that ends with a separator, lies under a top-level `.hg`, starts at a
  Windows drive's root or contains `..` [C].
- **Deployment scripts shipped with 7.2** (sdist listing): `contrib/hgweb.wsgi`, `contrib/hgweb.fcgi`, `hgweb.cgi` and
  `contrib/hg-ssh`, the forced command that restricts an SSH key to `hg -R <repo> serve --stdio`.
- **Conversion.** `hg convert`'s `--rev` imports "up to source revision REV"; `convert.git.similarity` (rename
  detection) defaults to 50 (`hgext/convert/__init__.py` [C]).
- **The shared notes.** g261's `research/vcs-servers.md` quotes 7.2.4's sdist for the shared mapping (§4.17.1); the
  facts in this file were read in 7.2's own files, and the two agree on every line §4.17.1 quotes.

## 11. What this reading decided (the short list §4.17.3 rests on)

1. A node is SHA-1 over the sorted parents and the exact text (§3.2), so the server keeps every changeset, manifest and
   file revision's bytes and parents exactly (§4.17.1's objects); the greens hold the trees.
2. A named branch may have several heads (§1 `branchmap`), so each head is a line of its own, a green; the named branch
   is the set of its lines.
3. Phases are attributes of changesets with one invariant, only ever toward public (§4): public changesets are never
   dropped or rewritten, a rewrite of a line drops only drafts a marker obsoletes, and secret ones never arrive.
4. A stock client declares each changed file whole (a manifest entry with a new file node), never byte ranges, so its
   push is judged per file: §4.17.1's whole-file rule, no inference.
5. `check:updated-heads` names exactly the lines a push moves, which is §4.17.1's base; `check:heads` and `unbundle`'s
   `heads` ask more (every head unchanged), checked in the push's own register step.
6. The server can rewrite a pushed changeset only when the client can apply what it rewrote: `pushback` and marker
   exchange both (§2, §4, §10).
7. Topics fade when public and live as drafts until then (§5): a topic is a review request, a chain of works.
8. Changegroup `04` names two layouts across versions (§3.3): send it only when the peer's other capabilities identify
   the 7.x layout; take `01` to `03` in pushes.
9. Every fixed number stock Mercurial uses on its serving path (§8: header length, compression levels, the stream-clone
   rule of thumb, `_maxpayload`) is a choice slates learns per deployment or replaces with a stated bound.

## 12. Found while building piece 1 (A-514, 2026-11-19; 7.2's code [C], and runs of stock `hg` 7.2)

- **A chunk length of 4 is refused, not empty.** `changegroup.getchunk` and `_chunklength` refuse any length `l` with
  `0 < l <= 4` ("invalid chunk length"); only 0 is the empty chunk. A writer still emits 4 for empty sidedata
  (`chunkheader(len(sidedata))` in `_revisiondeltatochunks`), which 7.2's own reader would refuse; sidedata is never
  empty in practice (its serialization begins with a count).
- **The tree-manifest list is always there from changegroup 03.** `cg3unpacker._unpackmanifests` reads a list of
  directory groups after the root manifest unconditionally; `internals.changegroups` says the segment exists "only if
  the 'treemanifest' param" is set. The fixtures settle it: every 03 and 04 changegroup 7.2 wrote carries the empty
  list (one empty chunk), and 7.2's own `hg debugbundle --all` of the 03 bundle lists the changelog and the manifest and
  no file, because it reads that empty chunk as the end of the file list.
- **7.2's `hg debugbundle --all` fails on a changegroup 04** that carries full texts: `TypeError: object of type
  'NoneType' has no len()` at `debugcommands.py:398` (`len(deltadata.delta)`, where a full text has no delta). Seen on
  the first fixture run on the Mac Studio; the fixture script lists 04 bundles with the plain `debugbundle` instead.
- **mpatch's rules are C's, not the pure module's.** `mpatch.c`'s `mpatch_decode` refuses a hunk whose start is
  negative or past its end or whose length is negative, and a delta not consumed exactly; `mpatch_apply` refuses a
  hunk that starts before the previous one ended or ends past the base. `pure/mpatch.py` checks less (its
  `patchedsize` stops at `start > end` without refusing). Clients run the C version; the codec holds its rules.
- **A part header is read loosely by 7.2.** `unbundlepart._fromheader` slices the header block, so parameter sizes
  that run past it yield short keys and values, and bytes left after the parameters are ignored; `_initparams` keeps
  the last of two equal keys. `internals.bundle2` says keys "MUST be unique". slates refuses all three.
- **An interrupt reads one part, or none.** `interrupthandler.__call__` reads one part header; a header length of 0 is
  "no part found during interruption", after which the interrupted part resumes. Nothing in the protocol bounds how
  deep interruptions nest (each is handled by the same code, recursively in 7.2); slates walks them with a count and
  bounds them by the request's admission.
- **Stream parameters are matched without case.** `_processparam` looks a name up as `name.lower()`, so a lower-case
  `compression=GZ` is processed as `Compression` is; an unknown name is ignored when its first letter is lower-case
  and refused otherwise.

## 13. Found while building piece 1's part payloads (A-545, 2026-11-19; 7.2's code [C] in the wheel `uvx` installs, and runs of stock `hg` 7.2 on the Mac Studio)

- **7.2's two format 1 marker readers disagree, measured.** 7.2 reads an obsstore or an `obsmarkers` payload with the C
  reader when it is built (`_fm1readmarkers`, `obsolete.py:459`: `parsers.fm1readmarkers`, the wheel's
  `cext/parsers.cpython-312-darwin.so`) and the pure one otherwise (`_fm1purereadmarkers`, `obsolete.py:347`). Three
  markers, each fed to both by a probe run with `uvx --python 3.12 --from mercurial==7.2 python -I`: a size field of 41
  over a 39-byte marker and two more bytes reads as one marker in C (the two bytes skipped as the marker's) and fails
  the pure reader (`unpack requires a buffer of 19 bytes`); a parent count of 0 is no parent data (`None`) in C and
  parents recorded and none (`()`) in the pure reader; a parent count of 4 with no parent bytes is no parent data in C
  and fails the pure reader (`unpack requires a buffer of 80 bytes`). The pure reader's branches are
  `obsolete.py:386-395` (`numpar == noneflag` → `None`, `numpar == 1`, else `numpar` nodes). slates-hg refuses a size
  that is not exactly the marker's fields and a count past 3, so both readers read alike whatever it accepts, and keeps
  a count of 0 as written (7.2's writer writes it for `parents == ()`), read as the pure reader reads it.
- **Format 0's metadata.** `_fm0decodemeta` (`obsolete.py:291-298`) splits at NUL, skips empty entries and splits each
  at its first `:`, so an entry without one fails it; `_fm0encodeonemarker` refuses a SHA-256 marker ("cannot handle
  sha256 with old obsstore format", `obsolete.py:261`) while `_fm0readmarkers` reads 20-byte nodes whatever the flag.
- **A push sends `phase-heads` only for what it publishes.** `_pushb2phaseheads` (`exchange.py:1046-1053`) adds the part
  only when `pushop.outdatedphases` is not empty, and then only public heads (`updates[0]`); a getbundle sends every
  phase's heads. The captured pushes have none: the modern push left its draft a draft, and the legacy one published
  through a pushkey part.
- **The client asks `listkeys phases` after every push** (`_pushsyncphase`, `exchange.py:1346-1350`: "even when we don't
  push, exchanging phase data is useful"). Over SSH version 1 a bundle2 reply is written unframed
  (`_sshv1respondstream`, `wireprotoserver.py:378-382`) and the next response, `<length>\n<payload>`, follows it at
  once, so a reply cut at the end of the server's output carries that response after its stream's end. 7.2's reader
  stops at the stream's end marker and never sees it; slates-hg refuses it as trailing bytes, which is how the first
  fixture run's cut was found (3 of 78 tests). The fixture script now walks each reply's frames to its end and keeps the
  listkeys payload apart: the amended draft as the one draft root after the modern push, nothing after the legacy push
  published it, the strict push's commit after that.
- **The server's reply parts.** `handlepushkey` (`bundle2_part_handlers.py:438-444`) writes `reply:pushkey` with the
  default mandatory type (`REPLY:PUSHKEY` on the wire) and both parameters advisory, `in-reply-to` as the part id's
  decimal and `return` as `'%i'`; `handlepushkeyreply` (`:530-535`) reads both with `int()`. The captured replies hold
  exactly that, and slates-hg's `reply_part` writes parts equal to them.
- **The legacy exchange.** With `devel.legacy.exchange=bookmarks,phases` 7.2's client moves a phase and a bookmark by
  `pushkey` parts (four mandatory parameters, in the order `namespace`, `key`, `old`, `new`) instead of `phase-heads`
  and `bookmarks`, and still sends `check:bookmarks` and `check:phases`.
- **The fixtures are deterministic.** The payload fixture script, run a second time on 2026-11-19 from a clean clone,
  made every file the first run made byte for byte (the pushes, the obsstores, every listing and log) but the three
  replies, whose cut it had changed.

## 14. Found while building piece 2, the protocol codecs (A-569, 2026-11-19; captures of stock `hg` 7.2 on the Mac Studio, `fixtures/wire`)

- **Arguments arrive as the bytes the client meant, every way.** `debugwireargs un deux --three 'a b=c&d+e%f;g,h:i'
  --four café` was echoed `un deux a b=c&d+e%f;g,h:i café None` (UTF-8 intact) over SSH, in `X-HgArg` headers, behind
  `X-HgArgs-Post` and in the query (`fixtures/wire/*.answer`, four files byte for byte alike): 7.2's HTTP server reads a
  field's escapes as the bytes they name, as slates-hg does.
- **Where 7.2's client puts HTTP arguments.** With the server advertising `httpheader=1024` (its default): in
  `x-hgarg-1`, `x-hgarg-2`, ... (sent lower-case; a server matches them without case), the form split at 1009 bytes
  (`encodevalueinheaders`: the limit less `X-HgArg-000` and `: ` with the line end), the query only `cmd`. With
  `httppostargs` advertised: a `POST` whose body begins with the form, `x-hgargs-post` its length. With `httpheader=0`:
  after `cmd` in the query. Either way the fields are sorted by name and form-encoded (`quote_plus`: a space as `+`, `/`
  quoted). Every request also sends `x-hgproto-1: 0.1 0.2 comp=zstd,zlib,none,bzip2 partial-pull` and a `vary` naming
  the `x-hg` headers it sent.
- **What 7.2's SSH client sends.** `hello` and `between` (`pairs` of forty zeros, a dash, forty zeros) together, then
  `protocaps` (`caps`: the same list as `x-hgproto-1` without `0.1 0.2`), each command's arguments sorted by name so the
  dictionary (`*`) comes first, its entries in the client's order; `batch` as `* 0` and `cmds` (discovery's `heads
  ;known nodes=<hex>`, `heads` with its space and no arguments); `getbundle` as a dictionary of seven entries
  (`bundlecaps`, `common`, `heads`, `cg`, `phases`, `bookmarks`, `listkeys`, in that order); `unbundle`'s input in
  chunks after the server's empty string (one chunk of 973 bytes in the capture), ended by `0\n`. No empty line ends a
  session; the client closes its end.
- **A bundle2 push always sends `heads=force`.** Both pushes (SSH and HTTP, neither forced) sent `heads` = `666f726365`,
  `force` in hex: with bundle2 the race check travels in the `check:` parts (A-545), so `unbundle`'s own heads argument
  no longer carries it.
- **What 7.2's SSH server answers.** `hello`: `capabilities: ` and its capabilities sorted by name, then a newline;
  `between`: one empty line per pair; `protocaps`: `OK`; `batch`: the answers escaped and joined by `;` (`heads`: hex
  nodes and a newline, `known`: `0` or `1` per node); `getbundle` and a bundle2 `unbundle`: the stream unframed
  (`_sshv1respondstream`, `wireprotoserver.py:378-382`), so the next answer follows the stream's end at once. The
  bundle2 capability blob is sorted by name, `HG20` first, each `name=value1,value2` quoted, and the whole quoted again
  in `bundle2=`.
- **A bundle1 client is refused differently over SSH and over HTTP.** Over HTTP, `getbundle` answered
  `application/hg-error`, its body `incompatible Mercurial client; bundle2 required` and the hint, which the client
  printed after `abort: remote error:`. Over SSH the server wrote `abort: incompatible Mercurial client; bundle2
  required` and the hint on stderr and ended the session, and the client reported `stream ended unexpectedly (got 0
  bytes, expected 4)` (`ssh-oob.*`): the out-of-band form (an empty line on stdout, the message and `\n-\n` on stderr)
  was not used. slates-hg writes that form for SSH errors (`ssh::respond_error`); which refusals use it is the serving
  piece's to decide.
- **Media types.** `capabilities`, `batch`, `listkeys`, `branchmap`, `debugwireargs` and a push's `unbundle` were
  answered `application/mercurial-0.1`; `getbundle` `application/mercurial-0.2`, its first byte the engine name's
  length: `zstd` by default (the server's first engine the client named, `_httpresponsetype`), `none` when the server
  offers only that (`server.compressionengines=none`), its payload then a plain bundle2 stream. String answers carry a
  `Content-Length`; the streams (`getbundle`, and a bundle2 push's `unbundle` answer, which was `0.1`) were sent
  `Transfer-Encoding: chunked`, and the fixtures hold their bodies de-chunked.
- **The captures are reproducible but for the HTTP heads.** The SSH captures depend only on the script's fixed
  configuration; the HTTP requests carry the run's port (`host`) and the answers its date (`Date`), so those heads
  differ from run to run and the tests read only their fields.
