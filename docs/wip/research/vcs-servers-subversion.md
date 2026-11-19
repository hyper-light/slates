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
| `subversion/libsvn_delta/svndiff.c` | `0ae17243dc9ab11b` | C |
| `subversion/libsvn_delta/delta.h` | `31ea1b30e27321f8` | C |
| `subversion/libsvn_subr/encode.c` | `3e5752e741f7aacd` | C |
| `subversion/libsvn_subr/compress_lz4.c` | `c50b1552af322b84` | C |
| `subversion/libsvn_subr/compress_zlib.c` | `35baff627fb62cd5` | C |
| `subversion/libsvn_ra_svn/marshal.c` | `908ccdd914cae15b` | C |
| `subversion/svnserve/svnserve.c` | `43155c2bd557492f` | C |
| `subversion/svnserve/serve.c` | `acb1cac75ea86ba7` | C |
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
  slates replaces with a pipe (R1).
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
