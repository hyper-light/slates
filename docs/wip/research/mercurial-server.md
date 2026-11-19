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

## 15. Found while building piece 3's objects and nodes (A-588, 2026-11-19; 7.2's code [C] from Homebrew's 7.2 and its PyPI source distribution, and runs of stock `hg` 7.2 on the Mac Studio, `fixtures/objects`)

- **7.2 hashes nodes with SHA-1 collision detection.** `utils/hashutil.py` takes `sha1 = sha1dc.sha1` whenever its C
  extension imports and falls back to `hashlib.sha1` only when it does not; `utils/storageutil.py`'s
  `hashrevisionsha1` hashes every node through it. Homebrew's 7.2 ships `sha1dc.cpython-314-darwin.so`, and both
  fixture runs of the PyPI 7.2 through `uvx` printed `hashutil.sha1 is <class 'sha1dc.sha1'>`. The binding
  (`thirdparty/sha1dc/cext.c`, read from `mercurial-7.2.tar.gz`, SHA-256 `a250227e…`) calls `SHA1DCInit` and then
  `SHA1DCSetSafeHash(&(self->ctx), 0)`, "We don't want "safe" sha1s ... We just want to detect collisions", and its
  `finalize` raises `OverflowError("sha1 collision attack detected")` when `SHA1DCFinal` reports one, so a revision
  whose hashing shows a known attack ends 7.2's push. slates-hg hashes through `sha1-checked` configured the same way
  (A-600's crate, git's configuration too). No public collision has 7.2's framing: the first forty bytes it hashes are
  the two parents in ascending order, and SHAttered's prefix opens with a higher twenty bytes (`%PDF-1.3…`) than its
  next twenty (` obj\n<</Width…`), so the detection is proven on the hash itself.
- **A copy, as `filelog.renamed` reads it.** None unless the revision's first parent is null ("When creating a copy or
  move we set filelog parents to null"), and none unless the block names both keys: "copy and copyrev occur in pairs.
  In rare cases due to old bugs, one can occur without the other. So ensure both are present to flag as a copy"
  (`filelog.py`). Half a pair is no copy, not an error, so a store from those old versions imports.
- **7.2 reorders some file revisions' parents on the way in** (`revlogutils/rewrite.py` `filter_delta_issue6528`,
  `_has_bad_parents`, on by default as `storage.revlog.issue6528.fix-incoming`): a revision with a metadata block and
  one null parent gets the null parent first; one without a block gets it second. Mercurial 5.8 had written some the
  other way round (issue6528). The node is unchanged, since it hashes the parents in order of value. Changegroup `04`
  carries a separate flag for a revision with a block (`REVIDX_HASMETA`, bit 11, "used only by filelog") and swaps such
  a revision's parents the other way in `emitrevisions`, so the rule a reader applies depends on the version it reads.
- **An empty user corrupts a changeset text.** 7.2's writer refuses one: "An empty username or a username with a "\n"
  will make the revision text contain two "\n\n" sequences -> corrupt repository since read cannot unpack the revision"
  (`changelog.add`); and `readfiles` cuts a text at its first `\n\n`, which an empty user line would be, while
  `changelogrevision` reads the files after the date line, so the two readers would disagree.
