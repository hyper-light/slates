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
- Mercurial (§4.17.3, g291): its reading notes are their own file, `research/mercurial-server.md`.
- Subversion (§4.17.4, g292): its reading notes are `research/vcs-servers-subversion.md` (A-500), the evidence behind
  §4.17.4.

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

- RFC 8332 and RFC 5656's key forms (RSA with SHA-2 and ECDSA keys for SSH), before the SSH server's authentication
  part. The Internet-Drafts OpenSSH names for its AES-GCM and ChaCha20-Poly1305 rules, owed here before the transport,
  were read for A-594 (§5.5).
- `technical/partial-clone.adoc` and `shallow.adoc` (downloaded, read only for their definitions so far) before
  piece 7. git's `gitformat-bundle` and `technical/bundle-uri.adoc`, owed before piece 14, were read for A-624 (§5.6).
- GitLab Workhorse's own documentation, to confirm it is the HTTP front that drives Gitaly for git clients (§4.17.2
  names it from memory).

### 5.5 The SSH transport (A-594, read 2026-11-19)

| Source | Revision read | sha256 (first 16 hex) |
|---|---|---|
| RFC 4250, 4251, 4253, 4344, 5647, 5656, 7748, 8032, 8308, 8709, 8731 | the RFC Editor's text | eb3a9d7c3e1fae53, 23a5052796d29edc, b471173305154f6c, be7136101f416191, a0ee7571f0fa4a2b, 0cc84636afaf4860, 279ca0ecc5e92e29, ed63657ff3893012, c7d36121bcd0f242, ea4aa3a8feec6bd5, 26dcf1e8481d83e0 |
| `draft-ietf-sshm-strict-kex-03`, `draft-ietf-sshm-chacha20-poly1305-04`, `draft-miller-sshm-aes-gcm-01` | the IETF archive's text (each the newest revision the datatracker named that day) | cda5d60229f73469, a8aa91696db885d2, 3ea287492f70cbec |
| OpenSSH `PROTOCOL`, `kex.c`, `packet.c` | openssh-portable `master` at `fa4b2a82be7df9768272cfabc2d78e4a12c2b3c0` | 91b7534f158c66ae, 5b8e7fb70d685e7b, 81ddae0fafc0c2a2 |
| NIST CAVS 14.1 SSH KDF vectors | AWS-LC `crypto/evp_extra/sshkdf_tests.txt` at `02561621ffa4cf17c0c4f70bc11a82df36b42ae9`, the vendored aws-lc-sys's commit (`vendor/UPSTREAM.md`) | a492dbca6c2637f0 |

What the transport rests on (quoted):
- **OpenSSH's extensions are now drafts** [B: `PROTOCOL` §1.6, §1.7, §1.9]: AES-GCM "as described in"
  `draft-miller-sshm-aes-gcm`, ChaCha20-Poly1305 in `draft-ietf-sshm-chacha20-poly1305`, and the Terrapin hardening,
  "collectively referred to as "strict KEX"", in `draft-miller-sshm-strict-kex` (now `draft-ietf-sshm-strict-kex`).
- **Strict key exchange** [B: strict-kex draft §3.1-§3.3]: "a client indicates support ... by including the standard
  "kex-strict-c" identifier and/or the pre-standard "kex-strict-c-v00@openssh.com" identifier"; servers "SHOULD offer
  both"; the markers "are valid only in the initial SSH_MSG_KEXINIT"; "implementations MUST terminate the connection if
  they receive a non-KEX message during the initial key exchange"; "MUST verify that the SSH_MSG_KEXINIT was the first
  message received from the peer"; "MUST ensure that the sequence number does not wrap ... prior to the completion of
  the initial KEX"; a server "MUST only accept" `SSH_MSG_KEX_ECDH_INIT` once; "the sequence number used when sending
  packets MUST be reset to zero immediately after any SSH_MSG_NEWKEYS packet is sent".
- **ChaCha20-Poly1305** [B: the chacha draft §6-§8, Appendix A]: the second half of the 64-byte key encrypts the length
  alone, the first half the rest and keys Poly1305; the nonce is the sequence number; "the MAC MUST be checked before
  decryption"; "ChaCha20 must never reuse a {key, nonce}"; Appendix A works one packet through to its wire bytes, the
  golden vector the crate holds.
