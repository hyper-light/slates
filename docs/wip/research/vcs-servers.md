# Version-control servers on greens and works: reading notes (primary sources only)

Status: started 2026-11-18 by g261 for A-497 (UI goal 91), the evidence behind §4.17 of the design. §1 to §4 back the
shared mapping (§4.17.1): what each tool promises a client when it pushes, and why each promise is a verdict of §4.16.
The git server's own notes (§4.17.2) follow in a later section of this file; the Mercurial (§4.17.3) and Subversion
(§4.17.4) servers' owners add theirs. Evidence tiers as in the design (B: a standard or the vendor's own documentation;
C: deployed code). Every quotation below was read directly on 2026-11-18 from the revision named, at the lines given.

## 1. Sources and revisions

| Source | Revision read | Where it came from |
|---|---|---|
| git `Documentation/` (`gitprotocol-pack.adoc`, `gitprotocol-capabilities.adoc`, `git-receive-pack.adoc`, `githooks.adoc`) | git `master` at `e1ec09b23c8e0deb21a0b95ac5996422277395d4` | `raw.githubusercontent.com/git/git/master/Documentation/<file>`; sha256 of the files read: `gitprotocol-pack.adoc` 7b65e9c1…b9f5f, `gitprotocol-capabilities.adoc` daa6f447…e1ff, `git-receive-pack.adoc` 092f3b08…bd27, `githooks.adoc` 5b3808fb…6b |
| Mercurial `mercurial/helptext/internals/` (`wireprotocol.txt`, `bundle2.txt`) and `mercurial/helptext/config.txt` | Mercurial 7.2.4, the source release on PyPI (`mercurial-7.2.4.tar.gz`, sha256 `85839e0f39e6cb893a88932aa36ef661759f3c5c5de4551ad26bd9df53cb71a2`) | `files.pythonhosted.org`; the project's own repository web views (`repo.mercurial-scm.org`, `foss.heptapod.net`) answered a bot check, so the release tarball is the copy read. sha256: `wireprotocol.txt` 3be121c3…6084, `bundle2.txt` a4daeed6…e169, `config.txt` a034617d… |
| Subversion `subversion/libsvn_ra_svn/protocol`, `subversion/include/svn_fs.h` and `subversion/include/svn_repos.h` | Subversion `trunk` at `fd4e82b00cbe89319728ad9e21121c40e59f92be` (the GitHub mirror of the Apache repository) | `raw.githubusercontent.com/apache/subversion/trunk/<path>`; sha256: `protocol` b117e0cc…1ec0, `svn_fs.h` 5c4a3f31…757eee, `svn_repos.h` 941491e4… |

## 2. What each tool promises a pushing client

### 2.1 git: each ref is compared and swapped on the value the client saw, all-or-none on request, objects quarantined