- **The extras' escapes, run through 7.2's own `_string_unescape` (Python 3.14.7, Homebrew's 7.2).** The item
  `k:\t\x41\101\'\q\777\\\0\\0` reads `k:` and tab, `A`, `A`, `'`, `\q` kept as it is (a `DeprecationWarning`, not an
  error), `\777` as the byte `0xFF` (CPython keeps an octal escape's low byte), `\\` and NUL, `\\` and `0`: the `\0`
  fix-up pairs backslashes from the left before `escape_decode` reads the rest. `decodeextra` of `branch:a\0branch:b`
  is `{branch: b}`: a key given twice keeps the later value.
- **7.2's C manifest reader refuses four things at parse time** (`cext/manifest.c` `find_lines`): an entry whose path
  is empty, a text not ending in a newline, a line under 42 bytes, and paths not strictly ascending by `strcmp`; a node
  that is not forty hexadecimal digits, or a flag other than `l`, `t` and `x`, fails only when the entry is read.
  slates-hg refuses all of these when the text is read, and upper-case digits and the tree flag `t` too, so every text
  it reads writes back to its bytes.
- **`debugdata` and `debugindex`.** `hg debugdata -c|-m|FILE REV` writes a revision's raw text (`rawdata`, a file
  revision's metadata block included): the 30 texts were byte for byte the ones the storage API's `revision()` gave.
  `hg debugindex --debug` lists a header and seventeen columns per revision (`rev`, `rank`, `linkrev`, `nodeid`,
  `p1-rev`, `p1-nodeid`, `p2-rev`, `p2-nodeid`, `full-size`, `delta-base`, `flags`, `comp-mode`, `data-offset`,
  `chunk-size`, `sd-comp-mode`, `sidedata-offset`, `sd-chunk-size`); every text's length equalled its `full-size`, and
  no revision had a flag.

## 16. Found while building piece 1's compressed streams (A-598, 2026-11-19; 7.2's code [C] from its PyPI source distribution, SHA-256 `a250227e…`; the bundles stock `hg` 7.2 compressed on the Mac Studio, `fixtures/compressed`; libzstd 1.5.7's and libbz2-rs-sys 0.2.6's sources)

- **How 7.2 frames a compressed bundle.** bundle1: `bundle2.bundletypes` maps `HG10UN` to (`HG10UN`, `UN`), `HG10GZ` to
  (`HG10GZ`, `GZ`) and `HG10BZ` to (`HG10`, `BZ`): the bzip2 stream follows `HG10` whole, its own magic `BZ` reading as
  the header's last two bytes. `exchange.readbundle` reads four bytes, then the two-byte type, and `cg1unpacker` turns
  `BZ` into `_truncatedBZ`, whose reader feeds `BZ` back to the decompressor first. bundle2: `setcompression` adds
  `Compression=<type>` (nothing for `UN`), and `getchunks` writes the magic, the parameters' length and block as they
  are, then every core chunk through the engine. The fixtures open `HG10GZ` and `x\x9c`; `HG10BZh91AY&SY`; and `HG20`,
  14, `Compression=GZ` and `x\x9c`, the same with `BZh91AY&SY`, and with a zstd frame.
- **What 7.2's readers take.** `supportedbundletypes` is every registered bundle type, `_truncatedBZ` among them, so
  `processcompression` takes `Compression=_truncatedBZ`; `cg1unpacker` takes any two-byte type in the set, so `HG10ZS`
  is read, though `parsebundlespec` refuses `zstd-v1` ("compression engine zstd is not supported on v1 bundles";
  `_bundlespecv1compengines` is `gzip`, `bzip2`, `none`, "THIS SHOULD NEVER CHANGE"). A second `Compression` runs
  `processcompression` again, the last winning. Values are matched exactly (`gz` is unknown).
- **Where 7.2's reading stops.** `_CompressedStreamReader.read` asks for 65,536 compressed bytes at a time, and the
  bundle2 reader stops at the end-of-stream header, so bytes after a compressed stream, or a zlib trailer the bundle's
  end comes before, are never read. zlib's end is found by feeding a copy of the decompressor one byte and finding it
  unused, bz2's by `EOFError`, zstd's by the `ZstdError` python-zstandard raises once its `decompressobj` has finished a
  frame: one frame is the whole stream.
- **The relay.** `unbundle20._forwardchunks`, which an SSH client's `getbundle` relays through, reads the parameters,
  drops every item whose raw name lower-cases to `compression`, and inflates the rest: the plain bundle
  `compression::plain` makes.
- **7.2's zstd frames.** `ZstdCompressor(level=3).compressobj()`, given no size: the fixture's frame and the wire
  capture's (`http-clone.03`) both open `28 b5 2f fd 00 58`: no content size, no checksum, no dictionary, a 2 MiB window
  (Exponent 11). For such a frame libzstd 1.5.7 holds a 131,072-byte input buffer and a 2,359,360-byte output buffer
  (the window and two 128 KiB blocks, each with 32 bytes of slack): 2,490,432 bytes, which its `ZSTD_sizeof_DCtx`
  reports after the fixture's frame is inflated.
- **The default levels.** zlib `-1` (zlib's 6), bzip2 9 (`BZ2Compressor`'s, at work factor 0, libbzip2's 30), zstd 3,
  single-threaded.
- **libbz2-rs-sys's fast decoder** allocates `tt`, one `u32` per byte of the block size its header digit names (3.6 MB
  at `BZh9`), when it reads that digit, beside a zeroed state; the fast mode is the one Python's `BZ2Decompressor` runs.

## 17. Found while building piece 3's revlog reader (A-611, 2026-11-19; 7.2's code [C] from Homebrew's 7.2, and runs of stock `hg` 7.2 on the Mac Studio, `fixtures/revlogs`)

- **The changelog keeps a data file from its first revision.** In the zstd and zlib repositories every changelog had a
  `00changelog.d` beside its index while the manifests and files stayed inline under 7.2's 128 KiB `_maxinline`
  (`revlog.py`), so a store holds both shapes from its start. `_maxinline` is a module constant with no configuration;
  the fixtures lower it with a two-line extension to split every revlog, as the store's own rule would at that size.
- **Version 2 in 7.2.** `experimental.revlogv2=enable-unstable-format-and-corrupt-my-data` makes an `exp-revlogv2.2`
  store with `fileindex-v1` and without `fncache` or `dotencode` (`repo/creation.py`); each revlog's `.i` is an 83-byte
  docket (the header `0x0000DEAD`, three 8-byte ids, the sizes, `(` as the default header) naming `<radix>-<id>.idx`
  (96-byte entries), `.dat` and `.sda`. The PyPI build aborts on such a store ("accessing `fileindex` repository
  without associated fast implementation") unless `storage.fileindex.slow-path=allow` (`repo/vfs_options.py`).
  `format.exp-use-changelog-v2` gives the changelog 96-byte entries of its own with no base and no link, every
  revision a full text.
- **A version 2 chunk's mode** (`revlogutils/deltas.py` `delta_compression`): plain for an empty chunk, a raw one
  beginning with NUL, or a `u` chunk with its `u` dropped; default when it begins with the docket's default header,
  which stays in place (a zstd frame's magic begins with `(`); else its own header. The grown file's twelve revisions
  in the version 2 store were nine default and three plain.
- **Sparse-revlog's intermediate snapshots need large texts.** With `format.maxchainlen=3` and 300-byte texts 7.2 wrote
  only full snapshots; with a file of about 6 KB of SHA-256 lines growing a line a changeset it wrote four intermediate
  snapshots (`hg debugdeltachain`: `snap`), a delta against an earlier snapshot no longer than half the text and than
  that snapshot (`_is_good_delta_info_snapshot_constraints`).
- **A compressed chunk cut short is refused by its engine's own reader:** flate2 1.1.11's `zio::read` answers
  `UnexpectedEof`, "incomplete deflate stream", once its input ends with the stream unfinished (`src/zio.rs`), and
  zstd's reader refused a frame cut short (the test `a_compressed_chunk_must_end_exactly`); bytes after a stream are
  found by the count of the bytes it took.

## 18. Found while building piece 4's read-serving logic, part 1 (A-619, 2026-11-19; 7.2's code [C] from its PyPI source distribution, SHA-256 `a250227e…`; a probe of stock `hg serve --stdio` 7.2 on the Mac Studio, `fixtures/serve`)

- **`heads`** is `reversed(cl.headrevs())`: the served view's DAG heads newest first, a closed branch head among them;
  an empty repository answers the null node. Measured: the probe's three heads, a closed head included, newest first.
- **`known`** answers `1` for the null node (its revision, `nullrev`, is neither missing nor filtered). Measured:
  `11011` for a held tip, the root, an unknown node, a held closed head and the null node.
- **`between`** walks first parents from each pair's top, keeping the nodes met at distances 1, 2, 4, 8 and on, until
  the bottom (compared, never looked up) or the root; every SSH session's handshake asks it for the null pair, answered
  by an empty line.
- **`branchmap`** lists, per branch, `branchheads(branch, closed=True)`: heads in revision order, closed ones kept;
  the branches come in the branch cache's order, which is by name once the cache has been written (`_write` sorts
  them). Measured: `default`, `old%20feature/x`, `stable`, the name quoted with `/` safe.
- **`listkeys phases`** (`listphases`) reads the unfiltered changelog's draft roots, so a hidden draft root would be
  listed (none was here); `publishing` is listed only by a publishing repository. Measured: three draft roots, each
  `1`, no `publishing` on a `phases.publish = False` server.
- **The server's view follows its own configuration.** A server process started without the evolution options does
  not hide an obsoleted changeset ("obsolete feature not enabled but 1 markers found!"), so the probe's first run
  answered from a view the dump made with them did not show; the options now live in the served repository's hgrc.
- **Set discovery's common heads keep stale bases.** `partialdiscovery.commonheads` is `headrevs(bases)`, which removes
  only the bases' direct parents, so an earlier common base can stay beside the true common head (the Rust version,
  `dagops::heads`, the same); harmless, since `getbundle` takes them as common. The test driver holds the found set to
  the shared set by its ancestor closure.

## 19. Found while building piece 4's read serving, part 2: a `getbundle`'s answer (A-619, 2026-11-19; 7.2's code [C] in the wheel `uv` installs on the Mac Studio, `mercurial==7.2`; three sessions of a stock 7.2 client against stock `hg serve --stdio` 7.2 on the Mac Studio, `fixtures/serve`)

- **Over SSH version 1 the answer is a raw bundle2 stream**, uncompressed (no stream parameter), its parts numbered
  from 0, written after the batch's string answer; the client's next command's answer follows its end. Measured: the
  clone's `CHANGEGROUP` (version `03` mandatory, `nbchanges` 10 advisory, 4,422 bytes), `BOOKMARKS` (53),
  `LISTKEYS` (namespace `bookmarks`, 92), `OBSMARKERS` (92: one format 1 marker) and `PHASE-HEADS` (96); the pull's
  five alike (`nbchanges` 1, phase heads 120); the incoming's `CHANGEGROUP` alone (it asks `cg` and nothing else),
  then a separate `listkeys phases`.
- **`_prunemanifests` sends every manifest outside an ellipsis clone** ("better to prevent calling of store.rev and
  store.linkrev on a lot of nodes as compared to sending some extra data"); only file revisions are left out, where
  their file log's link is common (`generatefiles`). A manifest's link is the first missing changeset naming it
  (`lookupcl`'s `manifests.setdefault`). Measured: the clone's ten changesets carried nine manifests, the
  `--close-branch` commit having reused its parent's.
- **File revisions are found by reading each sent manifest's delta** (`lookupmflinknode`:
  `read_delta_parents(shallow=True, exact=False)`, the stored delta's lines where it is stored against a parent, the
  whole manifest otherwise), kept at the paths a missing changeset lists (`changedfiles`), each linked to the earliest
  missing changeset whose manifest yields it. A merge that takes one side's file unchanged lists no file: the captured
  merge's list is empty.
- **The order:** the changelog in storage order (`nodesorder=b'storage'`), the manifests and each file group by their
  log's revision (`emitrevisions`' default), the file groups by path. 7.2's deltas are against the previous revision
  or a parent; a full text against the null base is what `mdiff.trivialdiffheader` makes, and every reader takes it.
- **bundle2 is asked for by any bundle capability starting `HG2`** (`bundle2requested`); the bundle2 capabilities of
  every `bundle2=` item are merged (`b2caps.update`); with none at all no changegroup part is made; a client that lists
  no changegroup version is sent version 01 (3.1 and 3.2 sent the capability empty); the version is chosen, and
  refused ("no common changegroup version"), before `_computeoutgoing`, so even when nothing is missing.
- **`listkeys` is a `csv` argument** (`v.split(b',')`): an empty value is one empty namespace, an empty `LISTKEYS`
  part; a boolean is false for `0` and for an empty value.
- **The markers sent** are `relevantmarkers(revs=::heads)` over the unfiltered changelog (an unknown head passed over),
  sorted by `sortedmarkers`, the whole marker tuple (predecessor, successors, flags, metadata, date, parents with
  `None` as `()`), in the newest format both read (`commonversion`). Measured: the amend's one marker, its metadata
  `ef1`, `operation` `amend`, `user` `test`.
- **`HGTAGSFNODES`** is advisory and sent only for heads whose `.hgtags` file node the server's cache already holds
  (`getfnode(computemissing=False)`); none was, so no session carried one.
- **An abort on a bundle2 request is answered as a bundle2 stream** holding one `error:abort` part (the message
  mandatory, the hint advisory), not as an SSH error: owed to the stdio loop.

## 20. Found while building piece 5's first part, a push's bundle, race checks and reply (A-641 part 1, 2026-11-19; 7.2's code [C] in the wheel uv installs on the Mac Studio, CPython 3.12; captures of stock `hg` 7.2 over SSH, `fixtures/push`)

- **Files read**, with their SHA-256 there: `bundle2_part_handlers.py` (`7aabb77e10cd6160…`), `bundle2.py`
  (`90ddd2bf3fbdb781…`), `exchange.py` (`ecf56e2d9f26d017…`), `wireprotov1server.py` (`b47340cf38d24c13…`); and from the
  same wheel, not hashed: `changegroup.py`, `branchmap.py`, `error.py`, `hook.py`, `hgext/hooklib/reject_new_heads.py`.
- **A part's handler** (`bundle2.py` `_gethandler`, its two raises at 539 and 546): routed by the type in lower case; a type no handler takes,
  or a mandatory parameter outside the handler's declared names (`part.mandatorykeys - handler.params`, sorted), raises
  `BundleUnknownFeatureError(parttype=part.type, params=...)` and is re-raised only for a mandatory part. The server
  answers it with `error:unsupportedcontent` carrying `parttype` and the names joined by NUL, both mandatory
  (`wireprotov1server.py` 808-814). Captured: an unknown `TEST:UNKNOWN` part answered `parttype: test:unknown`; an
  unknown mandatory `slates-unknown` on `CHANGEGROUP` answered `parttype: changegroup, params: slates-unknown`.
- **The race checks' words** (`bundle2_part_handlers.py` 244-352): `check:heads` and `check:updated-heads` "remote
  repository changed while pushing - please try again"; `check:bookmarks` the same then ` (bookmark "%s" move from %s
  to %s)` (the node the client saw, then the server's), `(bookmark "%s" is missing, expected %s)` or `(bookmark "%s" set
  on %s, expected missing)`; `check:phases` ` (%s is %s expected %s)` (the node, its phase now, the phase expected),
  every node as `short` (twelve hexadecimal digits). `check:heads` compares sorted lists (`sorted(heads) !=
  sorted(op.repo.heads())`), and an empty repository's `heads()` is `[nullid]`; `check:updated-heads` asks
  `branchmap().all_nodes_are_heads` (`branchmap.py` 888-894: every node among every branch's heads, closed ones too, and
  `[nullid]` alone only when the repository is empty). `check:phases` reads the payload into a mapping keyed in
  `allphases` order, so its nodes are judged public first whatever the payload's order. All four were captured: the
  three races answered `error:pushraced` with exactly these words (`push-race`, `push-race-bookmark`,
  `push-race-phase`).
- **What a 7.2 client sends** (the captures' listings): `replycaps` first, then `check:bookmarks` (when it moves a
  bookmark), `check:phases` (the remote's public heads and draft roots as the client saw them, `_pushb2checkphases`,
  `exchange.py` 950-972), `check:updated-heads` (`checkheads=related`, 7.2's default) or `check:heads` (a strict
  server), `changegroup` version `03`, then `bookmarks` or `phase-heads`. A forced push sends no check at all
  (`_pushb2ctxcheckheads` 896-922 and the bookmark and phase checks skip under `force`).
- **The reply** (`bundle2_part_handlers.py` 79-147, 424-452, 538-562; `exchange.py` `unbundle`, from 2818): parts are numbered from
  0 as added; `reply:changegroup` is advisory with advisory `in-reply-to` (the changegroup part's id: 3, or 4 after a
  bookmark check) and `return`; `reply:pushkey` and `reply:obsmarkers` are mandatory with advisory parameters; an
  `output` part (advisory) follows a part whose handler printed, and one more comes last for the transaction's hooks,
  but only where output is captured: always over HTTP (`url.startswith(b'remote:http')`), over SSH only under
  `experimental.bundle2-output-capture`, so the captures over SSH hold none. A bookmark or a phase pushed alone is
  answered by a stream with no part. A refused push is answered by a new stream (salvaged `output`, then one error
  part), so a reply part made before the refusal is never sent.
- **`reply:changegroup`'s `return`** (`changegroup.py` 879-890, 969-974): the heads added less those removed less the
  added heads that close their branch, plus one, or minus one where negative: "never return 0".
- **`unbundle`'s `heads` argument** (`exchange.py` 2799-2816): `force`, the list equal to `repo.heads()` in its order
  (newest first), or `hashed` and the SHA-1 of the heads sorted and joined; judged before the payload is read ("while
  preparing changes") and again as the bundle applies ("while uploading changes"). A bundle2 push sends `force`.
- **A new head refused by 7.2's own hook** (`hgext/hooklib/reject_new_heads.py`, run as `pretxnclose`): `error:abort`
  with "Changes on branch 'default' resulted in multiple heads" and no hint. Its `%r` is bytes formatting's `%a`, the
  repr of a `pycompat.bytestr` (bytes' repr without its `b`); measured for ten names under CPython 3.12 (quotes, both
  quotes, tab, NUL, bytes from 0x7f, a backslash, line ends, UTF-8), held by the test.
- **The fixture script's first run** recorded none of the three races: the other clients pushed through the recording
  stand-in without naming a capture, the stand-in failed on the missing name, and the script took their exit status
  255 for a refused push, so each replay found the server unmoved and was accepted (`reply:changegroup` 1). The
  stand-in now records only a named capture, and only a recorded push may be refused.
- **Part 2's verification** (A-641 part 2): `revlog.py` 3831-3848 (`addgroup` passes over a node its log holds,
  "this can happen if two branches make the same change", and raises "unknown parent" and "unknown delta base");
  `changegroup.py` 2896 ("missing file data for %s:%s - run hg verify", under `server.validate`).
- **The path audit** (`pathutil.py` 87-118, SHA-256 `57d845838cc47f2f…`; `encoding.py` 56-76, `47900298a9ce5928…`):
  "path ends in directory separator" for a trailing separator; "path contains illegal component" for
  `os.path.splitdrive(path)[0]`, a first component `.hg`, `.hg.` or empty, any `..`, or a Windows short name of `.hg`
  (`first.upper() in [b"HG", b"HG8B6C"]` before `~` and digits); "path '%s' is inside nested repo %r" for `.hg` or
  `.hg.` as any later component, the components before it joined. Each comparison is `_lowerclean`'s: Python's
  ASCII-only `lower()`, then `hfsignoreclean`, which removes the UTF-8 of U+200C to U+200F, U+202A to U+202E, U+206A
  to U+206F and U+FEFF. 7.2 applies it to a working copy's writes, not to a push.

## 21. Found while building piece 3's import of an existing store and its file index (A-630 parts 2 and 3, 2026-11-19; 7.2's code [C] from Homebrew's 7.2, and runs of stock `hg` 7.2 on the Mac Studio, `fixtures/import`)

- **Which requirements 7.2 knows.** `repo/requirements.py`'s `BASE_SUPPORTED` (23), the `exp-compression-<engine>`
  names of every engine with a revlog header (`zlib`, `zstd`, and `none`, whose chunks keep a NUL) with
  `revlog-compression-zstd` (`gather_supported_requirements`), and the shipped extensions' `narrowhg-experimental`,
  `lfs` and `largefiles`: 30 in all. A requirement it does not know refuses the repository ("repository requires
  features unknown to this Mercurial"); an empty one, or one not beginning with a letter or a digit, is a corrupt file
  (`ensurerequirementsrecognized`). `scmutil.readrequires` cuts the file with `splitlines`.
- **Where a store's requirements are.** With `share-safe` in `.hg/requires` (7.2's default), the store's own are in
  `store/requires`, which must be there (`readrequires(storevfs, False)`); without it every requirement is in
  `.hg/requires` and there is no `store/requires`. Measured: the dotencode, hybrid, version 2 and file index
  repositories' `.hg/requires` hold `share-safe` alone; a repository made with `format.use-share-safe=False` holds all
  seven of its store's in `.hg/requires` and no `store/requires`, its store byte for byte the share-safe one's.
- **A file index store is named by dotencode's encoding.** `localrepo.makestore` gives a `fileindex-v1` store
  `Encoding.DOTENCODE` unless the plain encoding is required, though `dotencode` itself is refused beside
  `fileindex-v1` (as `fncache` and `treemanifest` are). Measured: the version 1 file index store's data revlogs carry
  exactly the dotencode store's names (`data/~2eleading.i`, `data/~20space.i`), and the version 2 store's dockets the
  same with `-<id>.idx`, `.dat` and `.sda` beside them, a long one hashed with its extension kept
  (`dh/upper_zzz….txt-18857273.idx<sha1>.idx`).
- **The plain encoding** (`exp-very-fragile-and-unsafe-plain-store-encoding`, 7.2's `_plain_encode`) only gives each
  directory under `data/` a trailing `_`; with `dotencode` it is refused. Its doctests are this crate's unit vectors; no
  store of it was made, since on the Studio's case-insensitive volume the fixtures' history (`FOO` removed, then
  `foo`) would fold two file logs into one file, which is the hazard its name gives.
- **An absent revlog reads empty.** `revlog._get_data` answers an absent file with `b''`, so a store without a
  changelog or manifest log is an empty repository (`hg init` writes neither), and a split revlog without its data file
  reads until a chunk is needed. `verify` then calls a file log a changeset names but that is empty or missing an error
  ("empty or missing"), a changeset whose manifest the manifest log lacks one too, and a listed log no manifest names
  an orphan, a warning.
- **The file index on disk** (`helptext/internals/file-index.txt`, `store_utils/file_index_util.py`): a 60-byte fixed
  docket (`>12s3I8s8s8s3I`: the marker, three used sizes, three ids, the root's pointer, the unused bytes, flags 7.2
  zeroes), then a garbage list (`>II` and `>HIIH` entries, a buffer of old files' paths). The list, meta and tree files
  are `fileindex-list.<id>`, `-meta.` and `-tree.`, opened unencoded at the store's root; an id of eight `0`s names no
  file. The paths are the meta records from token 1 (`items`); the tree serves `get_token`. 7.2 checks the root only
  (token 0, an empty label, children when the tree file is not empty) and slices short garbage lists, children and
  paths without a word. Measured (`hg debug::file-index --docket`): 22 paths, a list of 555 bytes, a meta of 184 (23
  records with the root's), a tree of 198 with its root at 27 and 27 bytes no node reaches (a tree is appended to,
  never rewritten in place, until it is vacuumed), no garbage entries.
- **Deterministic dockets.** `utils/docket.py` makes every new id from the file `HGTEST_UUIDFILE` names when that is
  set (7.2's own test suite's hook), and `devel.fileindex.garbage-timestamp` fixes a garbage entry's time, so the
  fixtures' version 2 and file index stores are the same on every run; their ids come from one chain of seeds (the file
  index store's list file and the version 2 store's changelog index are both `150e1cfc`).
- **Revision flags.** 7.2 refuses a revision whose flags hold a bit outside `REVISION_FLAGS_KNOWN`, bits 6 to 15
  (`flagutil.processflagsraw`: "incompatible revision flag"); of the known ones, censored, ellipsis and externally
  stored change what the text is (`REVIDX_RAWTEXT_CHANGING_FLAGS` and the ellipsis processor), and the rest are
  information (`hasmeta`, the copies' sidedata, the delta's quality). A file log with `hasmeta` keeps its texts as
  without it (`filelog.add` sets the flag whenever it writes a block), so a text's block is split from its bytes the
  same way.
- **A first run of the import tests, red then green.** Against the stubs, 21 of the 224 lib tests failed, each by its
  assertion; with the import, 224 of 224 at the first build.

## 22. Found while building piece 4's read serving, part 3: the SSH version 1 session (A-619, 2026-11-19; 7.2's code [C] in the wheel `uv` installs on the Mac Studio, `mercurial==7.2`; the three captured sessions, `fixtures/serve`)

- **`_runsshserver`** reads a command line; an empty one ends the session; a name its table lacks
  (`commandavailable`) is answered with an empty string and no argument read; any other has its arguments read by its
  spec (`getargs`: `*` a count then that many `key length` lines) and answers bytes (`<length>\n<value>`), a stream
  (raw), a push result (two strings) or an out-of-band error (`\n` on stdout, the message and `\n-\n` on stderr).
- **The table** (21 commands): `batch` (`cmds *`), `between` (`pairs`), `branches` (`nodes`), `branchmap`,
  `capabilities`, `changegroup` (`roots`), `changegroupsubset` (`bases heads`), `clonebundles`, `clonebundles_manifest`
  (`*`), `debugwireargs` (`one two *`), `get_cached_bundle_inline` (`path`), `getbundle` (`*`), `heads`, `hello`,
  `known` (`nodes *`), `listkeys` (`namespace`), `lookup` (`key`), `protocaps` (`caps`), `pushkey` (`namespace key old
  new`, push), `stream_out`, `unbundle` (`heads`, push).
- **`hello`** is `capabilities: ` and `capabilities`' answer, the sorted list, then a newline; `protocaps` answers `OK`.
  Measured: 7.2's hello on the captured server, 537 bytes, `batch branchmap bundle2=… changegroupsubset getbundle known
  lookup protocaps pushkey streamreqs=… unbundle=HG10GZ,HG10BZ,HG10UN unbundlehash`.
- **`batch`** splits `cmds` at `;`, each call at its first space, its arguments at `,` and `=`, unescapes them, runs the
  command (any in the table, its permission checked; a stream answer asserts) and joins the escaped answers with `;`.
  Measured: `heads ;known nodes=` answered `124` bytes, the three heads, `;`, and an empty `known`.

## 23. Found while importing a repository's state with its history (A-649 part 2, 2026-11-19; 7.2's code [C] from Homebrew's 7.2, and runs of stock `hg` 7.2 on the Mac Studio, `fixtures/state`)

- **`phaseroots` is read strictly** (`phases._readroots`): each line is `line.split()` into exactly a phase and a node,
  `int` and `bin` of each, and `roots[phase]` over `allphases`, so a line of another shape, a phase 7.2 does not define
  or a node that is not hexadecimal aborts the command; a root whose node the changelog lacks is dropped with a debug
  message ("removing unknown node … from … boundary"). An absent file means every changeset is public. Measured: the
  history's file holds two roots, `1` for the first draft and `2` for the secret changeset, and the amended draft's
  hidden predecessor is draft by descent, not by a root of its own.
- **The bookmarks are read leniently** (`bookmarks.bmstore`): each line `strip`ped, an empty one passed over, `split(b'
  ', 1)` into a node and a name, `bin` of the node; a line without the space, or digits `bin` refuses (odd, not
  hexadecimal), is passed over with "malformed line in .hg/bookmarks", and a node the changelog lacks (a short one
  among them) silently; a name given twice keeps its later node. The file is `.hg/bookmarks`, or the store's own under
  `bookmarksinstore` (`format.bookmarks-in-store`), and a bookmark may name a secret changeset (measured: `secret-one`).
- **The obsstore** (`obsolete.obsstore`) is read only when it holds bytes, so an empty file is no marker; it is in the
  store's own format (here format 1, 7.2's default), markers in the order they were added. Measured: an amend's marker
  (predecessor, successor, `ef1`, `operation` and `user` metadata) and a prune of a node never held (no successor).