- **AES-GCM** [B: the AES-GCM draft §2; RFC 5647 §7.1-§7.3]: "AEAD modes ... MUST be listed in the
  "encryption_algorithms" field ... only", and "MAC negotiation MUST be skipped"; the IV is "a 4-octet fixed field and an
  8-octet invocation counter field", "incremented after each invocation"; the length is "additional authenticated
  data, not ... plaintext".
- **Rekeying** [B: RFC 4344 §3.1, §3.2; RFC 4253 §9]: "SHOULD rekey at least once every 2**32 outgoing packets", with
  "rekey after receiving more than 2**31 packets" the preferred receiving point; for a block cipher of L ≥ 128 bits,
  "SHOULD NOT encrypt more than 2**(L/4) blocks"; and RFC 4253's "after each gigabyte of transmitted data or after
  each hour of connection time", with "should not be performed too often".
- **The exchange** [B: RFC 8731 §3, §3.1; RFC 5656 §4]: "MUST check whether the computed Diffie-Hellman shared secret
  is the all-zero value", "MUST also abort if the length of the received public keys are not the expected lengths"; X
  read "as an unsigned fixed-length integer encoded in network byte order", then an `mpint`; the hash's eight fields.
- **OpenSSH's own reader** [C: `kex.c` `kex_exchange_identification`; `packet.c`]: a server refuses lines before the
  client's identification ("Do not accept lines before the SSH ident from a client"), a NUL or a CR not followed by LF
  is invalid, a bare LF ends a line, and `sscanf("SSH-%d.%d-%[^\n]\n")` takes any software version; packets under an
  AEAD must have `packlen` of at least 5 and a multiple of the block size after the length; `MAX_PACKETS (1U<<31)` in
  either direction forces a rekey, as do `1 << (block_size*2)` blocks for 16-byte blocks.