- The request names, per ref, the value the client saw and the value it wants [B: `gitprotocol-pack.adoc` 526-530]: "For
  each reference on the server that it wants to update, it sends a line listing the obj-id currently on the server, the
  obj-id the client would like to update it to and the name of the reference." The grammar (543-546): `create = zero-id
  SP new-id SP name`, `delete = old-id SP zero-id SP name`, `update = old-id SP new-id SP name`.
- The server's check is a compare-and-swap on that value [B: `gitprotocol-pack.adoc` 603-607]: "The server will receive
  the packfile, unpack it, then validate each reference that is being updated that it hasn't changed while the request
  was being processed (the obj-id is still the same as the old-id), and it will run any update hooks to make sure that
  the update is acceptable. If all of that is fine, the server will then update the references."
- The answer is per ref [B: `gitprotocol-pack.adoc` 641-647]: "Each line is either 'ok [refname]' if the update was
  successful, or 'ng [refname] [error]' if the update was not." The reasons it names (699-703): "The reference can have
  changed since the reference discovery phase was originally sent, meaning someone pushed in the meantime. The reference
  being pushed could be a non-fast-forward reference and the update hooks or configuration could be set to not allow
  that".
- A server may answer a different name, old value or new value for a ref [B: `gitprotocol-pack.adoc` 664-668]: "The
  'report-status-v2' capability extends the protocol by adding new option lines in order to support reporting of
  reference rewritten by the 'proc-receive' hook. ... each reference may have different name, different new-oid, and
  different old-oid." (This is how a server that makes its own commits for a push, as a review server does, tells the
  client what it made.)
- All-or-none is a capability the client asks for [B: `gitprotocol-capabilities.adoc` 332-335]: "If the pushing client
  requests this capability, the server will update the refs in one atomic transaction. Either all refs are updated or
  none."
- Fast-forward is the server's policy, not the protocol's [B: `git-receive-pack.adoc` 32-34]: "'git-receive-pack'
  honours the receive.denyNonFastForwards config option, which tells it if updates to a ref should be denied if they are
  not fast-forwards." The pre-receive hook runs first [B: `git-receive-pack.adoc` 126-127]: "This hook is called before
  any refname is updated and before any fast-forward checks are performed."
- Pushed objects are held apart until the push is admitted [B: `git-receive-pack.adoc` 260-264]: "When `receive-pack`
  takes in objects, they are placed into a temporary "quarantine" directory within the `$GIT_DIR/objects` directory and
  migrated into the main object store only after the `pre-receive` hook has completed. If the push fails before then,
  the temporary directory is removed entirely."
- The protocol authorizes nothing [B: `gitprotocol-pack.adoc` 507-509]: "The protocol itself contains no authentication
  mechanisms. That is to be handled by the transport, such as SSH, before the 'receive-pack' process is invoked."

### 2.2 Mercurial: the heads the client saw are checked before the push applies

- `unbundle` carries the heads the client observed [B: `wireprotocol.txt` 1131-1134]: "Accepts the argument ``heads``,
  which is a space-delimited list of hex nodes corresponding to server repository heads observed by the client. This is
  used to detect race conditions and abort push operations before a server performs too much work or a client transfers
  too much data."
- The answer is the change in heads [B: `wireprotocol.txt` 1139-1142]: "a special ``push response`` type is returned.
  This type contains an integer describing the change in heads as a result of the operation. A value of ``0`` indicates
  nothing changed. ``1`` means the number of heads remained the same."
- The bundle2 form of the same checks [B: `bundle2.txt` 350-351, 383-387, 325-327 and 342-345]: ``check:heads`` "is a
  means to validate that the sender's state of DAG heads matches the receiver's"; ``check:updated-heads`` is "nearly
  identical to ``check:heads`` except the heads in the payload are only a subset of heads in the repository";
  ``check:bookmarks`` validates "that the sender's known state of bookmarks matches the receiver's ... (In the case of
  pushes, this mismatch signifies a race condition and the receiver should consider rejecting the push.)"

### 2.3 Subversion: a commit merges into the newest revision node by node, and a file changed on both sides fails

- A commit is a request followed by an editor drive, and its answer names the new revision [B: ra_svn `protocol`
  296-306]: "Upon receiving response, client switches to editor command set. Upon successful completion of edit, server
  sends auth-request. After auth exchange completes, server sends commit-info. ... commit-info: ( new-rev:number
  date:string author:string ? ( post-commit-err:string ) )".
- Committing a transaction made against an older revision merges it with what was committed since [B: `svn_fs.h`
  1187-1190]: "If the transaction conflicts with other changes committed to the repository, return an
  #SVN_ERR_FS_CONFLICT error. Otherwise, create a new filesystem revision containing the changes made in @a txn".
- The merge is node by node, and a file changed on both sides is a conflict [B: `svn_fs.h` 2263-2266]: "@a source, @a
  target, and @a ancestor are generally directories; this function recursively merges the directories' contents. If they
  are files, this function simply returns an error whenever @a source, @a target, and @a ancestor are all distinct node
  revisions."
- The out-of-date rule is a comparison with the path's last change [B: `svn_fs.h` 3052-3055, `svn_fs_lock_many`]: "If @a
  current_rev is a valid revnum, then do an out-of-dateness check. If the revnum is less than the last-changed-revision
  of the path (or if the path doesn't exist in HEAD), yield an #SVN_ERR_FS_OUT_OF_DATE error for this path."

### 2.4 Hooks: each tool runs an admission hook before the change is kept, and observers after

- git [B: `git-receive-pack.adoc` 126-131]: pre-receive "is called before any refname is updated and before any
  fast-forward checks are performed. If the pre-receive hook exits with a non-zero exit status no updates will be
  performed, and the update, post-receive and post-update hooks will not be invoked either."
- Mercurial [B: `config.txt` 1382-1389, 1220-1223]: ``pretxnchangegroup`` is "Run after a changegroup has been added via
  push, pull or unbundle, but before the transaction has been committed. ... A non-zero status will cause the
  transaction to be rolled back, and the push, pull or unbundle will fail"; ``changegroup`` is "Run after a changegroup
  has been added via push, pull or unbundle."
- Subversion [B: `svn_repos.h` 2476-2489]: the repository's commit "invoke[s] the @a repos' pre- and post-commit hooks
  around the commit ... If the pre-commit hook fails, do not attempt to commit the transaction ... If the commit
  succeeds the post-commit hook is executed."

## 3. What follows for slates (each claim of §4.17.1 and the lines above that carry it)

1. **git's and Mercurial's push is a compare-and-swap on the branch's value**, so a green per branch whose head is one
   attribute that every push sets makes §4.16's verdict give their answers: the same new value is `AcceptIdentical`,
   another value is a conflict (§2.1 lines 603-607; §2.2 lines 1131-1134 and `check:heads`). All-or-none over several
   refs is one register append when a repository's greens share one register (§2.1, `atomic`).
2. **Subversion's commit is §4.16's verdict at file granularity, as written**: its merge accepts a path changed on one
   side, fails a file changed on both (§2.3 `svn_fs_merge`), and its out-of-date test is `last_changed[path] > base`,
   which is the negation of §4.16's fast path (§2.3, `svn_fs_lock_many`'s check). Subversion needs no head attribute: a
   commit made against an older revision lands when its paths did not move.
3. **A stock client's accepted push must leave exactly its bytes**: git names a ref's new value by the client's commit,
   and only report-status-v2 lets a server name another (§2.1, lines 664-668); Subversion's merge never combines two
   changes to one file (§2.3). So a stock push is decided at whole-file granularity, and byte-range merges are for works
   the server turns into the tool's objects itself.
4. **The work is the quarantine**: git holds pushed objects apart until the push is admitted and removes them if it
   fails (§2.1, lines 260-264); a work holds what its push brought, and a refused work releases it.
5. **Policy is the server's, and the transport authenticates**: fast-forward is configuration and hooks run before it
   (§2.1); every tool has an admission hook that can refuse and observers after the commit (§2.4); no protocol carries
   authorization (§2.1, lines 507-509), so slates authorizes each principal the transport proves.

## 4. Owed reading (named so it is not forgotten)

- The git server's (§4.17.2) is in §5.4.
- Mercurial (§4.17.3, g291) and Subversion (§4.17.4, g292): their own sections' sources.

## 5. The git server (§4.17.2)

### 5.1 Sources and revisions

| Source | Revision read | sha256 (first 16 hex) |
|---|---|---|
| git `Documentation/gitprotocol-v2.adoc`, `gitprotocol-http.adoc`, `gitprotocol-common.adoc`, `gitformat-pack.adoc`, `gitformat-commit-graph.adoc`, `git-http-backend.adoc`, `rev-list-options.adoc`, `config/pack.adoc`, `config/receive.adoc` | git `master` at `e1ec09b23c8e0deb21a0b95ac5996422277395d4` | 64ef6a810d28ef39, 7eef045c485eb363, bcb00e39ed96cf2e, ec285ba4f4e15a68, 7b089ad1cc15fa32, 40346bdc94072d27, 00a963603ae954d1, c8f72f4e6e272fae, 4c50a43076bf4bba |
| git `Documentation/technical/hash-function-transition.adoc`, `bitmap-format.adoc`, `pack-heuristics.adoc` | the same | 5525e6da835a7589, 979540cc5cebfc9d, c26902ab23b163e3 |
| Git LFS `docs/api/batch.md`, `basic-transfers.md`, `locking.md`, `authentication.md`, `docs/spec.md` | git-lfs `main` at `33752a72ba9a30fc591a8a58ed8fc55197a36928` | eafbead6a8752ea7, 72c293217d844f25, 6582abda1c171aae, 8b257338b66ae62f, 14ea5fc720fbfe03 |
| Gitaly `README.md`, `doc/design_pack_objects_cache.md`, `doc/object_pools.md`, `doc/design_ha.md` | gitaly `master` at `ca6ca9dbe8bc12efc4a0272131c450a680e5c28e` | c5703542d3e3d3e4, 3dc78f8c209293ad, 9ed418ec8c138eed, ee3df625fa2da8d9 |
| RFC 4252, 4253, 4254 (SSH), 8709 (Ed25519 for SSH), 8731 (curve25519-sha256), 5647 (AES-GCM for SSH) | the RFC Editor's text | a36b882cf77b1073, b471173305154f6c, bfc727e9350067ea, ea4aa3a8feec6bd5, 26dcf1e8481d83e0, a0ee7571f0fa4a2b |
| OpenSSH `PROTOCOL` | openssh-portable `master` at `fa4b2a82be7df9768272cfabc2d78e4a12c2b3c0` | 91b7534f158c66ae |

### 5.2 What the git server rests on (quoted)

- **Framing** [B: `gitprotocol-common.adoc` 87-88, 93]: "The maximum length of a pkt-line's data component is 65516
  bytes. Implementations MUST NOT send pkt-line whose length exceeds 65520"; "A pkt-line with a length field of 0
  ("0000"), called a flush-pkt". Protocol v2 adds "'0001' Delimiter Packet" and "'0002' Response End Packet"
  [B: `gitprotocol-v2.adoc` 46-47].
- **Asking for v2** [B: `gitprotocol-v2.adoc` 72-73, 81]: over SSH "the GIT_PROTOCOL environment variable must be set
  explicitly to include "version=2". The server may need to be configured to allow this environment variable"; over
  HTTP the client asks "by supplying "version=2" in the `Git-Protocol` header". v2's commands are `ls-refs`, `fetch`,
  `object-info` and `bundle-uri` (its sections at 197, 241, 556 and 602); "Protocol version 2 is stateless by default"
  (170).
- **Smart HTTP** [B: `gitprotocol-http.adoc` 126-127, 131-132, 264, 281]: "If there is a repository at `$GIT_URL`, but
  access is not currently permitted, the server MUST respond with the `403 Forbidden` HTTP status code"; "Servers SHOULD
  support chunked encoding for both request and response bodies"; "The Content-Type MUST be
  `application/x-$servicename-advertisement`"; "Clients MUST verify the first pkt-line is `# service=$servicename`".