- **Mutable state and immutable rows.** The shared index never rewrites a row, while the phase roots and bookmarks move
  with every push, so they are the register's (A-642), and the import hands them back typed; markers, never changed once
  made, are the index's rows.

## 24. Found while landing pushes (A-657, A-641 part 3, 2026-11-19; 7.2's code [C] in the wheel uv installs on the Mac Studio, `mercurial==7.2`, CPython 3.12, read with `inspect.getsource`; a run of stock `hg` 7.2 on the Mac Studio; the push fixtures' texts rebuilt)

- **Phases on a push** (`changegroup.py` `cg1unpacker.apply`, `srctype` `serve`): a publishing repository takes
  `targetphase = phaseall = public`, a non-publishing one `draft` ("Those changesets have been pushed from the outside,
  their phases are going to be pushed alongside. Therefor `targetphase` is ignored."); then `registernew(targetphase,
  added)` and `advanceboundary(phaseall, revs=duprevs + added)`, `duprevs` the changesets the bundle carried that the
  repository held (`ondupchangelog`, `rev < clstart`). So a publishing push publishes every draft ancestor of what it
  carried, held ones included.
- **`advanceboundary`** (`phases.py` `phasecache.advanceboundary`): the given revisions above the target phase walked
  down through every parent above it (a heap, highest first); a walked revision whose phase differs from both parents'
  is a root removed; then every unwalked revision from the lowest walked one to the end, above the target, with a parent
  walked and no unwalked parent of its own phase, is a new root of its phase. `updatephases` (the `phase-heads` part)
  advances, phase by phase in `allphases` order, the heads not already in their phase. `pushphase` (the `phases`
  pushkey): advances when the node's phase is the old one and the new one is lower (1), answers 1 when the node is in
  the new phase already ("raced, but got correct result"), else 0.
- **The change in heads** (`apply` and `revlog.headrevsdiff`, `dagop.headrevsdiff`): `len(heads_added) -
  len(heads_removed)`, less every added head that closes its branch, then `+ 1`, or `- 1` when negative. The heads are
  the changelog's, and from an empty changelog `headrevsdiff(0, n)` counts the null revision as removed (its loop starts
  at `start - 1`, -1), so a first push of one head answers 1. The `pretxnchangegroup` hook runs inside `apply`, with
  `node` and `node_last` the first and last added changesets, and only when it added some.
- **Bookmarks** (`bundle2_part_handlers.py` `handlebookmark`, `bookmarks.py`): the `bookmarks` part is applied as it
  is in mode `apply` (`bmstore.applychanges`: a node sets, none deletes); its race is the earlier `check:bookmarks`'s; a
  divergent name (`isdivergent`: an `@` not at the end) aborts "cannot accept divergent bookmark %s!". With
  `server.bookmarks-pushkey-compat` the part runs `prepushkey` and `pushkey` hooks per bookmark. The `bookmarks`
  pushkey (`pushbookmark`): refused for a divergent name; 0 when the bookmark's hex is neither the old value nor the
  new; an empty new value deletes; a new node the repository lacks is 0. A mandatory `pushkey` part that answers 0
  raises `PushkeyFailed` with the part's `namespace`, `key`, `new`, `old` and `ret` parameters as sent.
- **Hooks' words** (`hook.py` `_exthook`, `procutil.explainexit`): an external hook's environment is `HG_PENDING` (the
  repository's root, when the transaction has pending writes), `HG_HOOKTYPE`, `HG_HOOKNAME`, `HGPLAIN` (empty unless
  configured) and `HG_<ARG>` for each argument; it runs in the repository's root; a non-zero status at a point that
  throws is `HookAbort("%s hook %s" % (name, desc))`, `desc` "exited with status %d" or "killed by signal %d"; at a point
  that does not throw, `warning: %s hook %s` is printed and the transaction stands.
- **Branch heads by descent, checked by a run** (Mac Studio, stock 7.2, a repository of `0` on `default`, `1` on
  `stable` from it, `2` on `default` from `1`): `hg heads default` lists `2` alone, so a branch head is dropped when a
  changeset of its branch descends from it through any branch, as `branchmap`'s update drops the ancestors of new heads.
  slates-hg's `serve::discovery::branch_head_revs` drops a head only for a child on its branch, and would list `0` and
  `2` (reported to main as a sibling of A-657, not changed here).
- **The push fixtures' repository rebuilt** (`make-push-fixtures.sh`, user `test`, date `0 0`): each changeset's text
  `{manifest hex}\ntest\n0 0\n{file}\n\n{description}`, each manifest `{path}\0{file node hex}\n` sorted, each file a
  new file's bytes; the nodes computed from them are 7.2's (`base` ad120869…, `second` 59d5dc23… with manifest
  a539ce0c…, `other` f2563910…, `plain` 048741fa… with manifest 8b52510d…), first checked with `python3 -I` and
  `hashlib`, then by the server's tests.

## 25. Found while serving HTTP version 1 (A-650 part 1, 2026-11-19; 7.2's code [C] in the wheel `uv` installs on the Mac Studio, `mercurial==7.2`, `__version__.py` reading `7.2`; a stock 7.2 client's clone, pull and incoming over HTTP against stock `hg serve`, `fixtures/serve-http`)

- **What is a wire request** (`wireprotoserver.handlewsgirequest`). One whose query holds `cmd` naming a command of
  the version 1 table (`iscmd`); any other request is hgweb's. `req.qsparams[b'cmd']` reads a `multidict`, whose
  `__getitem__` "Returns the last set value for a key" (`hgweb/request.py`), so 7.2 serves a query's last `cmd`:
  A-569's codec read the first, and is corrected in A-650. A wire `cmd` on a path within the repository (a
  `dispatchpath`) is answered `404` with `0\nNot Found\n`; a permission refusal (`ErrorResponse`) with its status and
  `0\n<message>\n`.
- **The answer** (`_callhttp`). A command the table holds that the transport does not is `200` `application/hg-error`
  "requested wire protocol command is not available over HTTP"; the command's permission is checked before it runs;
  then a string is `200 Script output follows` in `application/mercurial-0.1`; a stream (`streamres`) is compressed as
  `_httpresponsetype` chooses and, for `0.2`, framed by `genversion2` (one byte giving the engine's name's length, then
  the name); a push's answer is `<result>\n<output>`, a push's refusal `0\n<message>\n` with the body drained; an
  out-of-band error is `application/hg-error`, its message the body.
- **The compression** (`_httpresponsetype`). To a client whose `X-HgProto` holds `0.2`: `none` where the answer prefers
  uncompressed ("All clients are expected to support uncompressed data"), else the first engine of the server's list
  (`supportedcompengines` for the server's role: engines whose server priority is above 0, zstd then zlib) that the
  client names (`clientcompressionsupport`: the first `comp=` token's list, else `zlib` and `none`), at
  `server.<engine>level` (`zstdlevel` 3, `zliblevel` -1 by `configitems.toml`); with no engine shared, or to a client
  that does not read `0.2`, `application/mercurial-0.1` in zlib at `server.zliblevel`.
- **The capabilities** (`httpv1protocolhandler.addcapabilities`) add to `_capabilities`' list `batch`,
  `httpheader=<server.maxhttpheaderlen>` (1024 by default), `httppostargs` when `experimental.httppostargs` is set,
  `httpmediatype=0.1rx,0.1tx,0.2tx` and `compression=` the server's engines by wire name; `capabilities` answers the
  list sorted and joined by spaces. `protocaps` is advertised over SSH alone. The arguments (`_args`): the query's
  fields as a dictionary of lists, updated by the `X-HgArgs-Post` front of the body or the `X-HgArg-<N>` values
  joined, a name both carry taking the later's list; `getargs` reads each list's first value.
- **`getbundle` judges bundle1 first** (`wireprotov1server.getbundle`). A request naming no bundle capability that
  starts `HG2` is refused before anything else: over HTTP `ooberror(bundle2required)`, the message "incompatible
  Mercurial client; bundle2 required" and the hint "(see https://www.mercurial-scm.org/wiki/IncompatibleClient)", each
  ended by a line end; over SSH an abort. Then `exchange.getbundlechunks` decodes the bundle2 capabilities before it
  runs the parts' generators, the changegroup's among them, which compute what is missing. A-619's `getbundle`
  computed what is missing first, so a bundle1 client naming a head the view lacks was answered with a bundle2 abort it
  cannot read; A-650 judges the capabilities first, in 7.2's order. An abort of a bundle2 request is an `error:abort`
  part with its message and hint, sent uncompressed (`prefer_uncompressed`).
- **hgweb's permissions** (`hgweb/common.py` `checkauthz`, run for every wire command with its table's permission:
  `unbundle` and `pushkey` push, every other pull). `deny_read`, then `allow_read`: `401` "read not authorized"; a pull
  where pulls are off: `401` "pull not authorized"; a push by any method but `POST`: `405` "push requires POST
  request"; `push_ssl` without HTTPS: `403` "ssl required"; `deny_push`, then `allow-push`: `401` "push not
  authorized". The status line carries the message (`statusmessage`). slates keeps the order and answers a principal
  its policy refuses `403`, since its credentials were proved.
- **The room a compressed answer is charged** (`compression::bound`, charged before the answer is compressed). Each
  engine's own documented worst case: libbzip2's "1% larger than the uncompressed data, plus six hundred extra bytes"
  (`BZ2_bzBuffToBuffCompress`; libbz2-rs-sys is its port), libzstd's `ZSTD_compressBound`, and for zlib the bound of the
  backend the workspace links (A-607): zlib-rs 0.6.8's `compress_bound`, zlib-ng's `compressBound`,
  `len + ceil(len / 8) + 3 + 6`, a byte more below 9 bytes and one more for none. Stock zlib's `compressBound`
  (`len + (len >> 12) + (len >> 14) + (len >> 25) + 13`, `compress.c`) is no bound there: it counts stored blocks, which
  stock zlib falls back to on bytes that do not compress, while zlib-rs's level 1 is zlib-ng's `deflate_quick`
  (`src/deflate/algorithm/quick.rs`), which writes every literal in fixed codes, nine bits for the literals 144 to 255
  (RFC 1951 §3.2.6), and never falls back. Measured (Windows x86_64, jupiter, kiln r92, and macOS arm64, Mac Studio,
  kiln r181, alike, 2026-11-19: the oracle `the_longest_codes_stay_within_every_engines_bound` run with stock zlib's
  bound in zlib-rs's place): level 1 wrote 64 of those bytes in 80 (stock zlib's bound 77), 4,096 in 4,617 (its bound
  4,110) and 65,536 in 73,721, 8,152 bytes (12.4%) past its 65,569; every other zlib level stayed within it. zlib-rs's
  bound for the same lengths is 81, 4,617 and 73,737, and level 1 met it exactly at 4,095 and 4,096 bytes: the bound is
  the worst case itself, with nothing to spare.
- **Measured** (`fixtures/serve-http`, `make-http-fixtures.sh`, the repository of `fixtures/serve` made the same way;
  its states, bookmarks and incoming output equal byte for byte). The clone: `capabilities` (582 bytes:
  `batch branchmap bundle2=… changegroupsubset compression=zstd,zlib getbundle httpheader=1024
  httpmediatype=0.1rx,0.1tx,0.2tx known lookup pushkey streamreqs=… unbundle=HG10GZ,HG10BZ,HG10UN unbundlehash`), a
  `batch` of `heads` and an empty `known`, and `getbundle` answered `application/mercurial-0.2` in zstd (1,952 bytes).
  The pull: the same three, the `batch`'s `known` naming the client's three heads, the `getbundle` 716 bytes. The
  incoming: the same three (its `getbundle` 338 bytes) and `listkeys` of the phases. The client sent
  `X-HgProto-1: 0.1 0.2 comp=zstd,zlib,none,bzip2 partial-pull` with every command after `capabilities`, and its
  arguments in `X-HgArg-1` alone, each request by `GET`.

## 26. Found while building the served view (A-680 part 1, 2026-11-19; 7.2's code [C] in the wheel `uv` installs on the Mac Studio, `mercurial==7.2`, CPython 3.12; stock 7.2's `served` filter over `fixtures/state`'s two repositories, `fixtures/state/make-served-fixtures.sh`)

- **`served` is two filters joined** (`repoview.computeunserved`): the revisions `visible` hides and those
  `served.hidden` hides (`computesecret`: the `remotehiddenphases`, secret 2, archived 32 and internal 96). `hg serve`
  answers every peer from it (`wireprotov1server.getdispatchrepo`, `server.view` at its default).
- **`visible`** (`computehidden`): `hideablerevs` (the obsolete revisions and those of the `localhiddenphases`, internal
  and archived) less `pinnedrevs` (the working directory's parents, every bookmark's changeset, the local tags' and,
  while files are unresolved, an active merge's two sides); then `_revealancestors` from every revision of a
  `mutablephases` phase (draft, secret, archived, internal) that is not hidden: each hidden parent leaves the hidden set
  and is walked in turn, on a stack, so a chain of hidden ancestors is revealed through hidden ones. A public revision
  reveals nothing (it is not mutable) and needs nothing revealed (its ancestors are public, never hideable).
- **Obsolete** (`obsolete._computeobsoleteset`): a revision of the `relevant_mutable_phases`, draft and secret, whose
  node is a key of `obsstore.successors`, which keys every marker by its predecessor, a prune's too. An archived or
  internal revision is hideable by its phase alone.
- **Phase roots** (`phases._readroots`): a root whose node the changelog lacks is dropped (`removing unknown node %s
  from %i-phase boundary`) and the roots marked dirty, never refused.
- **The captures.** Each repository made again from its committed store, `.hg/requires` and `.hg/bookmarks` (its nodes
  the fixtures' own) and opened with evolution on and publishing off: `served` keeps `c3daaa7` (public), `baa2576` and
  `b252f2a` (draft), and leaves out `a1286ad` (hidden: amended into `b252f2a`, no bookmark on it, no visible descendant)
  and `4ed9423` (secret); `heads` and `branchmap` answer `b252f2a` alone; `listkeys bookmarks` answers `one` and `two`,
  never `secret-one`, whose changeset is secret; `listkeys phases` answers the one draft root, `baa2576`. Both
  repositories (bookmarks in `.hg/bookmarks`, and in the store) answer alike.