What follows for slates (A-594): every bound on a key comes from its cipher's analysis, never from a chosen size; the
time bound is learned (§4.17.2's tunables table); the transport takes both markers, enforces strict key exchange when
offered, and keeps its sequence-number rules exactly as the draft states, since ChaCha20-Poly1305's nonce is that
number.

### 5.6 `object-info`, `bundle-uri` and `packfile-uris` (A-624, read 2026-11-19)

| Source | Revision read | sha256 (first 16 hex) |
|---|---|---|
| `Documentation/gitprotocol-v2.adoc`, `technical/bundle-uri.adoc`, `gitformat-bundle.adoc`, `config/bundle.adoc`, `config/uploadpack.adoc`, `config/transfer.adoc` | git `v2.50.1` | 2e7399659d1fb86a, c00e786e56cde209, 699b92c6b6a32d81, 3c817153e6f4e66f, 66ae6a63da96ef5a, 41778b268e1f27f7 |
| `protocol-caps.c`, `bundle-uri.c`, `serve.c`, `upload-pack.c`, `builtin/pack-objects.c`, `fetch-pack.c`, `connect.c` | git `v2.50.1` | 1310c747bcf8e6a4, 30944f34d1f85e46, df0cff2ee21a974a, 17f18f0bdbdf0b88, 9815de1ae622a3ce, 885b2166a5b0258d, 4a4584b959030542 |
| `Documentation/gitprotocol-v2.adoc`, `protocol-caps.c` | git `master` that day | 64ef6a810d28ef39, 3dc3b0ff8a5b067a |

What the three rest on (quoted):
- **`object-info`** [B: gitprotocol-v2 "object-info"]: "Object size is the only information that is currently
  supported"; the arguments `size` and `oid <oid>`; `obj-info = obj-id SP obj-size`. [C: `protocol-caps.c`] `send_info`
  writes nothing for a request naming no object ("if (!oid_str_list->nr) return"), the `size` line only when asked,
  every line by `packet_writer_write` (no LF, where the grammar has one), an object it cannot find as its id and a space,
  and each id as its client wrote it; an argument it does not know gets an `ERR` line and the request goes on
  ("object-info: unexpected line"). [C: `serve.c`] it is advertised under `transfer.advertiseobjectinfo`, off by default;
  nothing in 2.50.1's `fetch-pack.c`, `transport.c` or `cat-file` asks for it. [B: master] adds `type`: "Currently only
  object size and type are supported".
- **`bundle-uri`** [B: gitprotocol-v2 "bundle-uri"]: "A `bundle-uri` request takes no arguments"; the answer is "a list
  of key-value pairs provided as packet lines with value `<key>=<value>`"; "Clients MUST parse the line according to the
  above format, lines that do not conform to the format SHOULD be discarded". [C: `bundle-uri.c`] the server writes each
  `bundle.*` key of its configuration by `packet_write_fmt("%s=%s")`, with no LF, and dies on an argument; git's client
  (`bundle_uri_parse_line`, `bundle_list_update`) errs on a line without `=`, an empty key or value, a key outside
  `bundle`, a version other than 1, a mode other than `all` and `any`, and a bundle's second `uri`, and ignores keys and
  heuristics it does not know. [B: `config/bundle.adoc`] "The only value currently understood is `creationToken`". [C:
  `connect.c` `get_remote_bundle_uri`] the client sends its capabilities, then `command=bundle-uri`, a delimiter and a
  flush.
- **`packfile-uris`** [B: gitprotocol-v2 "fetch"]: "Before performing the connectivity check, the client should
  download from all given URIs"; "For each URI the server sends, it sends a hash of the pack's contents (as output by
  git index-pack) followed by the URI". [C: `pack-objects.c` `want_object_in_pack_mtime`] an object configured by
  `uploadpack.blobPackfileUri` is left out of the pack when its URI begins with a protocol the client listed and then
  `:`. [C: `upload-pack.c`] the list is cleared without `sideband-all` ("if (data->uri_protocols.nr &&
  !data->writer.use_sideband) string_list_clear"). [C: `fetch-pack.c`] the inline pack is fetched and indexed first
  (`get_pack`); then each URI's pack is downloaded in the order listed and indexed with the inline pack's own
  `index-pack` arguments (`--fix-thin` only under `thin-pack`), and checked against its hash ("does not match expected
  hash").

What follows for slates (A-624): `object-info` answers git's bytes from the index rows; a `bundle-uri` answer is the
repository's list written as git writes its configuration, and read as git's client reads it; and the
`packfile-uris` choice sends by URI only a suffix of the fetch's units that URI packs tile in order, since the inline
pack is indexed before any URI pack can supply a base.

### 5.7 The SSH server's authentication and channel (A-633, read 2026-11-19)

| Source | Revision read | sha256 (first 16 hex) |
|---|---|---|
| RFC 4252, 4254, 8332 | the RFC Editor's text | a36b882cf77b1073, bfc727e9350067ea, b194d8da7a5b07cf |
| git `quote.c`, `connect.c` | git `master` at `e1ec09b23c8e0deb21a0b95ac5996422277395d4` | 9a53645731a1b83f, 02363f9f84e69ca0 |
| Mercurial `mercurial/sshpeer.py`, `mercurial/utils/procutil.py`, `contrib/hg-ssh` | 7.2's PyPI source distribution, SHA-256 `a250227e…` (as §4.17.3's notes) | d84a797aff1ce219, c5412a454a414111, 323bfa2b2375e0a0 |
| Subversion `subversion/libsvn_ra_svn/client.c` | `1.14.x` at `d434dc14866e0c48602dba59a15443255d0b8c1e` | 3c3b9263dd7223a9 |

What it rests on (quoted):
- **The probe and the query** [B: RFC 4252 §4, §5.2, §7]: "The main purpose of sending this request is to get the
  list of supported methods from the server"; a query "MUST respond ... with either SSH_MSG_USERAUTH_FAILURE or ...
  SSH_MSG_USERAUTH_PK_OK"; the signed data's eight fields; "partial success MUST be TRUE if the authentication request
  to which this is a response was successful"; "SHOULD limit the number of failed authentication attempts ... (the
  RECOMMENDED limit is 20 attempts)".
- **RSA with SHA-2** [B: RFC 8332 §3, §3.2]: the `ssh-rsa` key format is kept; "the client MUST encode the same algorithm
  name in the signature"; "A verifier MAY accept shorter encodings of S with one or more leading zeros omitted".
- **Extensions** [B: RFC 8308 §2.4, §3.1]: the server may send `SSH_MSG_EXT_INFO` "As the next packet following the
  server's first SSH_MSG_NEWKEYS"; "a server MUST enumerate all public key algorithms it might accept during user
  authentication".