- **git-http-backend** [B: `git-http-backend.adoc` 15, 28]: "A simple CGI program to serve the contents of a Git
  repository to Git clients"; "By default, only the `upload-pack` service is enabled".
- **Thin packs** [B: `gitprotocol-capabilities.adoc` 116-121, 123-125]: "The upload-pack server advertises 'thin-pack'
  when it can generate and send a thin pack ... A client MUST NOT request the 'thin-pack' capability if it cannot turn a
  thin pack into a self-contained pack"; "Receive-pack, on the other hand, is assumed by default to be able to handle
  thin packs".
- **Errors** [B: `gitprotocol-pack.adoc` 40]: `error-line = PKT-LINE("ERR" SP explanation-text)`.
- **Pack format** [B: `gitformat-pack.adoc` 39-73, 114-130, 300-301]: a pack is "PACK", a version, an object count,
  entries with "n-byte type and length (3-bit type, (n-1)*7+4-bit length)", a base "name if OBJ_REF_DELTA or a negative
  relative offset ... if this is an OBJ_OFS_DELTA object", and a trailer checksum; "Ref-delta can also refer to an
  object outside the pack (i.e. the so-called "thin pack")"; the version 2 index keeps a CRC32 per object, "new in v2 so
  compressed data can be copied directly from pack to pack during repacking without undetected data corruption".
