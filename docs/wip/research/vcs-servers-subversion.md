# The Subversion server on greens and works: reading notes (primary sources only)

Status: written 2026-11-18 by g292 for A-500 (UI goal 91), the evidence behind §4.17.4 of the design. It builds on
`research/vcs-servers.md` (g261, A-497), which holds the shared mapping's evidence (§2.3 there quotes the Subversion
lines §4.17.1 rests on). Evidence tiers as in the design: B for the project's own specifications (`notes/`, the public
headers' documentation), C for its deployed code. Every quotation was read directly on 2026-11-18 from the revision
named, at the lines given.

## 1. Sources and revision

Apache Subversion's `1.14.x` branch (the release line stock clients run; 1.14 is the current LTS line) at
`d434dc14866e0c48602dba59a15443255d0b8c1e` (2026-11-01), fetched from `raw.githubusercontent.com/apache/subversion/
1.14.x/<path>` (the GitHub mirror of the Apache repository), plus two release-notes pages and one book page fetched from
`subversion.apache.org` and `svnbook.red-bean.com`. The first 16 hex digits of each file's sha256:

| File | sha256 (prefix) | Tier |
|---|---|---|
| `subversion/libsvn_ra_svn/protocol` | `b117e0ccc2008627` (the same bytes as trunk at `fd4e82b`, which `vcs-servers.md` read) | B |
| `notes/svndiff` | `a0ce3692010af9a5` | B |
| `notes/http-and-webdav/http-protocol-v2.txt` | `635d8f34636facae` | B |
| `notes/http-and-webdav/webdav-protocol` | `69c8e1c5f8bd8698` | B |
| `subversion/libsvn_fs_fs/structure` | `2063ef7b0e24642e` | B |
| `subversion/include/svn_fs.h` | `137781a3684cd921` | B |
| `subversion/include/svn_mergeinfo.h` | `129d6739fee73856` | B |
| `subversion/include/svn_error_codes.h` | `6d10e491f805222a` | B |
| `subversion/include/svn_ra_svn.h` | `574ea2a60916edb6` | B |
| `notes/locking/locking-functional-spec.txt` | `15408f68f7f331b5` | B |
| `subversion/tests/cmdline/README` | `b4dc08b968167424` | B |
| `subversion/libsvn_repos/commit.c` | `1727532bffd25017` | C |
| `subversion/libsvn_fs_fs/tree.c` | `5f4e47d132ef9e02` | C |
| `subversion/libsvn_fs_fs/lock.c` (read 2026-11-19 for A-602) | `45d641aea0fe524c` | C |
| `subversion/libsvn_fs_fs/transaction.c` (read 2026-11-19 for A-602) | `8d99770ee4faa1e5` | C |
| `subversion/libsvn_fs/fs-loader.c` (read 2026-11-19 for A-602) | `afde9ced13069c16` | C |
| `subversion/libsvn_subr/xml.c` (read 2026-11-19 for A-602) | `aeffb5c3effa073f` | C |
| `subversion/libsvn_subr/ctype.c` (read 2026-11-19 for A-602) | `8c00533a27dc55f5` | C |
| `subversion/include/svn_ctype.h` (read 2026-11-19 for A-602) | `2928f028e729a7b1` | B |
| `subversion/libsvn_fs_fs/fs_fs.c` (read 2026-11-19 for A-592) | `b12b91eb0418577d` | C |
| `subversion/libsvn_fs_fs/dag.c` (read 2026-11-19 for A-592) | `9efbb054b8818693` | C |
| `subversion/libsvn_subr/path.c` (read 2026-11-19 for A-592) | `b234d99acb31b426` | C |
| `subversion/libsvn_subr/sorts.c` (read 2026-11-19 for A-592) | `4fe2371630f6f71a` | C |
| `subversion/libsvn_subr/dirent_uri.c` (read 2026-11-19 for A-592) | `fe05687a2dedb22c` | C |
| `subversion/include/svn_dav.h` (read 2026-11-19 for A-609) | `f4740ee24bf1864d` | B |
| `subversion/include/private/svn_dav_protocol.h` (A-609) | `10fa33c966fe4a8f` | B |
| `subversion/libsvn_ra_serf/update.c`, `merge.c`, `get_deleted_rev.c` (A-609) | `b78f28a0945d7499`, `ac378386d20faff9`, `b7f6704777bccf62` | C |
| `subversion/mod_dav_svn/version.c`, `merge.c`, `util.c`, `liveprops.c` (A-609) | `3904f5a2f4028987`, `e8a9d878febb05d2`, `e27d11d47e16b983`, `f96288c11c209fe6` | C |
| `subversion/mod_dav_svn/reports/update.c`, `log.c`, `replay.c`, `get-locks.c` (A-609) | `172174063d0d87bc`, `3b2ec519b33951f9`, `af77144b5161ebf3`, `1279f8eb1374d28e` | C |
| `subversion/mod_dav_svn/reports/mergeinfo.c`, `inherited-props.c`, `get-locations.c`, `get-location-segments.c` (A-609) | `094b9d34c12744ea`, `a1ed27b41e1cd5b9`, `681da4c234580a41`, `a5127778d80cac58` | C |
| `subversion/mod_dav_svn/reports/dated-rev.c`, `deleted-rev.c` (A-609) | `c45708c8997cfbf5`, `639915799a79a818` | C |
| `subversion/libsvn_subr/base64.c`, `time.c` (A-609) | `5e7c46c46c4ea17f`, `c58645b206f847e8` | C |
| httpd 2.4.66 `modules/dav/main/mod_dav.h`, `mod_dav.c` (A-609) | `ad8361ef4e7e311c`, `2cab686b045e2237` | C |
| `subversion/mod_dav_svn/reports/list.c`, `file-revs.c`, `mod_dav_svn/lock.c` (A-627) | `9c852cb6ba67f446`, `6efa4630426dabd0`, `c5bc4498df507c4f` | C |
| httpd 2.4.66 `modules/dav/main/util_lock.c`, `props.c` (A-627) | `6b26b0425b33ea20`, `92a3201fa69632e6` | C |
| `subversion/libsvn_delta/svndiff.c` | `0ae17243dc9ab11b` | C |
| `subversion/libsvn_delta/delta.h` | `31ea1b30e27321f8` | C |
| `subversion/libsvn_subr/encode.c` | `3e5752e741f7aacd` | C |
| `subversion/libsvn_subr/compress_lz4.c` | `c50b1552af322b84` | C |
| `subversion/libsvn_subr/compress_zlib.c` | `35baff627fb62cd5` | C |
| `subversion/libsvn_ra_svn/marshal.c` | `908ccdd914cae15b` | C |
| `subversion/svnserve/svnserve.c` | `43155c2bd557492f` | C |
| `subversion/svnserve/serve.c` | `acb1cac75ea86ba7` | C |
| `subversion/libsvn_ra_svn/cram.c` | `64e6271eacf19286` (A-599) | C |
| `subversion/libsvn_ra_svn/internal_auth.c` | `61f373448a9c878c` (A-599) | C |
| `subversion/libsvn_ra_svn/client.c` | `3c3b9263dd7223a9` (A-599) | C |
| `subversion/libsvn_ra_svn/editorp.c` (read 2026-11-19 for A-617; 1.15.0's is the same bytes) | `610cffcd14b17ecf` | C |
| `subversion/mod_dav_svn/repos.c` | `940bc5282be7b763` | C |
| `subversion/libsvn_repos/hooks.c` | `96392d3013a7f6bb` | C |
| `subversion/libsvn_repos/log.c` | `d5f97de3e455314b` | C |
| `subversion/libsvn_repos/fs-wrap.c` | `126ab4670c8c23e0` | C |
| `subversion/tests/cmdline/svntest/main.py` | `b570d555bd82aaee` | C |

Also read: `svn_repos.h`, `svn_delta.h`, `svn_props.h`, `svn_types.h`, `libsvn_repos/reporter.c`, `delta.c`,
`rev_hunt.c`, `replay.c`, `authz.c`, `authz_parse.c`, `libsvn_fs_fs/lock.c`, `transaction.c`, `mod_dav_svn/version.c`,
`reports/update.c`, `notes/merge-tracking/` and `notes/locking/` (same revision); the 1.10 and 1.14 release notes
(`subversion.apache.org/docs/release-notes/1.10.html`, `1.14.html`, tier B); the book's path-based authorization page
(`svnbook.red-bean.com/en/1.7/svn.serverconfig.pathbasedauthz.html`, tier B); RFC 2195, CRAM-MD5
(`www.rfc-editor.org/rfc/rfc2195.txt`, tier B).

For the handshake (A-599, 2026-11-19), RFC 2104 (HMAC, sha256 `64d5245a91019290`) and RFC 2202 (its HMAC-MD5 test
cases, `c19effeca47e801b`), tier B; and the files of the release the fixtures were recorded with, Subversion 1.15.0,
from `raw.githubusercontent.com/apache/subversion/1.15.0/<path>`, tier C: `libsvn_ra_svn/cram.c`, `internal_auth.c` and
the protocol document byte-identical to `d434dc14`'s above; `svnserve/serve.c` `8154654e07aa4eb4`, which differs from
`d434dc14`'s by a Windows include and a comment; `libsvn_ra_svn/marshal.c` `361702975baa8c70`; `client.c`
`b1d9607af70baf9c`; `cyrus_auth.c` `3bd4ba09916282fd`.

For the reporter (A-616, 2026-11-19), at `d434dc14`, tier C: `libsvn_repos/reporter.c` `211be3256679d87f`,
`libsvn_ra_svn/editorp.c` `610cffcd14b17ecf`, `libsvn_repos/delta.c` `0b150501bcdcec51`, `libsvn_delta/text_delta.c`
`fc69ad86f58de9b9`, `include/svn_repos.h` `8421f33e065fd927` and `include/svn_delta.h` `809dd92780dd3c1d`; at 1.15.0:
`reporter.c` `5657b77e3ad25d3f` (two comments and a hash constructor apart), `editorp.c` and `delta.c` byte-identical,
and `libsvn_subr/dirent_uri.c` `fe05687a2dedb22c`, `path.c` `413c3a8a740ae18c` and `types.c` `8c9fe593b18d24cf`.

## 2. The wire: ra_svn (`svn://`, and `svn+ssh://` through a tunnel)

- **The grammar** [B: `protocol` 9-15]: `item = word / number / string / list`; `word = ALPHA *(ALPHA / DIGIT / "-")
  space`; `number = 1*DIGIT space`; `string = 1*DIGIT ":" *OCTET space` ("digits give the byte count of the *OCTET
  portion"); `list = "(" space *item ")" space`; `space = 1*(SP / LF)`. "All items end with mandatory whitespace" (21),
  and "it is possible to parse an item without knowing its type in advance" (23). The spec's own example (19):
  `( word 22 6:string ( sublist ) )`. Extra items in a tuple "must be ignored" (66-68).
- **What the deployed parser refuses** [C: `marshal.c`]: a word of 25 or more characters (`MAX_WORD_LENGTH 25`, 66; the
  buffer's end is the refusal "Word is too long"; the spec's §4.1 still says 31, 627: the code is stricter); nesting at
  64 levels (`ITEM_NESTING_LIMIT 64`, 73, "Items are nested too deeply"); a number past 2^64 ("Number is larger than
  maximum"); anything but space or LF after an item ("Malformed network data"). A long string is read 1 MiB at a time
  (`SUSPICIOUSLY_HUGE_STRING_SIZE_THRESHOLD`, 59) "to ensure we do not pre-allocate an unreasonable amount of memory"
  and is refused past the connection's `max_in` (`SVN_ERR_RA_SVN_REQUEST_SIZE`); svnserve's default for it is 16 MB
  (`svnserve.c` 165, `MAX_REQUEST_SIZE 16`), a constant.
- **Setup** [B: `protocol` 98-176]: the server greets with `( success ( minver maxver mechs ( cap ... ) ) )`; the client
  answers version, its capabilities and the URL; the server sends an auth request (SASL mechanism names and a realm),
  then `repos-info: ( uuid repos-url ( cap ... ) )`. svnserve's greeting [C: `serve.c` 4239-4256] is version 2 to 2
  with `edit-pipeline svndiff1 accepts-svndiff2 absent-entries commit-revprops depth log-revprops atomic-revprops
  partial-replay inherited-props ephemeral-txnprops file-revs-reverse list`; `mergeinfo` is a repository capability, in
  `repos-info` (`protocol` 169-176, 201-202).
- **Authentication** [B: `protocol` 98-176; C: `serve.c` 580-721 and 4209-4433, `cram.c`, `internal_auth.c`;
  M: `crates/svn/fixtures/handshake/`, stock 1.15.0's sessions, A-599]. The auth request lists `ANONYMOUS` (anonymous
  read allowed), `EXTERNAL` (a tunnel's user, authenticated read allowed) and `CRAM-MD5` (a password database,
  authenticated read allowed), in that order (`send_mechs`); a repository allowing none is refused "No access allowed
  to this repository" (170001) before any request. `auth` takes `EXTERNAL` as the tunnel's user unless its token names
  another ("Requested username does not match"), `ANONYMOUS` as no one, its token ignored, and `CRAM-MD5` by a
  challenge `<random.microseconds@host>` (`cram.c` 154-156), the response a string `user hexdigest` split at its last
  space with exactly 32 lowercase digits after it ("Malformed client response in authentication"), then "Username not
  found" or "Password incorrect"; a response that is no string is passed over without a word. Any other mechanism is
  "Must authenticate with listed mechanism". A failure lets the client answer again, with no limit
  (`internal_auth_request`'s loop); a success writes `( success ( ) )` and `repos-info`. The answer to the greeting is
  read as `nlc?c(?c)`: a `ra-client` without the client list after it is "Malformed network data" (recorded), and every
  refusal before `find_repos` ends the connection with nothing written, the ones after it with a command failure whose
  file is empty and line 0 in a release build (recorded). HMAC-MD5 in `compute_digest` hashes a password longer than 64
  bytes first, as RFC 2104 does (recorded with 64- and 70-byte secrets). A client built with Cyrus SASL, as Homebrew's
  1.15.0 is, sends `ANONYMOUS` base64 of `anonymous@` and its host with a line feed, and `CRAM-MD5` unencoded
  (`cyrus_auth.c` 498-505, 553-555).
- **The reporter** [C: `serve.c` 855-1050 and 1948-2160, `reporter.c`, `editorp.c`; M: `crates/svn/fixtures/report/`,
  stock 1.15.0's sessions, A-616]. A stock client sends `update ( ( rev ) target recurse depth copyfrom ancestry )` with
  depth `unknown` unless asked, both flags false; `switch` adds its URL; `status ( target recurse ( rev ) depth )`;
  `diff` reparents to the root and names its target and versus URL. Its report is `set-path ( path rev start-empty
  ( token? ) depth )`, a `delete-path` for each absent or not-present path, a `link-path ( path url rev ... )` for a
  switched subtree, then `finish-report`, siblings in no fixed order between runs. `svnserve` answers the command's and
  `finish-report`'s trivial auth requests, then drives: `target-rev`, `open-root ( ( rev ) d0 )`, each directory's
  entry properties (`svn:entry:committed-rev`, `committed-date`, `last-author`, `uuid`, in that order) before its
  others, deletions first, then entries in lexical order; tokens `d` or `c` and one counter from 0; a text as
  `apply-textdelta ( token ( base-md5? ) )`, a 4-byte `SVN\x02` chunk and three chunks a window (header, instructions,
  new data), `textdelta-end`, `close-file ( token ( md5 ) )`; an added file's new data in 102,400-byte windows;
  `status`'s texts the header chunk alone; `delete-entry ( path ( rev? ) token )` with the revision of deletion, none
  where the path never existed there; `absent-dir` where authorization hides a path; a valid lock token skips its
  file, a broken one opens it to delete `svn:entry:lock-token`. `close-edit` waits for the client's `( success ( ) )`,
  then `( success ( ) )`; a client's failure there is answered `abort-edit` and the failure returned; input waiting
  before any editor command is answered `abort-edit` (`check_for_error`).
- **Commands** [B: `protocol` 216-607]: the main set (reparent, get-latest-rev, get-dated-rev, change-rev-prop(2),
  rev-proplist, rev-prop, commit, get-file, get-dir, check-path, stat, get-mergeinfo, update, switch, status, diff, log,
  get-locations, get-location-segments, get-file-revs, lock(-many), unlock(-many), get-lock(s), replay(-range),
  get-deleted-rev, get-iprops, list), the editor set (target-rev, open-root, delete-entry, add-dir, open-dir,
  change-dir-prop, close-dir, absent-dir, add-file, open-file, apply-textdelta, textdelta-chunk, textdelta-end,
  change-file-prop, close-file, absent-file, close-edit, abort-edit, finish-replay) and the report set (set-path,
  delete-path, link-path, finish-report, abort-report). An edit answers once, at its end, but "the consumer may write an
  error response at any time during the edit ... the driver must notice that input is waiting on the connection, read
  the error, and send an abort-edit" (514-517), with non-blocking sends "to prevent TCP deadlock" (518-521).
- **Which svndiff a sender uses** [C: `marshal.c` 269-284]: version 2 if the peer announced `accepts-svndiff2`, else 1
  if it announced `svndiff1`, else 0, and 0 whenever the sender's own compression level is 0. So a server that never
  announces `svndiff1` never receives a zlib stream on this protocol, and may always answer in version 0.

## 3. The wire: HTTP (mod_dav_svn's WebDAV/DeltaV, and HTTPv2)

- **HTTPv2** [B: `http-protocol-v2.txt`]: "Map RA requests directly to HTTP requests" (requirements). OPTIONS answers
  `SVN-Youngest-Rev`, `SVN-Me-Resource` and the stubs `SVN-Rev-Stub`, `SVN-Rev-Root-Stub`, `SVN-Txn-Stub`,
  `SVN-Txn-Root-Stub`, `SVN-VTxn-Stub`, `SVN-VTxn-Root-Stub`, whose presence "tells ra_serf that this is a new server";
  the client builds `!svn/me`, `!svn/rev/REV`, `!svn/rvr/REV/PATH`, `!svn/txn/TXN`, `!svn/txr/TXN/PATH` itself, and
  "Clients MUST NOT assume that the "special URI" is "!svn"". A commit is a POST to the me resource (it "returns the
  name of the transaction it created"), PUT/PROPPATCH/DELETE/COPY/MKCOL against transaction resources, then MERGE, or
  DELETE of the transaction to abort; write requests carry `X-SVN-Version-Name` "(whose value carries the base revision)
  to facilitate server-side out-of-dateness checks". The read side's mapping (get-file to GET, get-dir to PROPFIND, log
  and the other reports to REPORT against the pegrev URI or the me resource) is the notes' status table.
- **DeltaV (v1)** [B: `webdav-protocol`]: the resources older clients discover (`!svn/vcc`, `!svn/bln`, `!svn/wbl`,
  `!svn/bc/REV/`, `!svn/act/UUID/`, `!svn/ver/REV/path`, `!svn/wrk/UUID/path`), the commit as MKACTIVITY, CHECKOUT per
  object, PUT/PROPPATCH/MKCOL/COPY/DELETE, MERGE, DELETE of the activity; the custom REPORT bodies (update-report with
  `send-all`, dated-rev-report, get-locks-report, get-locations, log-report, mergeinfo-report, and others) with
  examples; a GET with `X-SVN-VR-Base` and `Accept-Encoding: svndiff1;q=0.9,svndiff;q=0.8` "may choose not to return
  svndiff content but return full-text".
- **The bodies, the code** (read 2026-11-19 for A-609; §4.17.4 piece 13).
  - **Recorded requests** [M: svn 1.15.0, Homebrew, the Mac Studio, 2026-11-19]. Stock svn was run against a recorded
    responder (responder sha256 prefix `5b21b680f1ce2c21`). It sends:
    - REPORT bodies with no XML declaration;
    - OPTIONS, PROPFIND and MERGE bodies with one;
    - update-report bodies indented by `svn_xml_make_open_tag`, their attributes in a hash table's order, which
      differed between two runs;
    - an empty `<D:options xmlns:D="DAV:" />` as the chunked probe;
    - a MERGE with `X-SVN-Options: no-merge-response`;
    - every report but `get-deleted-rev-report` for the commands run.
  - **The handlers** [C: `mod_dav_svn/reports/*.c`]:
    - Each report finds `svn:` with `dav_svn__find_ns` or refuses: "The request does not contain the 'svn:'
      namespace".
    - Each skips children in another namespace (`if (child->ns != ns) continue`) and ignores unknown names.
    - Revisions are `SVN_STR_TO_REV`, which is `atol`.
    - Paths pass `dav_svn__test_canonical`: empty, a canonical URL, absolute path or relative path; refused with error
      id 0.
    - `get-locks.c` refuses a depth other than empty, files, immediates or infinity.
    - `replay.c` refuses a second revision and a missing low-water mark.
    - `update.c` requires `src-path` and an entry's `rev`, and honours `send-all="true"` only with bulk updates on.
    - `version.c` pairs a lock-token list's `lock-path` and `lock-token` by local name in any namespace; ra_serf writes
      both unprefixed.
  - **The answers** [C: the same files; httpd `mod_dav.h` 63-77]:
    - `DEBUG_CR` is `"\n"` (`#if 1`).
    - The format strings are as each test's vector shows, among them `"<S:owner %s>"` and the bare `DEBUG_CR` after a
      MERGE's absent post-commit error.
    - Base64 lines are 76 characters, the last line ended by a line feed too (`base64.c` 47, 171-204).
    - `svn_xml_fuzzy_escape` writes `?\NNN`.
    - `apr_xml_quote_string` escapes `&`, `<`, `>`, and `"` only when asked.
- **svndiff over HTTP** [C: `mod_dav_svn/repos.c` 1784-1851]: the response's version follows the request's
  `Accept-Encoding` and the server's compression level (svndiff2 when the client lists it and the level is 1, else the
  client's preference, else 0). What a client sends in PUT bodies follows the server's advertised capabilities
  (`version.c` 187-200: `SVNDIFF1` and `SVNDIFF2` advertised from 1.10). So here too a server that advertises neither
  never receives zlib.

## 4. svndiff, versions 0, 1 and 2

- **The format** [B: `notes/svndiff`]: a delta is "a sequence of windows, each containing a list of instructions for
  reconstructing a contiguous section of the target using a contiguous section of the source"; "Source views must not
  slide backwards from one window to the next" (11-13). A document is `SVN` plus a version byte, then windows; a window
  is the source view's offset and length, the target view's length, the instruction section's length, the new data's
  length, then the sections; versions 1 and 2 prefix each section with its original length, and "If the original size
  matches the encoded size (minus the length of the original size integer) from the header, the data is not
  compressed"; version 1 is zlib, version 2 is LZ4. Integers are big-endian base-128 with a continuation bit ("Higher-
  order bits are encoded before lower-order bits. As an example, 130 would be encoded as two bytes, 10000001 followed by
  00000010", 49-50). An instruction's top two bits select source (00), target (01) or new data (10), 11 being invalid;
  its six low bits are the length, 0 meaning a length integer follows; source and target copies then carry an offset.
  A target copy "must begin at a location before the current position in the target view, but its length may extend
  past the current position. In this case, the target data copied is repeated". Worked encodings (75-84) and "a complete
  example of an svndiff between the source document "aaaabbbbcccc" and the target document "aaaaccccdddddddd"" (86-102):
  header `53 56 4E 00`; window header `00 0C 10 07 01`; instructions `04 00`, `04 08`, `81`, `47 08`; new data `64`.
  These are §4.17.4's golden vectors.
- **What the deployed decoder refuses** [C: `svndiff.c`]: a header other than the three (`SVN_ERR_SVNDIFF_INVALID_HEADER`);
  a target or source view past `SVN_DELTA_WINDOW_SIZE`, a new-data length past it plus `SVN__MAX_ENCODED_UINT_LEN`, an
  instruction section past `MAX_INSTRUCTION_SECTION_LEN` ("Svndiff contains a too-large window", 718-720); overflowing
  sums ("corrupt window header"); "backwards-sliding source views" (735-737), checked only for a window whose source
  view is not empty, against the previous window whatever its view; an instruction that cannot be decoded, has length
  zero, overflows the target view, overflows the source view, starts a target copy at or past the position, or
  overflows the new data; instructions that do not fill the target window exactly or leave new data unused (536, 539);
  and leftover bytes past the largest possible window header (786). `SVN_DELTA_WINDOW_SIZE` is 102,400
  (`delta.h` 45); `MAX_INSTRUCTION_SECTION_LEN` is that times `2 * SVN__MAX_ENCODED_UINT_LEN + 1` (`svndiff.c`); the
  varint decoder reads at most 10 bytes and silently drops bits past 64 (`encode.c`, `svn__decode_uint`: `temp << 7`
  unchecked), which slates refuses instead.
- **The compressed sections** [C: `compress_lz4.c`, `compress_zlib.c`]: the original length first; equal lengths mean
  stored; otherwise version 2 is an LZ4 block (`LZ4_decompress_safe`, the result's length checked against the stored
  one) and version 1 a zlib stream (`uncompress`); each decompression is refused past its limit (the window size for
  data, the instruction-section size for instructions). zlib's encoder skips sections under 512 bytes
  (`MIN_COMPRESS_SIZE`), a constant of the encoder alone that a decoder never needs.

## 5. The filesystem a server presents

- **Revisions and transactions** [B: `svn_fs.h` 1021-1104]: a commit "will either succeed completely ... or fail
  completely"; "Once a revision has been committed, it never changes again"; a transaction is named, persistent and
  carries properties that become the revision's unversioned properties, `svn:date` set at commit unless
  `SVN_FS_TXN_CLIENT_DATE`; "Transaction names are guaranteed to contain only letters (upper- and lower-case),
  digits, `-', and `.'".
- **Node-revisions** [B: `structure` 551-588]: `id`, `type`, `pred` (predecessor), `count` (node-revs since the node's
  base), `text` and `props` representations each with size, MD5 and (format 4+) SHA-1, `cpath` (created path),
  `copyfrom`, `copyroot`, `minfo-cnt` ("The number of nodes under (and including) this node which have
  svn:mergeinfo"), `minfo-here`. Changed paths per revision (606-637): the node-rev id, the action (`add`, `delete`,
  `replace`, `modify`, with the kind from format 4), `text-mod`, `prop-mod`, `mergeinfo-mod`, the path, and the copy
  source. Node and copy ids (418-521): a creation takes its parent's copy id, a copy gets a fresh one, a modification
  through a copied parent is a "lazy copy" with a fresh copy id; relations (`svn_fs.h` 932-963) are `unrelated`,
  `unchanged` and `common_ancestor`.
- **Bubble-up.** FSFS writes a new node-revision for every directory above a changed node (the mutable parent chain of
  `svn_fs.h`'s transaction description, 1066-1071: changing a descendant "would create a new mutable copy of the root
  directory"), so a directory's created revision is the last revision that changed anything beneath it.

## 6. The commit rule (what §4.17.4's verdict must equal)

- **Out of date, during the drive** [C: `commit.c`]: `check_out_of_date` (176-196) refuses `base_rev < created_rev`
  (185) with `SVN_ERR_FS_TXN_OUT_OF_DATE` ("File '%s' is out of date", "Directory '%s' is out of date"), and a base
  revision past the transaction's base with `SVN_ERR_FS_NO_SUCH_REVISION`; open-file checks it (627-634), delete-entry
  checks the path exists ("If PATH doesn't exist in the txn, the working copy is out of date", 498) and then its base,
  change-dir-prop checks the directory ("Subversion rule: propchanges can only happen on a directory which is
  up-to-date", 717-724), and an add with a copy source refuses a path that already exists unless its parent was copied
  (338-344). All of these read the transaction's root, so they compare with one revision, the transaction's base.
- **The merge at commit** [C: `tree.c` `merge()`, 1790 on; its plan in the comment 1846-1910]: the base cases ("Either
  no change made in source, or same change as made in target"); a conflict if any of the three is a file ("If any of the
  three entries is of type file, declare a conflict", 1884); a property change only "on an up-to-date directory"
  (1918): the source's directory properties changed since the ancestor is a conflict, and the target's changed is a
  conflict unless the source's entries changed only in place (`compare_dir_structure`); a double delete or a delete
  against a modification is a conflict; an entry replaced on either side is a conflict, "even an identical replacement"
  (1890); and an entry added on both sides is a conflict: "Even if SOURCE and TARGET are adding exactly the same thing,
  two additions are not auto-mergeable" (1899). The error is `SVN_ERR_FS_CONFLICT`.
- **The error numbers**, derived from `svn_error_codes.h` (each category starts at 120,000 plus its index times 5,000;
  each definition names its offset), as `svn` prints them: `E160024` conflict, `E160028` transaction out of date,
  `E160042` out of date (locks), `E160006` no such revision, `E160035` already locked, `E160037` bad lock token,
  `E160039` lock owner mismatch, `E160040` no such lock, `E160049` property base value mismatch, `E165001` hook failure,
  `E165006` disabled feature (no pre-revprop-change hook), `E165007` post-commit hook failed, `E170001` not authorized,
  `E185000` to `E185005` svndiff (invalid header, corrupt window, backward view, invalid ops, unexpected end, invalid
  compressed data), `E200014` checksum mismatch, `E200020` mergeinfo parse error, `E200046` LZ4 decompression failed,
  `E210001` unknown command, `E210004` malformed data, `E210009` request size, `E220001` unreadable, `E220002`
  partially readable, `E220004` unwritable, `E125005` bad property value. (Derivation script kept outside the tree;
  `grep -n 'SVN_ERRDEF' svn_error_codes.h` shows each offset.)
- **The merge, then the locks** [C: `tree.c` `svn_fs_fs__commit_txn` 2194 on, `merge_changes` 2284; `transaction.c`
  `commit_body` 3797; read 2026-11-19 for A-592]: FSFS merges the head into the transaction first, and only a merged
  transaction reaches `commit_body`, whose `verify_locks` checks the locks; so a work that conflicts is answered with
  the conflict whatever locks stand in its way. The drive checks locks too: `fs-wrap.c` 151-152 begins a commit's
  transaction with `SVN_FS_TXN_CHECK_LOCKS`, which `tree.c` honours at each editor call (1614, 2497, 2554, 2645).
- **Subversion's path order** [C: `sorts.c` `svn_sort_compare_items_as_paths` 69-79, which is `path.c`
  `svn_path_compare_paths` 418-453]: "Children of paths are greater than their parents, but less than greater siblings
  of their parents": a `/` sorts before every other byte, so `/a/b` comes before `/a-b`, which byte order puts first.
  `svn_fspath__skip_ancestor` (`dirent_uri.c` 2664, through `svn_relpath_skip_ancestor` 1530-1551) is what
  `verify_locks` skips a checked subtree's paths by.
- **Property lists compare by content** [C: `fs_fs.c` `svn_fs_fs__prop_rep_equal` 1608-1665]: the same representation
  is equal, committed representations with different MD5s differ, and otherwise both lists are read and compared, so
  an empty list and no list are equal.
- **A transaction root's created revision** [C: `dag.c` `svn_fs_fs__dag_get_revision`]: "In the special case that
  this is an unmodified transaction root, we need to actually get the revision of the noderev's predecessor (the
  revision root); see Issue #2608"; every other node the transaction made answers no revision, so the drive's
  out-of-date test passes it unless the client named a revision past the transaction's base.

## 7. Properties, locks, mergeinfo, hooks, authorization

- **Properties** [C: `fs-wrap.c` `svn_repos__validate_prop`]: only regular properties may be stored; translated `svn:`
  properties (`svn:log` among them) must be UTF-8 with no CR; `svn:date` must parse; `svn:mergeinfo` must parse
  (`verify_mergeinfo`). Entry properties the server sends with every node [B: `svn_props.h` 576-592]:
  `svn:entry:committed-rev`, `committed-date`, `last-author`, `uuid`, `lock-token`.
- **Locks** [B: `locking-functional-spec.txt`]: "A lock grants one user the exclusive right to change a certain file
  from a specific working copy"; the owner alone may change, delete or move it, which includes "renaming or deleting
  any of its parent directories"; a lock is "an immutable data object" with "a unique identifier, the path being locked,
  the username that owns the lock, and a creation timestamp", a comment optional; release on commit by default; breaking
  and stealing; `svn:needs-lock` tells clients a file must be locked first.
- **Locks, the code** (read 2026-11-19 for A-602, §4.17.4 item 7).
  - **Locking.** [C: `fs-wrap.c` 580-614; `fs-loader.c` `svn_fs_lock_many` 1816-1904; `lock.c` `check_lock` 713-829,
    `lock_body` 838-945]
    - A request needs a username: "Cannot lock path, no authenticated username available."
    - The comment must be XML-safe: "Lock comment contains illegal characters", `E130004`.
    - The expiration must not be negative: "Negative expiration date passed to svn_fs_lock", `E200004`.
    - A supplied token must start `opaquelocktoken:` ("has bad scheme") and hold no byte that is past ASCII or a
      control character ("is not ASCII or is a control character"), `E160037`.
    - Each path is then checked in path order:
      - a directory is `SVN_FS__ERR_NOT_FILE`;
      - a path HEAD lacks is refused, "we deliberately choose not to allow such madness": `E160042` when a current
        revision was given, `E160013` otherwise;
      - a current revision past the youngest is `E160006`;
      - one older than the node's created revision is "Lock failed: newer version of '%s' exists", `E160042`;
      - an existing lock without stealing is `SVN_FS__ERR_PATH_ALREADY_LOCKED`, whoever asks.
    - The second loop gives each path that passed its token (supplied, or `svn_fs_fs__generate_lock_token`:
      "opaquelocktoken:" and `svn_uuid_generate`), the username as owner, and `apr_time_now()` as creation.
    - "### TODO: actually do this check": a supplied token is never checked against other paths' tokens.
  - **Expiry.** [C: `lock.c` `lock_expired` 455-458, `get_lock` 467-505, `get_lock_helper` 508-535, `walk_locks`
    541-600]
    - `lock_expired` is "lock->expiration_date && (apr_time_now() > lock->expiration_date)", so 0 is none.
    - `get_lock` removes an expired lock "Only ... if we have the write lock" and answers `E160041`.
    - `get_lock_helper` treats no lock and an expired lock alike as none: "this function doesn't tell the caller *why*
      the lock is unavailable".
    - `walk_locks` skips an expired lock, and removes it under the write lock.
  - **Unlocking.** [C: `lock.c` `check_unlock` 949-968, `svn_fs_fs__unlock` 1200-1275; `fs-loader.c` `svn_fs_unlock`]
    - "We need to have a username attached to the fs" holds even to break, `E160034`.
    - No lock is `E160040`.
    - Unless breaking, a token other than the lock's is `E160040`, and another owner `E160039`.
    - A token not sent is compared as "" ("if (!token) token = "";").
  - **At the commit.** [C: `transaction.c` `verify_locks` 3527-3593; `lock.c` `verify_lock` 606-631,
    `svn_fs_fs__allow_locked_operation` 645-672]
    - Changed paths are sorted "depth-first-ily". A modification checks the path's own lock. Every other change checks
      every lock at or beneath it: "For paths being added or removed, we need to hold the locks for that path and any
      children of that path".
    - Each lock is verified in order: no username (`E160034`), then another owner (`E160039`), then a token missing
      from the access context's tokens (`E160037`). Those tokens are keyed by token, not by path.
  - **Release.** [C: `serve.c` `commit` 1471-1580, `unlock_paths` 1429-1468, `lock` 2952-2990]
    - "if (! keep_locks && lock_tokens && lock_tokens->nelts) unlock_paths" unlocks every presented path with its
      token, never breaking, after the commit.
    - Its errors are ignored: "The lock may have become defunct after the commit, so ignore such errors".
    - ra_svn's `lock` asks "No expiration time" (0).
  - **Control characters.** [C: `xml.c` `svn_xml_is_xml_safe` 105-127; `ctype.c`]
    - Valid UTF-8, with no control byte but tab, line feed, carriage return and delete.
    - `svn_ctype_iscntrl` is 0 to 31 and 127.
  - **Error numbers.** [B: `svn_error_codes.h`] The filesystem category plus its offset: `E160034` no user, `E160035`
    already locked, `E160037` bad token, `E160039` owner mismatch, `E160040` no such lock, `E160041` expired, `E160042`
    out of date. Also `E130004`, XML category plus 4, and `E200004`, miscellaneous category plus 4.
- **Mergeinfo** [B: `svn_mergeinfo.h` 45-90]: the value grammar (`revisionrange`, `revisioneelement` with an optional
  `*` for non-inheritable, `rangelist`, `revisionline`, `top`); a path without its own mergeinfo "inherits mergeinfo
  from its nearest parent that has SVN_PROP_MERGEINFO set", except non-inheritable ranges; elision. Merging itself is
  the client's (it asks for mergeinfo, logs, location segments and diffs, merges text in the working copy, and commits).
- **Hooks** [C: `hooks.c` 458-894]: start-commit (repository path, user, the client's capabilities joined by `:`, the
  transaction), pre-commit (repository path, transaction; stdin `LOCK-TOKENS:` then `path|token` lines), post-commit
  (repository path, revision, transaction), pre-revprop-change (repository path, revision, author, name, action `A`,
  `M` or `D`; the new value on stdin) and post-revprop-change (the old value on stdin), pre/post-lock and
  pre/post-unlock. With no pre-revprop-change hook "default to MASSIVE PARANOIA" (670): every revision property change
  is refused, `SVN_ERR_REPOS_DISABLED_FEATURE`. Hooks get their stdin from a temporary file (`create_temp_file`), which
  slates replaces with a pipe (R1). Recorded from stock `svnserve` 1.15.0 [M: `crates/svn/fixtures/hooks`, A-623]: a
  `LOCK-TOKENS:` line names the client's own path, relative to its commit's base (`serve.c` `add_lock_tokens` hands the
  filesystem the path as sent), escaped by `svn_path_uri_autoescape`'s ten bytes and nothing past ASCII; pre-commit
  reads the null device when no token was presented; a hook's environment is its own hooks-env section, else the
  default one, and a section with no variables is none (`parse_hooks_env_option`); a hook `exec` refuses exits 255 with
  nothing written (`svn_io_start_cmd3` leaves APR's error check off); post-lock's and post-unlock's failures come
  wrapped in `E165008` and `E165009`, post-commit's as `commit-info`'s `post-commit-err`; a commit's release of its
  locks runs pre-unlock and post-unlock, whose failures `unlock_paths` logs and drops.
- **CRAM-MD5** [B: RFC 2195 102-107, 205-212]: keyed MD5's precomputed intermediate results "make it possible to avoid
  explicit cleartext storage of the shared secret on the server system by instead storing the intermediate results
  which are known as "contexts"", and "servers that store the secrets or contexts must both be protected": a context is
  as strong as the secret.
- **Authorization** [B: 1.10 release notes, "Improved path-based authorization"; book, path-based authz]: sections
  `[repos:/path]` and `[/path]` matched literally, and from 1.10 `[:glob:repos:/path]` and `[:glob:/path]` with `*` one
  segment and `**` any number; rules for users, `@group`, `&alias`, `$anonymous`, `$authenticated`, `*`, `~` to invert;
  `r`, `rw` or nothing; write-only refused; colliding sections refused; global and per-repository rules combined as
  1.14.1 fixed (issue 4762). Log's revision access [B: `svn_repos.h` 4365-4377; C: `log.c` 114-221]: `none` when no
  changed path is readable, `partial` (`svn:date` and `svn:author` only, and the readable changed paths) when some are,
  `full` otherwise.
- **Authorization, read whole for A-606 (2026-11-19, g229)**, at the same revision; sha256 prefixes: `authz_parse.c`
  `65dfe8189bc612e4`, `authz.c` `77d8cab538643c77`, `authz_info.c` `5f9ff5bbaaf9e187`, `authz.h` `8042f6925f6712a6`
  (C); `libsvn_subr/config_file.c` `0de5f15f23270f73`, `ctype.c` `8c00533a27dc55f5`, `string.c` `62b5ad96280ba127`
  (C: the file's lines, whitespace, member lists); `tests/libsvn_repos/authz-test.c` `6eaefc582403bf7e`, `authz.rules`
  `faed4da16fc20b00`, `authz.groups` `7436b89c876dd1e8`, `repos-test.c` `40e945d3228d791c`, `libsvn_repos/repos.c`
  `d5b563ff1363880f` (the `conf/authz` `svnadmin create` writes), `tools/server-side/svnauthz.c` `f62113915e696068`;
  APR 1.7.5 (tag object `ee3f7c40`) `strings/apr_fnmatch.c` `12788315aebe9455` and `test/testfnmatch.c`
  `28b79467bb826dba` (C); the 1.10 and 1.14 release notes (`7c7a34f5bee69657`, `cb16ff08aa5519d6`) and the book's
  path-based authorization page (`9b976e3a8e2792e6`), B. Trunk at `fd4e82b` (2026-11-07) has the same `lookup`, and
  its parser differs in comments alone, so Subversion 1.15 behaves as 1.14.x here.
  - The precedence [C: `authz.c` 904-1011, 1277-1418]: per (user, repository) the rules that apply are folded into a
    tree of path segments; the deepest level of the path an applying rule matches decides, and among the rules
    matching that level the one with the highest sequence number (defined last) wins (`combine_access`); a `**` node is
    matched again at every level and at its own (`add_next_node`); a rule ending in `**` eclipses the earlier rules
    beneath its prefix (`trim_tree`); a global and a repository rule for one path give way to the repository's when it
    matches the user (`create_user_authz`, 932-958: issue 4762). A recursive question is the least access over the
    subtrees of the level's nodes ("Potential because we don't verify that the respective paths actually exist").
  - Where the code departs from its own documentation: (1) `lookup` strips `/` to the empty string and walks it as
    one empty segment (1284-1319), so `[:glob:/*]` (`add_next_node(... ->any)`, 1354, for any segment, the empty one
    too) decides the root although `authz.h` 210 says `*` "Matches any single non-empty path segment"; (2) a suffix
    match reverses the segment in place (1375) and nothing restores it, so the next node of the same level meets it
    reversed (`[:glob:/*.secret]` beside `[:glob:/**/public]` leaves `/public` unmatched and matches `/cilbup`);
    (3) `parse_rule_path` returns the root for any path whose first segment is empty (`authz_parse.c` 505-512), so
    `[//secret]` is the root's rule, though the next lines say "A path with two consecutive slashes is not
    canonical". Two more outcomes depend on hash order: group expansion
    (`expand_group_callback`, 1062-1127) detects a cycle only through the group it expands first and otherwise
    recurses until its stack runs out, and several refusals in one file are reported in hash order.

## 8. The reference servers' shape (what the comparison is against)

- svnserve [C: `svnserve.c` 115-165]: by default a forked process per connection (threads with `-T`, at most 256),
  serving one request at a time per connection; a 16 MB request cap, compression level 5, a 16 MB cache by default; FSFS
  underneath, whose commit takes the repository's write lock and writes a revision file. mod_dav_svn: Apache's process
  and thread model, `SVNCompressionLevel`, the same FSFS.
- Every one of these numbers is a constant set once per deployment (`--max-request-size`, `-c`, `-M`, Apache's
  keep-alive settings). §4.17.4 replaces each with an estimate learned from the deployment's own traffic (§4.17.1).

## 9. The interop harness

- [B: tests `README` 55-350; C: `main.py` 2165-2237]: each test script takes `--url` (`svn://localhost`,
  `http://localhost`), `--bin` and `--tools-bin`; the suite creates its repositories itself under
  `svn-test-work/repositories` with `svnadmin create` (`create_repos`, 1157), writes `conf/svnserve.conf`, `conf/passwd`
  and hook scripts into them (1050-1073, 370-403), copies them with `svnadmin dump | svnadmin load` (`copy_repos`), and
  reads them back with `svnlook`. Over svnserve it is `svnserve -d -r <tests dir>` then `make check
  BASE_URL=svn://localhost`; over HTTP, `davautocheck.sh`. The users are `jrandom` and `jconstant`, password
  `rayjandom`.

## 10. What follows for slates (each claim of §4.17.4 and the lines that carry it)

1. One green per repository, version N being revision N, a branch a directory (§4.17.1's row, and §5 above: a revision
   is one immutable tree; copies are the only branching).
2. The commit verdict is FSFS's rule exactly, which is stricter than §4.16's: no identity accept (`tree.c` 1890, 1899),
   directory property rules (1918 and `compare_dir_structure`), and a directory's last change counts its whole subtree
   (bubble-up, §5). §4.17.1 items 2 and 5 state the file case; §4.17.4 states the rest and raises it with g261.
3. A server need not speak zlib: it never announces or advertises svndiff1 and never receives it (§2, §3); svndiff 0
   and 2 suffice, and the workspace already holds an LZ4 block codec (`lz4_flex`).
4. Every constant the reference servers expose (§8) is a tunable slates learns (§4.17.1's rule).
5. The test suite drives `svnadmin` and `svnlook` against a repository path, so slates must answer those verbs over its
   own repository, and the server must read the repository directory's `conf/` and `hooks/` (§9).

## 11. Mergeinfo (A-605, §4.17.4 item 8 and piece 9)

Read 2026-11-19 by g262 at the same revision (`1.14.x` at `d434dc14`), fetched as in §1. The sha256 prefixes:

| File | sha256 (prefix) | Tier |
|---|---|---|
| `subversion/include/svn_mergeinfo.h` | `129d6739fee73856` (as §1) | B |
| `subversion/include/private/svn_mergeinfo_private.h` | `d5b71c114d393f66` | B |
| `subversion/include/svn_types.h` | `6167309dad233964` | B |
| `subversion/libsvn_subr/mergeinfo.c` | `538cc82a8d6bca2f` | C |
| `subversion/tests/libsvn_subr/mergeinfo-test.c` | `28381a00f69941fd` | C |
| `subversion/libsvn_subr/types.c` | `8c9fe593b18d24cf` | C |
| `subversion/libsvn_subr/string.c` | `62b5ad96280ba127` | C |
| `subversion/libsvn_subr/dirent_uri.c` | `fe05687a2dedb22c` | C |
| `subversion/libsvn_subr/path.c` | `b234d99acb31b426` | C |
| `subversion/libsvn_subr/sorts.c` | `4fe2371630f6f71a` | C |
| `subversion/libsvn_fs_fs/dag.c` | `9efbb054b8818693` | C |
| `subversion/libsvn_fs_util/fs-util.c` | `2c9eb29504df0be3` | C |
| `subversion/libsvn_client/mergeinfo.c` | `b92a177e7a6abae0` | C |

and `tree.c`, `fs-wrap.c`, `log.c`, `serve.c` and the protocol at §1's hashes.

- **The parser** [C: `mergeinfo.c` `parse_pathname`, `parse_rangelist`, `parse_revision_line`, `parse_top`,
  `svn_rangelist__canonicalize`; `types.c` `svn_revnum_parse`; `string.c` `svn__strtoul`]: a line's last colon divides;
  the path is `svn_fspath__canonicalize`d (`dirent_uri.c` `canonicalize`: empty and `.` segments dropped, `..` kept); a
  revision is digits alone (`svn__strtoul` reads no sign and no space), "longer than 10 digits" and a ten-digit value past
  `APR_INT32_MAX` refused; `0` refused; horizontal space skipped only after the colon; "Range list parsing ended before
  hitting newline" refuses a comma at the value's end while a comma before a newline passes; a key named twice is merged
  (`svn_rangelist_merge2`, the comment on issue #3547); the value is read with `strlen`, and `verify_mergeinfo`
  (`fs-wrap.c`) refuses a value whose `strlen` is not its length before parsing it. `svn_mergeinfo_parse` returns
  `SVN_ERR_MERGEINFO_PARSE_ERROR` (200020) on top of every refusal.
- **The algebra** [C: `mergeinfo.c`]: 1.14's union is a sweep over both lists' intervals in which a revision takes the
  larger of `MI_NONE`, `MI_NON_INHERITABLE`, `MI_INHERITABLE` (`rangelist_merge`); intersection and removal are one walk
  (`rangelist_intersect_or_remove`) that pushes through `combine_with_lastrange`. With `consider_inheritance` the walk is
  not exact per revision: a first-list range met by a second-list range of the other kind is passed when it sorts first,
  so `1-10` intersected with `3-4*,5-6` is empty and `1-10` with `3-4*,5-6` removed is `1-10` (traced by hand on the
  code; the module's bound tests hold what is exact). The stock client computes `svn mergeinfo --show-revs` with these
  functions (`libsvn_client/mergeinfo.c` `svn_client__mergeinfo_log`: `svn_mergeinfo_intersect2(..., TRUE)`,
  `svn_rangelist_remove(..., TRUE)`, `svn_rangelist_diff(..., TRUE)`), so a server's answer equal to it needs these
  functions as they are.
- **The queries** [C: `tree.c` 4133-4420; `dag.c` 270-300; `fs-util.c` `svn_fs__append_to_merged_froms`; `fs-wrap.c`
  `svn_repos_fs_get_mergeinfo2`]: the path must exist (`open_path` with no flags); the nearest node with `minfo-here` from
  the path up (nearest-ancestor starts at the parent and answers none at the root; explicit looks at the path alone); a
  value that fails to parse is none and ends the search (issue #3896); an ancestor's value is `svn_mergeinfo_inheritable2`
  then extended by the path beneath it; descendants are crawled entering a directory only when `minfo-cnt` > 1, or = 1
  without `minfo-here`; "claims to have mergeinfo but doesn't" is `SVN_ERR_FS_CORRUPT`; the repository layer filters the
  requested paths by read access and not the descendants, and always adjusts what is inherited.
- **Elision** [C: `libsvn_client/mergeinfo.c` `should_elide_mergeinfo`, `svn_client__elide_mergeinfo`]: no mergeinfo
  never elides; empty elides to none or empty; any other never to none or empty, else when `svn_mergeinfo__equals` with
  inheritance considered.

## 12. The dump format (A-593; §4.17.4 piece 6, its first part)

Read 2026-11-19 at the same revision (`1.14.x` at `d434dc14`), from the same mirror; the first 16 hex digits of each
file's sha256:

| File | sha256 (prefix) | Tier |
|---|---|---|
| `notes/dump-load-format.txt` | `abd310d84f04dd6d` | B |
| `subversion/include/svn_repos.h` | `8421f33e065fd927` | B |
| `subversion/include/svn_types.h` | `6167309dad233964` | B |
| `subversion/libsvn_repos/dump.c` | `10e31e13700f3a74` | C |
| `subversion/libsvn_repos/dump_editor.c` (svnrdump's writer) | `21e5a7bb82fecc6a` | C |
| `subversion/libsvn_repos/load.c` | `7a04eb3858a0cfda` | C |
| `subversion/libsvn_repos/load-fs-vtable.c` | `e802f38a22f4c8ad` | C |
| `subversion/libsvn_subr/hash.c` | `5973d515ba697c50` | C |
| `subversion/libsvn_subr/checksum.c` | `907299438410f21b` | C |
| `subversion/libsvn_subr/sorts.c` | `4fe2371630f6f71a` | C |
| `subversion/libsvn_subr/ctype.c` | `8c00533a27dc55f5` | C |
| `subversion/svnrdump/dump_editor.c` | `96c82adcc9d5b6c8` | C |

The golden vectors (`crates/svn/fixtures/dump`) were written by Homebrew's Subversion 1.15.0 (r1937767) on the Mac
Studio, so its tag `1.15.0` was compared too: its `dump.c` and `hash.c` are byte for byte the files above; `load.c`
differs in one comment's spelling (`8d488ab2e38e6068`); `load-fs-vtable.c` in the hash function of its revision map,
`svn_hash__make` (`8b70fca46f6534fb`); `notes/dump-load-format.txt` (`8ecf80f7a448bd0d`, its version 1.3) adds that
revisions are always dumped in increasing order and describes `svn:author`, `svn:date` and `svn:log`.

- **The grammar** [B: notes 29-198]: a stream is a version line, then records; a record is header lines, "a key,
  followed by a colon, followed by text data to end of line", an empty line, and optionally a body; "unrecognized
  headers are ignored". A UUID record follows the version line in format 2 and later; revision records carry the
  revision's properties; node records follow their revision; "Dump decoders should be prepared for the optional lines
  after Node-action to be in any order". A property section is `K`/`V` entries, each "a header line giving the length
  of the key or value content in bytes" then the content and a line feed, ended by `PROPS-END\n`, "its length is
  included in the Prop-content-length". Format 3 adds `Text-delta`, `Prop-delta`, `Text-delta-base-md5` and
  `-sha1`, and `D` entries "to describe property deletion" (notes 195-197, 466-523).
- **The writer's bytes** [C: `dump.c`]: the version record `SVN_REPOS_DUMPFILE_MAGIC_HEADER ": %d\n\n"` (516-524); the
  UUID record (527-537); a revision record's headers, `Revision-number` first and `Content-length` last (411-454; only
  three are ever pushed, so the order is fixed), an empty line, the properties, and `"\n"` (540-598), every revision
  with a property section, empty or not (`write_revision_record` passes `include_revprops` as
  `props_section_always`, 1968-1997). A node's headers are pushed in a fixed order (`dump_node`, 1119-1545: path,
  kind, action, copy source, the copy source's checksums, `Prop-delta`, `Text-delta`, the delta base's checksums, the
  text's), and `svn_repos__dump_node_record` (601-645) appends `Prop-content-length`, `Text-content-length` and
  `Content-length`. A record with content ends with `"\n\n"` (1542); one with none with `"\n"` after its empty line
  (1366-1376); and a replacement by a copy is a deletion record that ends with its empty line alone, then an addition
  with the copy source ("### Unusually, we end this 'delete' node record with only a single blank line", 1234-1274,
  `dump_node_delete` 1087-1104). A deletion has no `Node-kind` (`close_directory` passes `svn_node_unknown`).
  svnrdump's writer does the same (`dump_editor.c` 285-306, 741-743).
- **Property sections** [C: `hash.c` 256-335]: entries sorted by `svn_sort_compare_items_lexically` (266, 302), which is
  `memcmp` then length (`sorts.c` 83-96): Rust's byte-slice order. `K %d\n%s\nV %d\n`, the value, `\n` (288-296); in a
  delta, entries equal to the base's are left out and every name the new list lacks is a `D %d\n%s\n` after all `K`s
  (298-316); then `PROPS-END\n` (319). A name is printed with `%s`, so a name holding a NUL could not be written whole.
- **Deltas** [C: `dump.c` 345; `dump_editor.c` 889-891]: both stock writers encode svndiff version 0
  (`svn_txdelta_to_svndiff3(..., 0, ...)`), against the copy source for a copy, the previous revision's text for a
  change, and the empty text for an addition, which then has no base checksums (1456-1510). The windows are
  Subversion's own deltifier's, so a format-3 dump is byte for byte `svnadmin`'s only given its windows.
- **The reader** [C: `load.c`, `load-fs-vtable.c`]:
  - the first line within 80 bytes, `SVN-fs-dump-format-version` and a colon, the number by `svn_cstring_atoi`, refused
    past 3 (`parse_format_version`, 361-427); a version record later in the stream is read as a record (676-683), which
    is how two dumps written one after the other load as one;
  - between records, an empty line or one whose first byte `svn_ctype_isspace` calls whitespace is passed over (635),
    tab, line feed, vertical tab, form feed, carriage return and space (`ctype.c` 58-62, 81);
  - a header line is cut at its first colon and its value taken two bytes on, the second byte unchecked (112); no
    colon, or a colon at the line's end, is "a malformed header" (103, 116); the stream's end ends a header block as
    an empty line would (92); a header given twice keeps the last (`svn_hash_sets`);
  - the record's kind is the first of `Revision-number`, `Node-path`, `UUID`, the version header it holds, and none is
    "Unrecognized record type in stream" (642-689);
  - lengths are read by `svn__atoui64`, which is `apr_atoi64` (`svn_types.h` 442), revisions by `SVN_STR_TO_REV`, which
    is `atol` (398): both stop at the first byte that is not a digit;
  - a property section is read until its stated length is used or `PROPS-END` comes, whichever is first (195-289): a
    section whose entries fill its length exactly needs no `PROPS-END`, and a `PROPS-END` before the length is used
    leaves the rest to be read as text; `K`, `V` and `D` lengths by `svn_cstring_strtoui64`, refused as
    `SVN_ERR_INCORRECT_PARAMS` (213-266); a `D` in a revision's section is malformed (271);
  - the content block's bytes past its sections are passed over, a `Content-length` below them refused ("Sum of subblock
    sizes larger than total block content length", 785-811);
  - an unknown `Node-kind` leaves the kind unknown (`load-fs-vtable.c` 385, 400-406), and an addition of unknown kind
    without history adds nothing (`maybe_add_with_history`); an unknown or missing `Node-action` is "Unrecognized
    node-action on node" (`load-fs-vtable.c` 688-694); a node in revision 0 is refused (676-679 there); checksums are
    read by `svn_checksum_parse_hex`, two hex digits of either case a byte, what follows them ignored, an all-zero
    digest read as none (`checksum.c` 371-441), the MD5s alone (`load-fs-vtable.c` 437-452; the SHA-1 headers "are not
    currently used by the loader", notes 519-522).
- **What follows for slates** (A-593): the codec reads what `load.c` reads and writes what `dump.c` writes, byte for
  byte, and is stricter where `load.c` would read on and load a different history: digits only in every number; an
  unknown kind, action or delta flag refused; a header given twice refused; a NUL in a header or a property name
  refused; a property section that does not end exactly at its `PROPS-END` refused; a header block the stream's end
  cuts refused (a cut dump must never read as a shorter valid one but at a record's end); a version other than 2 or 3
  refused (format 1 is the archaic form notes 525-541 keep "for historical completeness only"); version-1 svndiff
  (zlib) refused, which no stock writer puts in a dump. A deletion followed by an addition of its path is read as the
  one replacement FSFS records, so the codec's records are the changed-path rows of §4.17.4 item 2.

## 13. The read commands (A-613, §4.17.4 "4. Reads", piece 7's second part)

Read 2026-11-19 by g262 at §1's revision: `svnserve/serve.c` (sha256 `acb1cac75ea86ba7`, as §1), `libsvn_subr/time.c`
(`c58645b206f847e8`), `libsvn_repos/rev_hunt.c` (`5acc827a362d11f4`), the protocol (§1's), and stock 1.15.0's own bytes
(`crates/svn/fixtures/reads`, recorded through a relay by its `make-fixtures.py`).
- **The order of a command's writes** [C: `serve.c` `trivial_auth_request`, `must_have_access`, each handler]: a
  command's parameters are parsed first (`svn_ra_svn__parse_tuple`; a refusal there is not a command error, so the
  connection ends with nothing written: recorded, `hostile-malformed`), then the trivial authentication request,
  `( success ( ( ) 0: ) )`, then the command's work, so a refusal of its revision or path follows the request
  (recorded). An unknown command is refused by the dispatcher with no request: "Unknown editor command" 210001
  (recorded).
- **Refusals' bytes** (recorded): `( failure ( ( code message 0: 0 ) ) )`, a release build's empty file and zero line;
  "Bogus date" (125003) for a date `svn_time_from_cstring` refuses; "No such revision N" and "Invalid revision number
  'N'" (160006); "File not found: revision N, path 'P'" (160013, `get-mergeinfo`); "'P' path not found" (160013,
  `get-iprops`); `get-iprops` with no revision opens revision -1 and is refused.
- **Property lists' order**: `svnserve` writes a hash table's order; three recordings of one repository gave revision
  3's four properties three orders, every other byte the same, so the order is APR's per-table seed (`apr_hash_make`
  since APR 1.5), not `svn_hash__make`'s fixed function (`hash.c` `hashfunc_compatible`).
- **Dates** [C: `time.c` `svn_time_from_cstring`, `svn_time_to_cstring`; `rev_hunt.c` `get_time`,
  `svn_repos_dated_revision`]: fields read by `svn__strtoul` (digits alone), each cast to 32 bits, then APR's
  `apr_time_exp_gmt_get`, which refuses a month outside 0-11 or a time before 1970 and checks no other field; the
  pre-1.0 format is tried after (`sscanf`), which slates refuses as a bogus date. The dated search is a binary search
  whose edges are its own: a date before every revision's is revision 0, after them the youngest.
- **`get-dir` and `get-locations`** [C: `serve.c` `get_dir`, `parse_dirent_fields`, `get_locations`;
  `rev_hunt.c` `svn_repos_trace_node_locations`, `svn_repos__prev_location`, `check_ancestry_of_peg_path`; recorded]:
  `get-dir`'s properties are the node's with `svn:entry:committed-rev`, `committed-date` (the stored string),
  `last-author` and `uuid`; its dirent fields default to `none`, 0, `false`, 0, time 0's date and no author when not
  asked; a directory's size is 0 here; a file is refused "Can't get entries of non-directory" (160016).
  `get-locations` writes its entries, then `done`, then the answer or the refusal; its trace follows the closest copy
  (the youngest copy root on the path's chain, a child's over its parent's of one revision, refused when the node at
  the copy's revision is another line or was made fresh there) back to its source, keeps a revision after the peg
  while the line reaches back to the same path (refused when the path is absent in it), and ends where the path holds
  another line or none.

Read 2026-11-19 by g297 for A-613 part 2, from `raw.githubusercontent.com/apache/subversion/1.15.0/<path>` (the release
the fixtures were recorded with; the first 16 hex digits of each file's sha256): `svnserve/serve.c` `8154654e07aa4eb4`
(as §1), `libsvn_repos/rev_hunt.c` `5acc827a362d11f4` (as above), `libsvn_repos/log.c` `673c9f4b42515315`,
`libsvn_repos/list.c` `6ef0f62c00210b13`, `libsvn_ra_svn/marshal.c` `361702975baa8c70` (as §1), `libsvn_ra_svn/protocol`
`b117e0ccc2008627` (as §1), `libsvn_fs_fs/tree.c` `f507210992ec9736`, `libsvn_fs_fs/dag.c` `9efbb054b8818693` (as §1),
`libsvn_subr/mergeinfo.c` `afee0be02f1374d6`, `libsvn_repos/fs-wrap.c` `126ab4670c8c23e0` (as §1),
`libsvn_delta/svndiff.c` `311b0286b38d7d6d`, `libsvn_delta/text_delta.c` `88686a46845f5b8e`, `libsvn_delta/xdelta.c`
`2e7b6226acfa9078`, `include/svn_repos.h` `941491e450489ee4`, `include/svn_error_codes.h` `f5b2c11d407e752f`; the
fixtures recorded again on the Mac Studio with revisions 8 to 11 and every remaining command's sessions.
- **`get-file`** [C: `serve.c` `get_file`, `get_props`; `dag.c` `svn_fs_fs__dag_file_checksum`; recorded]: parameters
  `c(?r)bb?B` (a fifth item that is no boolean ends the connection); the authentication request, the revision, then
  the checksum: absent "File not found", a directory "Attempted to get checksum of a *non*-file node" (160017); the
  answer `( ( md5 ) rev ( props ) [ ( iprops ) ] )`, the property list present even when not wanted, the inherited
  list only when asked (empty when no ancestor has properties); the text as strings of `get_file`'s 4,096-byte
  buffer, then the empty string and `( success ( ) )` (a read error there: the empty string and the refusal).
- **What the remaining commands do** (read for the slices that follow): `list` is `svn_repos_list` (entries sorted by
  name, the depth word read by `svn_depth_from_word` and refused below `empty`, names matched by
  `svn_utf__fuzzy_glob_match`, absolute paths in the answer, `done` then the answer); `get-location-segments` and
  `get-deleted-rev` are `rev_hunt.c`'s (segments cropped and gaps reported; the deleted revision found by a binary search
  over node relation and closest copy, its absence answered `SVN_ERR_ENTRY_MISSING_REVISION`); `log` is
  `svn_repos_get_logs5` (FSFS's node history, `tree.c` `history_prev`, walked per path; changed paths in the
  revision's stored order, which is byte order; merged revisions from `fs_mergeinfo_changed` and
  `combine_mergeinfo_path_lists`); `get-file-revs` is `svn_repos_get_file_revs2` (the delta's bytes are FSFS's own
  windows re-encoded as svndiff 2 for a client that accepts it, so slates' deltas made from extents are compared by
  the text they make).

## 14. The commit's request and its editor commands (A-617; §4.17.4 "5. The wire", piece 7)

Read 2026-11-19 by g268 at §1's revision: `libsvn_ra_svn/editorp.c` (§1's row), `marshal.c`, `svnserve/serve.c`,
`libsvn_ra_svn/client.c`, `libsvn_repos/commit.c` and the protocol (§1's hashes). Recorded the same day on the Mac
Studio: stock `svn` 1.15.0 (r1937767) against stock `svnserve` 1.15.0 over a tunnel whose relay logged every byte each
way (`crates/svn/fixtures/commit`, its `make-fixtures.py`; each file's SHA-256 in `SHA256SUMS`): 25 sessions over 26
connections, 17 of them commits.

- **The request** [B: `protocol` §3.1.1; C: `serve.c` `commit` 1470-1583, `add_lock_tokens` 1360-1409]: `( logmsg:string
  ? ( ( lock-path:string lock-token:string ) ... ) keep-locks:bool ? rev-props:proplist )`, read as `"clb?l"`, or as
  `"c"` when it holds one item (1495-1510: a client before 1.2 sends no tokens, keeps its locks and sends no revision
  properties). The tokens are read after the authentication request, each a list of two strings, and a malformed one is
  a command failure, "Lock tokens aren't a list of lists" (1521-1524). The log message is used only when no revision
  properties came (1526-1534), and the author is always the session's principal (1536-1541). Recorded: stock 1.15.0
  sends `svn:log`, `svn:txn-client-compat-version` and `svn:txn-user-agent`; a token's path relative to the session's
  URL (`a.txt`); keep-locks false in `svn commit`'s sessions and true in `svn copy`'s between URLs (`commit-with-lock`,
  `copy-url`).
- **The drive** [C: `editorp.c` `svn_ra_svn_drive_editor2` 1073-1212, the handlers 532-935]: each command `( name:word
  params:list )`, read `"wl"` (1110), its parameters with its handler's prototype, from `open-root`'s `"(?r)s"` (555) to
  `close-file`'s `"s(?c)"` (856). A name no handler has is "Unknown editor command" (`SVN_ERR_RA_SVN_UNKNOWN_CMD`,
  210001, 1127-1132), a command error; an error of the item reader (malformed data, 210004) leaves the loop and ends the
  connection. On a command error the server aborts the edit, writes the failure (1149-1174), then reads and drops
  commands until `abort-edit` or `success`, a disconnection being no error (1178-1208).
- **Reading a prototype** [C: `marshal.c` `vparse_tuple` 1608-1718]: items past the prototype are ignored; `?` lets the
  rest be absent; a nested list's own `?` lets it be empty; another kind than the prototype names is "Malformed network
  data". A `c` item is used as a C string, so `svnserve` reads `a\0b` as `a`; an `r` item is cast to `svn_revnum_t`
  (1643-1644), so a number past `LONG_MAX` turns negative and reads as no revision. slates refuses both
  (`commit::edit`'s module documentation); no stock client sends either. A word is at most 24 bytes (`MAX_WORD_LENGTH`
  25, 66; "Word is too long").
- **What the stock client sends** (the recordings): no `target-rev`; `open-root` first, then the tree in its path order,
  every directory closed, the root's too, before any file's text; then a file at a time, `apply-textdelta`, its
  `textdelta-chunk`s, `textdelta-end` and `close-file` with the text's MD5; then `close-edit`. So a file's token
  outlives its directory's `close-dir`. Tokens are `d<n>` and `c<n>` from one counter. The deltas are svndiff 2 to
  `svnserve`, which announces `accepts-svndiff2` (§2), each file's chunks one svndiff stream.
- **The answers** (the recordings; C: `serve.c` `commit` 1512-1583, `editorp.c` 888-906): the authentication request
  (`must_have_access`, 1517-1519), trivial in a session already authenticated, `( success ( ( ) 0: ) )`, and the
  request's `( success ( ) )` (1555); nothing during an accepted drive; at `close-edit`, the editor's `close_edit` (the
  verdict) run as a command (893) and `( success ( ) )` (905), a second trivial authentication request (1562), then `(
  new-rev:number ( date:string ) ( author:string ) ( post-commit-err:string ) )` (1576), the last list empty here. A
  refused commit's client stream still ends at `close-edit`, with no `abort-edit`: the client sends the whole drive
  before it reads, and the server's one `( failure ( ( code message file line ) ) )` is its last item: `E160028` "File
  '/trunk/project/a.txt' is out of date" (an `open-file` on a stale base) and "Directory '/trunk/project' is out of
  date" (a stale root's property), `E160039` "User 'alice' does not own lock on path '/trunk/project/a.txt' (currently
  locked by 'bob')", `E160024` "Conflict at '/trunk/project/sub/deeper/d.txt'" (another commit landed while the client's
  stream was held at `close-edit`).
- **A sibling noted, not changed.** §6's `check_out_of_date` (176-196) is at 179-198 of the `commit.c` §1 names
  (`1727532bffd25017`), where A-592's code cites it.