- **Channels** [B: RFC 4254 §5.1-§5.3, §6.4, §6.5, §6.10]: the open and its failure reasons; "MUST NOT advertise a
  maximum packet size that would result in transport packets larger than its transport layer is willing to receive";
  the window "MUST NOT be increased above 2^32 - 1 bytes"; "Only one of these requests can succeed per channel"; "The
  channel needs to be closed with SSH_MSG_CHANNEL_CLOSE after this message".
- **The clients' commands** [C]: git builds `prog SP sq_quote(path)` and `sq_dequote` reads a quoted run then only
  `\'` or `\!` between reopened quotes; Mercurial 7.2 sends `%s -R %s serve --stdio%s` with `_serverquote` (bare for
  `[a-zA-Z0-9@%_+=:,./-]`, else `'...'` with `'\''`), and `contrib/hg-ssh` takes exactly `['hg', '-R', path, 'serve',
  '--stdio']`; Subversion's tunnel agent appends `svnserve -t` after the host.

### 5.8 The post-quantum hybrid exchange (A-634, read 2026-11-19)

| Source | Revision read | sha256 (first 16 hex) |
|---|---|---|
| `draft-ietf-sshm-mlkem-hybrid-kex-10` | the IETF archive's text (the newest revision the datatracker named that day) | d7e1ff8723466a80 |
| FIPS 203 (ML-KEM) | `NIST.FIPS.203.pdf` from nvlpubs.nist.gov | fe1f12f32a7e44ec |
| OpenSSH release notes 10.0, 10.1, 10.2 | openssh.com's `txt/release-10.0`, `-10.1`, `-10.2` | 988550ce34c71941, f0465b36d7886bad, af431f2e9f30b280 |
| OpenSSH's post-quantum page | `https://www.openssh.com/pq.html` | 6a6dc8e9af25ff99 |
| OpenSSH `kexmlkem768x25519.c`, `myproposal.h` | openssh-portable `master` at `fa4b2a82be7df9768272cfabc2d78e4a12c2b3c0` | 0fbdf9f4baeab94f, 2d390158f9ae54d6 |
| AWS-LC's ML-KEM: `ml_kem.h`, `mlkem_native_config.h`, `mlkem/kem.c` | the vendored aws-lc-sys 0.45.0, `vendor/aws-lc-sys/aws-lc/crypto/fipsmodule/ml_kem/` | 0709c8738f69680c, 671e5e7137733b21, 3922156826521429 |

What it rests on (quoted):
- **The exchange** [B: the draft §2.1, §2.3.3, §2.4, §2.5]: `C_INIT = C_PK2 || C_PK1` and `S_REPLY = S_CT2 || S_PK1`,
  the ML-KEM value first; the server "MUST check that the length of the C_INIT is the sum of the expected length of each
  public key" and "MUST perform the encapsulation key checks defined in Section 7.2 of [FIPS203]", and on failure abort
  with `SSH_DISCONNECT_KEY_EXCHANGE_FAILED`; "K = HASH(K_PQ || K_CL)", the two secrets "encoded as fixed-length byte
  arrays, not as integers"; "K is encoded as a string"; the messages are numbers 30 and 31 (§2.2); and no method of the
  draft makes a packet past RFC 4253's 35000 bytes (§3). The draft gives no test vectors.
- **ML-KEM** [B: FIPS 203 §7.2, Table 2, Table 3, Algorithm 20]: the encapsulation key check is a type check (length
  384k + 32) and a modulus check (`ByteEncode12(ByteDecode12(ek[0:384k]))` equal to its input, so every integer is in
  `[0, q - 1]`); "ML-KEM.Encaps shall not be run with an encapsulation key that has not been checked", though the check
  "need not be performed by the encapsulating party"; ML-KEM-768 has k = 3, q = 3329, d_u = 10, d_v = 4, a 1184-byte
  encapsulation key, a 1088-byte ciphertext and a 32-byte shared secret; `m` is "32 random bytes".