- **Commit graph** [B: `gitformat-commit-graph.adoc` 20, 29]: it keeps "The generation number of the commit" and "The
  Bloom filter of the commit carrying the paths that were changed between the commit and its first parent".
- **Bitmaps** [B: `bitmap-format.adoc` 6]: "Bitmaps store reachability information about the set of objects in a
  packfile, or a multi-pack index".
- **Hardened SHA-1** [B: `hash-function-transition.adoc` 35-37]: "Git v2.13.0 and later subsequently moved to a hardened
  SHA-1 implementation by default, which isn't vulnerable to the SHAttered attack, but SHA-1 is still weak."
- **git's fixed tuning** [B: `config/pack.adoc` 3, 7-8; `pack-heuristics.adoc` 275-276]: `pack.window` "Defaults to 10";
  `pack.depth` "Defaults to 50. Maximum value is 4095"; the heuristics notes: "We limit the delta depth to another magic
  value (right now both the window and delta depth magic values are just "10")".
- **receive-pack's options** [B: `config/receive.adoc` 34-37, 52-58, 117-127]: `receive.fsckObjects` "Defaults to
  false"; `receive.keepAlive` "The default is 5 seconds"; `receive.procReceiveRefs`: "pushing to reference such as
  "refs/for/master" will not create or update a reference named "refs/for/master", but may create or update a pull
  request directly by running the hook "proc-receive"".