- **AWS-LC** [C]: `mlk_kem_enc_derand` calls `mlk_kem_check_pk` ("Implements @[FIPS203, Section 7.2, Modulus check]",
  `kem.c` line 331), which decodes, reduces, re-encodes and compares; `mlk_randombytes` is `RAND_bytes` under
  `AWSLC_ABORT_IF_NOT_ONE` (`mlkem_native_config.h` 81-89); `ml_kem.h` names 1184, 1088 and 32 bytes; aws-lc-rs's
  `EncapsulationKey::new` checks the length alone and `encapsulate` takes no bytes of the caller's.
- **OpenSSH** [B: release notes; pq.html; C: source]: 10.0 (2025-04-09) "the hybrid post-quantum algorithm
  mlkem768x25519-sha256 is now used by default for key agreement"; 10.1 (2025-10-06) "add a warning when the connection
  negotiates a non-post quantum key agreement algorithm", "controlled via a new WarnWeakCrypto ssh_config option,
  defaulting to on"; 10.2 (2025-10-10) has nothing on it; the page gives the warning ("** WARNING: connection is not
  using a post-quantum key exchange algorithm.") and names the two methods that silence it, `mlkem768x25519-sha256`
  and `sntrup761x25519-sha512`; `KEX_CLIENT_KEX` lists `mlkem768x25519-sha256` first; `kex_kem_mlkem768x25519_enc`
  checks the length, encapsulates, and puts SHA-256 of the ML-KEM secret and the raw X25519 secret as a string.

What follows for slates (A-634): the server offers the hybrid first and keeps `curve25519-sha256` for clients without
it; the modulus check is AWS-LC's, held by a hostile test rather than written twice; `sntrup761x25519-sha512` is not
offered, since AWS-LC has no NTRU Prime and every client that warns offers the hybrid; and the encapsulation's
randomness is AWS-LC's own, the same generator the crate's entropy reads, so only the tests' seeded transcripts lose
reproducibility under the hybrid.

### 5.9 Protected branches: git's own refusals, a hook's words, and what a commit signature covers (A-644, read 2026-11-19)

| Source | Revision read | sha256 (first 16 hex) |
|---|---|---|
| `builtin/receive-pack.c`, `commit.c`, `gpg-interface.c` | git `v2.50.1` | 6e3cec699acf968d, 19583027a3899ea3, bed3d128e23439e4 |
| `Documentation/config/receive.adoc`, `Documentation/githooks.adoc` | git `v2.50.1` | 4c50a43076bf4bba, dc7165822ba02f5c |
| `routers/private/hook_pre_receive.go`, `routers/private/hook_verification.go` | Gitea `v1.24.0` | cb0325ee50ed2074, 48dcea533dea9fec |

What a protected branch rests on (quoted):
- **git refuses two things itself** [B: `config/receive.adoc`]: `receive.denyNonFastForwards` "will deny a ref update
  which is not a fast-forward ... even if that push is forced", and `receive.denyDeletes` "will deny a ref update that
  deletes the ref". [C: `receive-pack.c` `update()`, lines 1519-1580] a delete of a branch: `rp_error("denying ref
  deletion for %s", name)` and the reason `deletion prohibited`; a non-fast-forward of a branch: `rp_error("denying
  non-fast-forward %s" " (you should pull first)", name)` and `non-fast-forward`, both judged before the update hook
  runs.
- **The rest is a hook's** [B: `githooks.adoc` "update"]: "The hook executes once for each ref to be updated"; "Exiting
  with a non-zero status prevents `git receive-pack` from updating that ref". [C: `update()`] after a refusing hook,
  `rp_error("hook declined to update %s", name)` and the reason `hook declined`. [C: `run_update_hook`,
  `copy_to_sideband`, line 502] the hook's standard error and output go to band 2 as read, `char data[128]` a read, so a
  line longer than 128 bytes arrives in pieces (captured: `fixtures/protected/untrusted.response.bin`).
- **What a commit signature covers** [C: `commit.c` `parse_buffer_signed_by_header`, line 1170]: a line of the
  repository's own header (`gpg_sig_headers`: `gpgsig`, `gpgsig-sha256`) and its continuation lines are the signature,
  every one in order; any other line that `starts_with(line, "gpgsig")` is left out of the payload with its continuation
  lines; everything else is the payload. [C: `gpg-interface.c` `get_format_by_sig`, line 136] the format is chosen by
  `starts_with(sig, ...)` over `openpgp_sigs` (`-----BEGIN PGP SIGNATURE-----`, `-----BEGIN PGP MESSAGE-----`),
  `x509_sigs` (`-----BEGIN SIGNED MESSAGE-----`) and `ssh_sigs` (`-----BEGIN SSH SIGNATURE-----`); SSH signs and
  verifies with `-n git`.
- **What a hosted server judges** [C: Gitea `hook_verification.go` lines 33-38]: "When creating a new branch, the
  oldCommitID is empty, by using "newCommitID --not --all"", and otherwise `oldCommitID + "..." + newCommitID`; its
  refusals are its pre-receive hook's (`hook_pre_receive.go`: "branch %s is protected from force push", "... from
  deletion", "... from unverified commit %s").

What follows for slates (A-644): a rewrite and a delete a branch refuses answer in git's own words; a linear history
and signed commits answer as git's update hook would, `hook declined` after the words relayed in git's 128-byte pieces.
A commit's signature and payload are split exactly as `commit.c` splits them, and its format told by its armour as
`gpg-interface.c` tells it. What a move brings is judged against its own ref alone (A-642): a create brings its whole
history. Gitea's `--not --all` reads every ref's position (a global reference, A-642) and lets a protected branch be
made at a commit pushed first to a branch nothing protects; it is not followed.

### 5.10 The SSH edge: what waits on people, and how each tool's program refuses (A-651, read 2026-11-19)

| Source | Revision read | sha256 (first 16 hex) |
|---|---|---|
| OpenSSH `sshconnect2.c`, `sshconnect.c`, `channels.h` | openssh-portable `master` at `fa4b2a82be7df9768272cfabc2d78e4a12c2b3c0` | 1caf45cb23fd452e, b5b0ec1052da2c26, ffbc133ac4c1a4c1 |
| git `usage.c`, `builtin/upload-pack.c` | git `master` at `e1ec09b23c8e0deb21a0b95ac5996422277395d4` | 114f74835706dd86, ff09e4385553b959 |
| git `serve.c`, `pkt-line.c` (A-651 part 2) | git `master` at `e1ec09b23c8e0deb21a0b95ac5996422277395d4` | 834f019139daadf5, 708420106f01ee9d |
| Mercurial `mercurial/dispatch.py`, `mercurial/scmutil.py`, `mercurial/localrepo.py` | 7.2's PyPI source distribution, SHA-256 `a250227e…` (as §5.7) | 7f057268680b0b53, cea9db559e8c7dee, 4fa160a96510dad8 |
| Subversion `subversion/svnserve/svnserve.c` | `1.14.x` at `d434dc14866e0c48602dba59a15443255d0b8c1e` | 43155c2bd557492f |

What it rests on (quoted):
- **People inside a connection** [C: OpenSSH]: `input_userauth_pk_ok` ("Server accepts key") calls
  `sign_and_send_pubkey` (`sshconnect2.c` 745); `identity_sign`, for a key that is neither in an agent nor loaded,
  "Load the private key from the file" through `load_identity_file` (1242), which builds the prompt "Enter passphrase for
  key '%.100s': " (1543) and reads it; `check_host_key` asks "Are you sure you want to continue connecting" through
  `confirm` (`sshconnect.c` 1302-1305), before the client's NEWKEYS. `CHAN_SES_PACKET_DEFAULT (32*1024)`
  (`channels.h` 224).
- **git's refusals** [C: git]: `die_builtin` reports with the prefix "fatal: " (`usage.c` 74) and `exit(128)` (87);
  upload-pack's `die("'%s' does not appear to be a git repository", dir)` when it cannot enter the repository
  (`builtin/upload-pack.c` 62).
- **git's reader at the end of its input** [C: git] (A-651 part 2): `process_request` initializes its reader with
  `PACKET_READ_GENTLE_ON_EOF` (`serve.c` 286, 295) and returns quietly when the peek of a request's first packet meets
  the end (302), then clears the flag (304); a later short read reaches `get_packet_data`'s `die(_("the remote end
  hung up unexpectedly"))` (`pkt-line.c` 339, 371), so exit 128. A request with no command dies "no command
  requested" (`serve.c` 350).