- **Filters** [B: `rev-list-options.adoc` 990-1024]: "The form `--filter=blob:none` omits all blobs", then
  `blob:limit=<n>`, `object:type=`, `sparse:oid=`, `tree:<depth>` and `combine:`.
- **Git LFS** [B: `docs/api/batch.md` 1-27, 125-160]: "The Batch URL is built by adding `/objects/batch` to the LFS
  server URL", requests are `POST` with `Accept` and `Content-Type: application/vnd.git-lfs+json`, and each object in
  the answer carries `actions` with `href`, `header` and `expires_in`; `hash_algo` "defaults to `sha256`".
- **Gitaly** [B: `README.md`; `doc/design_pack_objects_cache.md`; `doc/object_pools.md`]: "Gitaly is a Git RPC service
  for handling all the Git calls made by GitLab"; its pack-objects cache sits behind `uploadpack.packObjectsHook`, which
  runs `gitaly-hooks` in place of `git pack-objects` ("We designed this cache to solve a specific problem on GitLab.com:
  high Gitaly server CPU load due to massively parallel CI fetches"); forks share objects through object pools,
  "essentially a repository which holds the shared objects of both repositories", linked by `objects/info/alternates`.
- **SSH** [B: RFC 4254 §6.4 "Environment Variable Passing" (lines 654-661), §6.5 (679-703)]: environment variables: "It
  is recommended that implementations either maintain a list of allowable variable names or only set environment
  variables after the server process has dropped sufficient privileges"; `exec`: "Normal precautions MUST be taken to
  prevent the execution of unauthorized commands." OpenSSH's ciphers [B: OpenSSH `PROTOCOL` §1.6-§1.7, lines 78-92]:
  "Because of problems with the design of the algorithm negotiation in this RFC, OpenSSH (and other SSH implementations)
  use different rules", with `chacha20-poly1305@openssh.com` described by an Internet-Draft.

### 5.3 What follows for the git server

1. Fetch speaks v2 and push speaks receive-pack's exchange, because v2 defines no push command (§5.2, "Asking for v2").
2. A fetch for a branch's own new history needs no walk when each version keeps the objects it brought: git's bitmaps
   answer the same reachability question from a precomputed structure (§5.2, "Bitmaps"), and a version chain gives
   that structure by construction.
3. Pack fragments can be joined without re-encoding: offset deltas are relative to their own entry, id deltas may name a
   base the client holds in a thin pack, and git itself copies compressed entries between packs, guarding them with a
   CRC32 per entry (§5.2, "Pack format", "Thin packs").
4. A push must be checked always, since git's own check is off by default (§5.2, "receive-pack's options"), and its
   object names must be computed with collision detection (§5.2, "Hardened SHA-1").
5. git fixes its delta window and depth and its keepalive by constants (§5.2, "git's fixed tuning", "receive-pack's
   options"); §4.17.2 learns each from the deployment.
6. Push-to-review is git's own documented pattern (`receive.procReceiveRefs`), answered with report-status-v2
   (§2.1 and §5.2).

### 5.4 Owed reading (git)

- RFC 8332 and RFC 5656 (RSA with SHA-2 and ECDSA keys for SSH), and the Internet-Drafts OpenSSH names for its AES-GCM
  and ChaCha20-Poly1305 rules, before the SSH server's piece.
- git's `gitformat-bundle` and `technical/bundle-uri.adoc` before piece 14; `technical/partial-clone.adoc` and
  `shallow.adoc` (downloaded, read only for their definitions so far) before piece 7.
- GitLab Workhorse's own documentation, to confirm it is the HTTP front that drives Gitaly for git clients (§4.17.2
  names it from memory).