- **Mercurial's** [C: Mercurial 7.2]: `RepoError(_(b'repository %s not found') % path)` (`localrepo.py` 574),
  printed "abort: %s" by `callcatch`, whose coarse exit code is -1 (`scmutil.py` 198, 231-234, 311), and
  `sys.exit(status & 255)` (`dispatch.py` 106): 255.
- **svnserve's** [C]: a failure is reported by `svn_cmdline_handle_exit_error(err, NULL, "svnserve: ")` with
  `EXIT_FAILURE` (`svnserve.c` 1426-1427).

What follows for slates (A-651): the SSH edge reaps a stalled connection only when a new one needs its seat, since the
waits above belong to people and to a push's compression; and a command the edge cannot run is refused as its tool's
own program refuses, in its words and with its status, after the `exec` itself is accepted; and upload-pack's
session ends 0 when its client's input ends before a request's first packet is whole, and dies 128 after it (A-651
part 2).

## 6. Policy by name pattern and the hooks' protocol (§4.17.1 shared piece 7, A-597)

Read 2026-11-19 (UTC) by g255. Each file was downloaded and hashed; the lines cited are each file's own.

| Source | Revision | sha256 (first 16 hex) |
|---|---|---|
| git `Documentation/config/transfer.adoc`, `for-each-ref-options.adoc`, `githooks.adoc`, `rev-list-options.adoc`, `gitignore.adoc` | git `master` at `e1ec09b23c8e0deb21a0b95ac5996422277395d4` (§1's) | `ca01864712d813ed`, `0b7a87487ca5ec29`, `5b3808fb0dcd2675`, `00a963603ae954d1`, `338221577b4f47ea` |
| git `refs.c`, `ref-filter.c`, `wildmatch.c`, `ctype.c`, `builtin/receive-pack.c`, `t/t3070-wildmatch.sh`, `t/helper/test-wildmatch.c` | the same | `8c51d996511d3b2a`, `9dae1123d7958514`, `58541e5550a0689b`, `ef2be0e95b607f50`, `c296cdfc55dad629`, `9765041e1217bdbb`, `59553b0a94cfd884` |
| Mercurial `hgext/acl.py`, `mercurial/helptext/config.txt` | 7.2.4, the PyPI tarball §1 names (its sha256 `85839e0f…` checked again) | `e744192eb2b44b0b`, `a034617d0f449282` |
| Subversion `libsvn_repos/authz.h`, `authz.c`, `authz_info.c`, `hooks.c` | `trunk` at `fd4e82b00cbe89319728ad9e21121c40e59f92be` (§1's) | `8042f6925f6712a6`, `7cd6ac4bc227a4be`, `5f9ff5bbaaf9e187`, `555e485812a84f63` |
| Subversion 1.10 release notes, "Improved path-based authorization" | `subversion.apache.org/docs/release-notes/1.10.html` | `7c7a34f5bee69657` |

- **git: which refs a pattern covers** [B: `for-each-ref-options.adoc` 1-5]: "only refs are shown that match against at
  least one pattern, either using `fnmatch`(3) or literally, in the latter case matching completely or from the
  beginning up to a slash." [C: `ref-filter.c` 2696-2723, `match_name_as_path`]: a literal prefix that ends at a slash
  or at the name's end, or a pattern ending in `/`, else `wildmatch(p, refname, WM_PATHNAME)`.
- **git: which rule wins** [B: `config/transfer.adoc` 117-120]: "You may also include a `!` in front of the ref name to
  negate the entry, explicitly exposing it, even if an earlier entry marked it as hidden. If you have multiple hideRefs
  values, later entries override earlier ones". [C: `refs.c` 1741-1771, `ref_is_hidden`: the entries are read from the
  last, and the first that matches answers.]
- **git: the glob** [C: `wildmatch.c` 59-290, `dowild`; B: `gitignore.adoc` 124-142]: "A leading `**` followed by a
  slash means match in all directories", "A trailing `/**` matches everything inside", "A slash followed by two
  consecutive asterisks then a slash matches zero or more directories", "Other consecutive asterisks are considered
  regular asterisks". git's own vectors [C: `t3070-wildmatch.sh`, 190 `match` lines; `test-wildmatch.c`: the `wildmatch`
  mode is `WM_PATHNAME`, and an argument beginning `XXX/` loses its first three bytes]. git's classes are its own ASCII
  table [C: `ctype.c`]: space is tab, newline, carriage return and space only.
- **git: the hooks** [B: `githooks.adoc` 261-450]: `pre-receive`: "If the hook exits with non-zero status, none of the
  refs will be updated"; `update`: "takes three parameters ... the name of the ref being updated, the old object name
  ..., and the new object name"; `post-receive`: "does not affect the outcome of `git receive-pack`"; for all three,
  "Both standard output and standard error output are forwarded to `git send-pack`". [C: `builtin/receive-pack.c`
  834-995: `post-receive` is fed only the commands with no error (`skip_broken`); the push options become
  `GIT_PUSH_OPTION_<i>` and `GIT_PUSH_OPTION_COUNT`, and with none negotiated the count is removed from the
  environment; a refusal reads "pre-receive hook declined" (2127) or "hook declined" (1634).]
- **Mercurial: acl** [C: `acl.py` 25-30]: "The order in which access checks are performed is: 1) Deny list for branches
  ... 2) Allow list for branches ... 3) Deny list for paths ... 4) Allow list for paths"; keys are "a branch name, or an
  asterisk, to match any branch", and the same for bookmarks; "You can add the "!" prefix to a user or group name to
  invert the sense of the match". `buildmatch` (326 on) gives a present section with no entry for the user `util.never`,
  and `_txnhook` (432 on) refuses "denied on branch" for a deny match, then "not allowed on branch" for an allow list
  that does not match. The example configuration (119-140) and the `ring = !gollum` example (199) state their outcomes.
- **Mercurial: the hooks** [B: `config.txt` 1165-1400]: "Hooks can be prioritized by adding a prefix of ``priority.`` to
  the hook name ... The default priority is 0" (a higher priority runs first); ``pretxnchangegroup``: "Exit status 0
  allows the transaction to commit. A non-zero status will cause the transaction to be rolled back, and the push, pull
  or unbundle will fail" (1382-1389); ``changegroup``: "The ID of the first new changeset is in ``$HG_NODE`` and last is
  in ``$HG_NODE_LAST``. The URL from which changes came is in ``$HG_URL``" (1220-1223). [C: `acl.py`'s example
  configuration: `HG_SOURCE` is "serve" for all remote access.]
- **Subversion: authz** [B: 1.10 release notes]: "In these rules, /path is always matched literally"; in `:glob:` rules
  "`*` matches a single (exactly one), arbitrary path segment", "`**` matches an arbitrary number of path segments",
  "Classic wildcard patterns such as `*foo*.bar` work as expected, including escaping of special characters with a
  backslash", and "/*/**/* will match any path which contains at least 2 segments and is equivalent to /**/*/* as well
  as /*/*/**". [C: `authz.c` 56-59]: "If multiple rules apply to the same path (only possible with wildcard matching),
  the one with the highest SEQUENCE_NUMBER wins, i.e. the latest one defined in the authz file"; `lookup` (1277 on)
  takes the rules matching each depth and inherits the depth above where none does; `add_next_node` (1119 on) adds a
  `**` node again at every level; `process_acl` (558 on) skips a section with nothing for the user; `trim_tree` (707 on)
  removes the rules a later `**` eclipses. [C: `authz.h` 255-258]: "The authz lookup code selects the highest-numbered
  ACL from amongst a set of equivalent matches."
- **Subversion: the hooks** [C: `hooks.c`]: `pre-commit`'s arguments are the repository's path and the transaction, its
  input "LOCK-TOKENS:" then a `path|token` line a token (the path URI-escaped) and a blank line (510-588);
  `pre-revprop-change`'s are the path, the revision, the author (empty for none), the property's name and `A`, `M` or
  `D`, with the new value on input (622-683), and with no such hook every change is refused, "default to MASSIVE
  PARANOIA" (668-678); `post-commit`'s are the path, the revision and the transaction (590-620); a hook's standard
  output goes to the null device (228) and its standard error is the message: "Commit blocked by pre-commit hook (exit
  code %d) with output:" or "... with no output." (65-162).
- **What follows** (§4.17.1, A-597): each tool's precedence is the one its own documentation or code states, never a
  hosting server's; a policy rule is one statement, so authz's union of lines within a section stays the authorization
  file's (A-606); and every hook's deadline is learned from its own runs, with no deadline before its first finished
  run but its client's connection.
