# g294-kiln-k8s-engine — progress

Goal: kiln UI goal 88's part 01a1f1249c5e75b5bbf0c92c84385109: "kiln's Kubernetes engine runs each agent's pod on its
slates volume through the slates library (kubelet's mount of the slates export, never a host copy), with review, drift
and landing through a grant, proven on a KIND cluster through kiln". Role: implementer subagent of main.
THE RULE (read back to main 2026-11-19 ~00:30Z): one piece; only `cargo test -p <crate> --lib <filter>` on the Studio
d1323898 (sync false, own clone + CARGO_TARGET_DIR, deleted after); kiln: no tree-wide rustfmt; merge (fetch, merge
origin/main, read diff --stat, push HEAD:main alone); END with progress + run-requests + report. A-521 for a
slates-side change (none in piece 1).

## Clones
- kiln: ~/slates-work/g294-kiln-k8s-engine-kiln (clone, main), worktree ~/slates-work/g294-kiln-k8s-engine-kiln-p1
  (branch agents/g294-kiln-k8s-engine, from kiln main 7e5ccf240).
- slates (read-only, shallow, for the design and xtask kind): ~/slates-work/g294-slates-read (slates main e2d1bc4).

## Piece 1 (in progress): W10 design in kiln docs/agentic.md + the first unit-testable step
- Section number: W10 (W9 left to g293's OCI/Docker engine; told g293 by inbox + message).
- First step: a fleet runner's node certificate also names the host its node is listed at ([slates_fleet.nodes]),
  an iPAddress SAN for an IPv4 literal, a dNSName otherwise. Why: slates serves the network export with the node's
  fleet identity (crates/server/src/fleet.rs serve_export), and an RPC-with-TLS client must check the name it mounted
  against the server's certificate (RFC 9289 5.2.1; ktls-utils tlshd: gnutls_certificate_verify_peers3(session,
  parms->peername)); kiln's CA named only slates.kiln and r0.d<domain>.slates.kiln, so no kubelet could verify it.
- Coordination: g293 acked (W9 its containers section, W10 mine; both after W8; its A-520 raises slates-api CONTRACT
  41 -> 42, so my A-521 takes the next number and adds its paragraph after g293's).
- RED commit 8220ea03d (branch agents/g294-kiln-k8s-engine, pushed): W10 design + status row W; host threaded through
  issue/prepared/finished/identity/admit/join (NodeIssued.host optional) while still naming the fleet alone; new test
  w10_a_listed_runners_node_names_its_host_so_a_client_of_its_export_verifies_it (rustls name check); K1-K4 helpers list
  each node at the address it announces. Studio run r178: `cargo +stable test --locked -p kiln --lib
  workspace::slates_fleet` in ~/.cache/g294-k8s (log red.log there).
- FIX prepared locally (uncommitted until red is seen): pki sign_slates_node names `host`, refuses kiln-server as a
  host; node_names includes the host; module docs.
- Decided, not built (reported to main): a node certificate is serverAuth+clientAuth (Slates needs both), so a listed
  host the kiln server's own certificate also carries (public_url host, tls_names, hostname) would let that runner stand
  in for the server to clients trusting kiln's CA; kiln's precedent (capacity holders) refuses only the reserved
  kiln-server name, and a stricter rule would refuse the server's own machine listed at its address. Ada/main to decide.
- RED confirmed: Studio r178 at 8220ea03d, `cargo +stable test --locked -p kiln --lib workspace::slates_fleet`:
  10 passed, 1 failed = the W10 test ("a client that mounted bench-a's export at 10.0.0.1 can't verify the certificate
  it serves with, which names [slates.kiln, r0.d6706139991797673574.slates.kiln]").
- FIX cba8d89ad; merged main 3b0f9ae91 (993fc2371; main had rewritten K2's manifest test, auto-merged); GREEN Studio
  r230 at 993fc2371: 11 passed, 0 failed.
- Main moved to 70e3dcaed (Slates re-vendored, contract 42; g293's W9 landed): docs/agentic.md conflicted (W9 vs W10
  sections, status row W); resolved by editing: W8, W9, W10 in order, row keeps main's sentences + W10's. No code
  overlap (grep: no new callers of the changed functions). Merge 449a9288e; diff --stat vs origin/main = my 4 files.
- **LANDED: kiln main 70e3dcaed..449a9288e** (8220ea03d red test + design, cba8d89ad fix, 993fc2371 + 449a9288e merges).
- Studio: clone (684M) + target (24G) removed; logs ~/.cache/g294-k8s/{red,green}.log THERE.
- Filed in run-requests.md: kiln's gate (clippy 3 platforms) for the change; a real fleet join after it (runners re-ask
  for a node certificate naming their listed host).
- Told g267 (inbox): its K-series node certificates now carry the listed host; admit requires it.

## Left (W10's build order; main starts the next piece)
1. Slates' export verb in slates-api (A-521): Instance::export(volume, ExportOptions { intent, uid, subtree }) ->
   attachment + capability path, typed refusals; contract after g293's 42 (so 43 if g293's lands first); kiln's
   workspace::slates::export calling it. Its by-use test needs a daemon (filed as main's run then).
2. crate::k8s: pure builders for the PersistentVolume (nfs server/path, mountOptions nfsvers=4.2,xprtsec=mtls,port,
   nosuid,nodev, claimRef), the claim (volumeName, storageClassName ""), the pod's volume/mount + runAsUser/runAsGroup;
   unit-tested like job_pod; chart RBAC (PVC create/delete in the jobs namespace, a ClusterRole for PVs).
3. The engine (isolation kind "pod"), the workspace's `home` runner and the server's routing of review/drift/landing.
4. The end and its reaper (pod, claim, PV deleted; attachment detached; volume ended by W3's rules).
5. kiln's issue of a k8s node's client certificate for tlshd (operator's call, recorded W27-R01).
6. The KIND proof through kiln (main's run, Linux with CONFIG_TLS).
- Decision for Ada/main: whether a listed host that the kiln server's own certificate also carries should be refused
  (today only kiln-server is, as for capacity holders).

## Restart 2026-11-19 (g294 restarted after a usage limit): piece 2 = step 1, certificate roles (main's decision)
- Readback of THE RULE sent to main. Kiln main = 449a9288e (my piece 1); clones clean.
- Step 1 finding (kiln pki.rs read in full): roles are distinguished for three kinds by marks only their issuer sets:
  the server's leaf by the SAN `kiln-server` (pki.rs 96-100 `reserved`, refused at 106, 444, 465-466, 517), a front
  door by serverAuth alone + O "kiln front door" (513-542, since be7bce8e8 2026-10-17), a machine by clientAuth alone
  (102-110, since ba0ecd654 2026-10-17). A holder's and a Slates node's are both serverAuth+clientAuth.
  Verified: the QUIC link only (net.rs 172-174 AgentBook::server_name = kiln-server). NOT verified on HTTPS: every
  `Roots::WebPkiPlusDer` client (agent.rs 198/2044, upgrade.rs 100 = the runner's next binary, capacity.rs 235,
  slates_fleet.rs 277/400/529, runner/guest.rs 641, frontdoor/proxy.rs 93, peers/forward.rs 263) checks the URL host
  alone, so a node certificate (piece 1 names the listed host) or a holder's (named as its machine, often the server's
  hostname, which server_leaf names) for a host the server is reached at stands in for the server.
- Plan: red test in http/tls.rs (node for kiln.home and 10.0.0.1, holder for kiln.home refused; server's own leaf and a
  front door accepted, same client); fix = pki::answers_as_server + http::tls verifier for WebPkiPlusDer (kiln's CA
  alone first; a certificate it issued for the name must answer as the server; else the full roots decide).
- Siblings to report, not fix: the link's server side takes a node/holder cert as a runner named by its first DNS SAN
  (machine certs from 10-13..10-17 carry both EKUs, so "clientAuth alone" there would cut off old runners);
  deploy/link.rs (linked server, operator CA, Roots::Pem); third-party clients (browsers, tlshd) check names only.
- RED baced921b (Studio d1323898, `cargo +stable test --locked -p kiln --lib w10_`, build 8m23s): 21 passed, 1 failed =
  http::tls w10 test, "a Slates fleet node's certificate listed at kiln.home, presented where the server is expected: None"
  (the server's own leaf and a front door had passed the same client first).
- FIX e44819198: pki::answers_as_server (names kiln-server, or front door = serverAuth alone + O `kiln front door`,
  FRONT_DOOR_ORGANIZATION const used by sign_front_door); http::tls KilnServer verifier for Roots::WebPkiPlusDer (server's
  CA alone first; a cert it issued for the dialled name must answer as the server, else AnotherRole refusal naming the
  cert's names; anything else -> full roots); ClientTls.kiln; Roots doc; docs/agentic.md W10 + row W; architecture.md.
- GREEN e44819198 (Studio, `cargo +stable test --locked -p kiln --lib -- w10_ http::tls:: pki::`): 40 passed, 0 failed.
- Merge 815a790ce over main da7a00ee6: docs/agentic.md row W conflicted (main: W1 Windows drive, W8 KVM/WHP) -> main's
  row + my clause at its end; main's commits touch no Roots/ClientTls/pki line; diff --stat = my 5 files.
- **LANDED: kiln main da7a00ee6..815a790ce** (baced921b red, e44819198 fix, 815a790ce merge). Branch synced to 815a790ce.
- Studio: clone (613M) + target (17G) removed; red.log, green.log kept in ~/.cache/g294-role THERE (60K).
- Filed in run-requests.md: main's gate (3 platforms, --since da7a00ee6) and a live check of each server-facing role
  (own leaf, front door, peer) plus the hostile node certificate.

## Left after piece 2
1. NEXT (step 2): Slates' library export verb in slates-api (A-521): Instance::export(volume, options) -> attachment +
   capability path, typed refusals; take the next library contract number after main's (main is past 48: W9's A-520
   third piece is contract 48); kiln's workspace::slates::export calling it.
2-6 as before (crate::k8s builders, the engine, the reaper, tlshd node client certificate, KIND proof through kiln).
- Siblings reported to main, not fixed: (a) the QUIC link's server side takes a node's or holder's certificate (clientAuth
  among its usages) as a runner named by its first DNS SAN (net.rs ServerBook::identify); "clientAuth alone" there would
  cut off runners enrolled 10-13..10-17 (both usages before ba0ecd654), so it needs a reissue plan; (b) deploy/link.rs's
  linked kiln server is checked under an operator-given CA by name alone (Roots::Pem; the CA may be a corporate one);
  (c) browsers, curl and tlshd check names alone: the operator's hosts are the only guard there; (d) a front door's IP
  names come from addresses its runner reports (frontdoor/control.rs names_to_sign), by design the server's entrance.

## Piece 3 (step 2): slates A-521, the library's export verb (main's brief 2026-11-19: contract 50; siblings (a)+(b)
## queued after it as their own piece; (c) already said in kiln W10 by 815a790ce; (d) fixed on slates main)
- Policy read: slates CLAUDE.md at main f8ba01f in full (§8 = THE RULE). Role: implementer subagent of main.
- Clone: worktree ~/slates-work/g294-slates-a521 (branch agents/g294-a521, from slates main f8ba01f; object store in
  ~/slates-work/g294-slates-read, shallow depth 50).
- Design: Instance::export(volume, uid, &ExportOptions { read_only, subtree }) -> Exported { attachment, path }
  (status for the name, attach_bound_mount (A-115), mount::export_path); no token = ProtocolFault::UnexpectedReply;
  Exported's Debug hides the token. Seam verb + SimVolumes::export (Held::Export record, token = attachment id).
  No wire change: the network export's address is the node's plan (deploy.rs: probe bind over TCP, Unix + roots);
  owed: a report field for it, and `slates export` printing the loopback port (wrong for a PV) over this verb.
- RED commit bda17c0 pushed (sim tests call sim.export: red by compile). Studio red run NOT yet run: this MacBook's
  kiln link is down since ~02:15Z ("not connected to the kiln server"; `kiln status` works, `kiln runners` exits 1).
- FIX written, uncommitted: export.rs (new), seam.rs, sim.rs, lib.rs (CONTRACT 50 + history), tests/api.rs (suite's
  stranger + expect_an_export_is_one_clients_and_ends_at_its_detach), docs: §4.6 status, §4.12 log, A-521 entry, GAPS
  (library bullet; runners-as-members line now says kiln's k8s engine uses the export), library.md.
- Kiln link outage 02:15Z-02:26Z (flapping until ~02:27Z); told main. RED seen at bda17c0 (Studio: 7 compile errors,
  no crate::export, no SimVolumes::export). FIX 3ad777c; GREEN at 3ad777c: `cargo test --locked -p slates-api --lib --
  sim:: export:: contract_history` 6 passed 0 failed; fmt 41fae0c (2 files, as `cargo fmt -p slates-api` left them).
- Merges: 89ff7c0 (main's A-551, A-573, A-565 first at the amendment log's end) and ff4c98f (main moved to bfb2c82:
  A-572 first); main's commits touched no crates/api or docs/library.md; diff --stat = my 8 files.
- **LANDED: slates main bfb2c82..ff4c98f** (bda17c0, 3ad777c, 41fae0c, 89ff7c0, ff4c98f). amendments.tsv A-521 row
  updated. Studio: clone (164M) + target (3.7G) removed; red/green/fmt logs in ~/.cache/g294-a521 THERE (24K).
- Filed in run-requests.md: clippy/xtask/doc/doctests for slates-api; the conformance suite over a real daemon.

## Left after piece 3 (main starts each)
1. QUEUED by main: the QUIC link's runner check (a runner's role required on the link; dual-usage machine certs
   reissued over the authenticated link at the runner's next connect, acceptance ending per runner once its reissue is
   recorded) + deploy/link.rs's linked-server role check, as one kiln piece.
2. A-521 owed: the daemon reporting its network export's address (a report field: wire change, publish_appended_variants);
   `slates export` over Instance::export (it prints the loopback port beside the path, wrong for a PV).
3. kiln W10 build order 2-6: crate::k8s builders (PV/claim/pod, RBAC), the engine (isolation kind "pod") calling
   Instance::export, the reaper (detach), the tlshd node client certificate, the KIND proof (main's).

## Piece 4: kiln's link takes a runner only in a runner's role + deploy/link.rs (main's brief 2026-11-19)
- Worktree ~/slates-work/g294-kiln-k8s-engine-kiln-p1 (kiln main 815a790ce, unmoved); branch pushed as agents/g294-link-role.
- Facts: machine certs dual-usage until ba0ecd654 (2026-10-17); identities record began 1c3f1f0b2 (2026-10-26); holders
  (11-12), keepers (11-13), slates-nodes (11-18) all recorded; so dual+unrecorded = a pre-10-17 runner's; recorded
  dual kinds refused by key. Endpoint::set_tls exists (hyper-transport) -> runner takes up a reissue without restart.
- Design: identify: revoked key (incl. holder/keeper/node keys) -> revoked name -> revoked serial -> link_role
  (Runner ok / BothSides = legacy, flagged dual_usage / Neither refused). Hub: Connected{dual_usage} -> role_owed;
  Features with `runner-role` -> reissue_runner_role (sign for the SPKI, record "machine") -> ServerMsg::RunnerRole(86);
  runner: take_runner_role check, write agent.pem (own_cert_path), net.renew_leaf (Out::Leaf, slot kept across rebuilds),
  AgentMsg::RunnerRole(74){serial}; hub: runner_role_said -> runner_role_taken (revoke legacy serial, by "kiln") ->
  Out::RevokeSerials. deploy/link.rs: roots(ca) -> Roots::KilnServerPem (operator CA alone, KilnServer rule).
- RED cf3e5c5a3 (Studio r368: 18 compile errors, all new API). FIX 3a89feb58 pushed; GREEN run r400 in flight.

## PARKED 2026-11-19 (main's brief: UI goal 92 takes over): piece 4 (UI goal 89, kiln link role)
- Worktree ~/slates-work/g294-kiln-k8s-engine-kiln-p1 is CLEAN (nothing uncommitted); local branch
  agents/g294-kiln-k8s-engine == pushed branch agents/g294-link-role at b152f8b8a.
- Commits on it: cf3e5c5a3 (red), 3a89feb58 (fix), b152f8b8a (wire tables: the two appended RunnerRole variants
  counted/listed; they failed in Studio run r400 at 3a89feb58: 413 passed, 2 failed).
- NOT verified green at b152f8b8a (no run after it); NOT merged: kiln main is 9f1a41e55 and lacks it.
- To resume: Studio run `cargo +stable test --locked -p kiln --lib` at b152f8b8a, then fetch/merge kiln main, diff --stat,
  push HEAD:main.

# g294 on UI goal 92 (51cef9f7): A-594, the SSH server's transport (restart 2026-11-19)
- Policy read: slates CLAUDE.md at origin/main e85c17f (§8 binding); role: implementer subagent of main.
  Readback of THE RULE sent to main.
- Design read: §4.17.2 "Transports" item 2 and piece 9; §4.17.3/§4.17.4 SSH lines (host sshd + CLI verbs; untouched).
- Sources read (each in ~/.cache/g294-ssh-reading, sha256/16): RFC 4250 eb3a9d7c3e1fae53, 4251 23a5052796d29edc,
  4253 b471173305154f6c, 4344 be7136101f416191, 5647 a0ee7571f0fa4a2b, 5656 0cc84636afaf4860, 7748 279ca0ecc5e92e29,
  8032 ed63657ff3893012, 8308 c7d36121bcd0f242, 8709 ea4aa3a8feec6bd5, 8731 26dcf1e8481d83e0; OpenSSH PROTOCOL
  91b7534f158c66ae (fa4b2a82); draft-ietf-sshm-strict-kex-03 cda5d60229f73469, draft-ietf-sshm-chacha20-poly1305-04
  a8aa91696db885d2 (Appendix A = a worked packet: golden vector), draft-miller-sshm-aes-gcm-01 3ea287492f70cbec.
- Worktree ~/slates-work/g294-ssh (branch agents/g294-a594-ssh from slates main e85c17f; object store
  ~/slates-work/g294-slates-read). Crate choice: a NEW crate crates/ssh (`slates-ssh`), not a module, since git, hg and
  svn+ssh share it and it needs only aws-lc-rs.
- RED c5a312d (tests + manifest + lock entry, modules absent): Studio `cargo test --locked -p slates-ssh --lib` -> 23
  compile errors, each an unresolved crate::<module>; the hand-written Cargo.lock entry passed --locked.
- CHANGE 832e1c8 (implementation; rustfmt'd locally with `rustup run 1.98.1 rustfmt --edition 2024`, no build);
  first green build: 2 E0716 in tests/kexinit.rs only -> fix 33a9588 -> GREEN 68 of 68 (Studio, debug).
- Red-checks (one Studio run, scratch clone): strict receive reset removed 12/26 transport tests fail; KEXINIT-first
  check removed 1 fails; GCM counter frozen 8 fail; explicit all-zero check removed SURVIVES (AWS-LC refuses first).
- DOCS 6b4b61d: design §4.17.2 status + tunables row "SSH key time"; amendment A-594 at the log's end; GAPS §4.17 entry;
  research/vcs-servers.md §5.4 (drafts discharged) + §5.5 (sources pinned by hash).
- Merges 0a9e6964 (3 append conflicts: GAPS, §4.17.2 statuses, log end; main's first, mine after) and 9098fdc7 (log
  end again, A-599); diff --stat = my 26 files. **LANDED: slates main ac693838..9098fdc7.**
- Follow-ups landed: 600cb4a literal markers (xtask R3 rule emulated: 35 flagged -> 0; RFC 4250 reasons as named
  consts), Studio 68/68 + fmt --check clean; **main 6ff5d4c4..4bf4877f**. 18abebe4 unsafe-budget row `slates-ssh = 0`
  (xtask unsafe fails an unbudgeted crate); **main 4bf4877f..18abebe4**.
- Studio: clone + target removed both times; logs red/green/mut/green2/fmt in ~/.cache/g294-a594 THERE (36K).
- amendments.tsv A-594 row: landed. run-requests.md: clippy/xtask/doc/cross-lint, Linux+Windows lib tests, live
  OpenSSH interop + a golden OpenSSH transcript with known server keys, the PQ-kex note.

## Left after A-594 (main starts each)
1. SSH user authentication (RFC 4252): publickey with ssh-ed25519 (RSA SHA-2/ECDSA after RFC 8332/5656 reading),
   EXT_INFO server-sig-algs right after the first NEWKEYS when the client lists ext-info-c (Established carries
   client_ext_info), principals of §4.13, disconnect once methods are exhausted.
2. The session channel (RFC 4254): one session, `exec` of exactly `git-upload-pack '<path>'`/`git-receive-pack '<path>'`,
   `env` GIT_PROTOCOL alone; no shell, subsystem or forwarding; windows/max packet from the shard budget.
3. The listener on slates-rt + the per-shard RFC 6298 estimator of authenticated connection lifetimes feeding
   ServerTransport::set_key_time_ns (keylife::key_time_limit_ns), the host key's keeping, the reaper.
4. Decision for Ada/main: a post-quantum hybrid (mlkem768x25519-sha256) — OpenSSH 10.1 warning (from memory).
5. PARKED goal 89 piece 4 (kiln link role): see the PARKED section above.

## UI goal 93: A-633 (SSH userauth + session channel) LANDED, then A-634 (mlkem768x25519-sha256)
- Merged main first (main's lint pass had escaped my crate's doc tags); read lint-habits.md.
- A-633: worktree ~/slates-work/g294-ssh, branch agents/g294-a633-ssh. RED 3663c5af (Studio: 11 compile errors, the
  missing layers). CHANGE 825387a5; fixes 6c6b6980 (test helper sent SERVICE_REQUEST inside the strict first kex) and
  c088a300 (a closed transport hid the client's DISCONNECT event); GREEN 105/105 at c088a300 + fmt --check clean.
  Red five ways (ed25519 check skipped, window not charged, sends inside own rekey, partial ignored, any git escape).
  Docs 6db8a482 (§4.17.2 status + SSH item, §4.17.3/§4.17.4 SSH lines, A-633 entry, GAPS, research §5.6).
  **LANDED: slates main 0727f830..4b5530a7.** Fixture: crates/ssh/fixtures/userauth (OpenSSH 10.2p1 RSA 3072 test key).
- Studio: ~/.cache/g294-a633 clone (323M) + target (324M) removed after A-634; red/green/mut/final/fmt logs kept THERE (80K).
- A-634 (mlkem768x25519-sha256): worktree ~/slates-work/g294-ssh (branch pushed as agents/g294-a634-ssh). RED bdb2fb7d
  (Studio: 100 passed, the 10 new/changed tests failed; the server offered curve25519 alone, the hybrid half a stub).
  CHANGE 07650154: GREEN 110/110 on the first build. a064d221: the hybrid unit test runs 64 encapsulations and requires
  a hash with its top bit set (the mpint mutation had failed the unit test only by chance). Red four ways: K as mpint
  (23 fail), secrets swapped (35), length check removed (2), reply swapped (35). Docs 455893fe: §4.17.2 SSH item +
  status, A-634 entry, GAPS, research §5.8 (and A-633's notes renumbered §5.7, moved into §5). Merges 1287e000
  (110/110 --locked, fmt --check clean) + ef3fd18a (log-end conflicts twice, main's entries first; changed nothing
  slates-ssh builds from). **LANDED: slates main f820c1b3..ef3fd18a.**
- A-634 decisions: FIPS 203 §7.2's modulus check is AWS-LC's (mlk_kem_enc_derand -> mlk_kem_check_pk, vendored
  aws-lc-sys 0.45.0 kem.c:331), held by a hostile test, not written twice; the encapsulation's randomness is AWS-LC's
  RAND_bytes (aws-lc-rs has no seeded encapsulation; EVP_PKEY_encapsulate_deterministic is experimental), so the
  determinism test pins curve25519; Algorithms::kex added; sntrup761x25519-sha512 not offered (no NTRU Prime in AWS-LC).
- OpenSSH claim checked (release notes pinned by hash, research §5.8): 10.0 (2025-04-09) made mlkem768x25519-sha256
  the default; 10.1 (2025-10-06) ssh(1) warns on every non-PQ key agreement, WarnWeakCrypto on by default; 10.2 nothing;
  pq.html: mlkem768x25519-sha256 or sntrup761x25519-sha512 silence it.
- amendments.tsv A-634 row: landed. run-requests.md: A-633 and A-634 entries appended (clippy/xtask/doc/cross-lint,
  Linux+Windows lib tests, live OpenSSH 10.2 interop, golden transcript with a fixed ML-KEM key).
- Also in A-634: ~12 narrowing `as` casts in A-594/A-633 test code made try_from (cast_possible_truncation is denied
  in tests too); `x % 2 == 0` -> is_multiple_of in tests/support.rs.

## UI goal 93: A-651 (the SSH edge in the daemon) -- code + docs on branch, NOT yet on main
- Worktree ~/slates-work/g294-a651b, branch agents/g294-a651-edge from origin/main 6ebb5073 (the earlier worktree
  ~/slates-work/g294-a651, branch agents/g294-a651-ssh-edge, has no commits and is abandoned: g299's rule landed first).
- Commits: red 510a759c (+ fixture fix 8bcb6a6c), green 0df9950f, test 55468644 (Start's principal read), docs 45777774.
- Design (main's rulings): waits as g299's HTTP edge (first RTO of TcpStream::round_trip before a phase learns; overdue
  marks; the most overdue reaped only for a new connection with every seat taken; generation back-off); SSH phases
  Handshake (to the client's NEWKEYS: ServerTransport::is_established, new) / Authentication / Setup / Running silences /
  Closing; keys: NoKeyRecord until g299's credentials record lands (no key file, no default key); dispatch in each tool's
  words and exit status (git 128, hg 255, svnserve 1) by repository, policy, then Services (all four NotYet: upload-pack
  and hg Stdio wait for A-662's store map, receive-pack A-654, svn A-656); Command trait shaped like hg's Stdio (input held,
  step -> consumed + next, output/sent), window given back on consumption.
- Evidence (Studio, debug): red 510a759c ssh 110/111; 8bcb6a6c server vcs_ 74/92 (18 new failing); green 0df9950f ssh
  111/111 + server vcs_ 92/92; 55468644 92/92; red-check never-reap: both reap tests failed. Four red-checks owed (the
  Studio ran out of memory mid-run r1210, stopped).
- To finish when the Studio answers (CARGO_BUILD_JOBS=4, one cargo at a time, delete targets after reading):
  1. ~/.cache/g294-a651: check `git status` in slates/ (the stopped mutation script restores in finally; else
     `git show HEAD:<path> > <path>`), delete target/, rerun the four mutations (script in ~/.cache/g294-a651/mutate.py
     there, or ~/slates-work/g294-scratch/a651-mutate.py here), delete target/.
  2. fetch + merge origin/main, `git diff --stat origin/main HEAD` (my files), green on the merged tree (server vcs_ + ssh
     lib + `cargo fmt -p slates-server -p slates-ssh -- --check`), delete target/, then `git push origin HEAD:main`.
  3. amendments.tsv A-651 row -> landed; update the A-651 entry's red-check line; delete the Studio clone.
- run-requests.md: A-651 entry filed (lints, Linux/Windows, the 4 red-checks, the live OpenSSH git clone command;
  two NOT-mine dead-code warnings: merge/src/engine/copy.rs source_of, server/src/repository/push.rs RepositoryLogs).
- Update (Studio offline since ~23:20 local): main's load rule now: CARGO_BUILD_JOBS=4, one cargo at a time, delete
  each red/green target after reading. Edge branch merged with origin/main 018d32ba at e1afa981 (docs conflict at the
  log's end only, main's first) and pushed. g299 landed A-625 part 3 (StoreLender) -> part 2 written:
  ~/slates-work/g294-a651c, branch agents/g294-a651-part2: red 8cf81d67 (stubs: UploadCommand exits at once,
  ToolServices starts nothing), green 15cc6b6a (UploadCommand over StoreLender: advertise first, whole_len requests,
  answers a pkt-line's room a step, flush -> exit 0, refusal -> 128; ToolServices: upload-pack over held stores only;
  vcs_http's `upload` module made pub(crate)). NOT yet built: run red then green on the Studio after part 1 lands.
- Part 3 owed (after g299's credential record is on main, main tells): slates-ssh session defers a publickey request
  whose key the holders cannot say yet (event asking the caller; resumed when answered), the edge resolving it with
  credentials::find(ssh_key_id(blob)).await (live_at(now), Unanswered -> refused typed), replacing NoKeyRecord.
- Order when the Studio answers: (1) part 1: clone ~/.cache/g294-a651/slates: check git status (restore mutations),
  delete target, ff to e1afa981, run the 4 mutations + `cargo test -p slates-server --lib vcs_` + `-p slates-ssh --lib`
  + fmt --check, delete target; fetch/merge main, diff --stat, push HEAD:main. (2) part 2: merge main into it, ff the
  clone to its red, run, delete target; green, run, delete target; land.
- Part 3 written (no build): ~/slates-work/g294-a651d, branch agents/g294-a651-part3 (on part 2): red e6a64f20, green
  a23ec511. slates-ssh: Holding::Unknown -> Answer::wanted (request unanswered, uncounted) -> SessionEvent::KeyWanted,
  reading held until Session::key_resolved(holding) re-asks the stored request (Unknown again = Nobody;
  SessionUse::NoKeyWanted misuse). Edge: SshSite::lookup (async), on KeyWanted -> lookup -> key_resolved; ToolSite::lookup
  = Nobody counted (NoKeyRecord never asks). Tests: userauth (wanted, uncounted), session (defer, order, misuse), edge e2e
  (LooksUp site). Production use of g299's CredentialRecord (credentials::find(ssh_key_id(blob)), live_at, Unanswered ->
  refused typed) waits for main's word that g299's record is on main.
- Docs owed for parts 2 and 3 (their own amendment entries + GAPS lines), written once their runs give numbers.
- Landing order when the Studio answers: part 1 (red-checks + merged green, land) -> part 2 (merge main, red, green, land)
  -> part 3 (merge main, red, green, land). Each run: CARGO_BUILD_JOBS=4, one cargo, delete target after reading.

## Relaunch 2026-11-19 ~16:10Z (fresh g294; Studio offline, builds on jupiter, Windows x86_64)
- Policy read: origin/main CLAUDE.md sha256/16 0070255077dbba7d (327 lines, §8 binding); role: implementer subagent.
  Readback of THE RULE sent to main. Worktrees clean; no conflict markers. Main = 4644dcec3.
- Pushed the unpushed merges: agents/g294-a651-part2 15cc6b6a..aea60bf7, agents/g294-a651-part3 a23ec511..b194940d.
- jupiter state found: clone ~/.cache/g294-a651p1 (dc48785, clean), target ~/.cache/g294-target-a651p1 (partial),
  logs ~/.cache/g294-a651p1-logs: main's slates-ssh lib 110 passed 0 failed (main after 455893fe, the last crates/ssh
  change; build 4m 10s); the predecessor's green-server run (15:55:35Z) was cut mid-compile; patches in logs/mut/.
- Run r35 (16:19:10Z, jupiter, same clone + target): p1 slates-ssh lib; slates-server lib --test-threads=4 vs the 23
  baseline names; fmt per crate; the 4 mutations (vcs_ssh filter). Expected: cut-at-bound fails the two
  `..._is_not_cut_while_a_seat_is_free`; unproven-backs-off fails `a_passphrase_pause_is_reaped_...` and
  `reaps_armed_under_one_bound_back_it_off_once`; window-on-receipt fails the slow-push tests; no-policy fails
  `the_repositorys_policy_decides_who_reaches_it`.
- Part 2 fidelity fix (found reading git at e1ec09b2; sources in ~/slates-work/g294-scratch/git-src-e1ec09b2, serve.c
  sha256/16 834f019139daadf5, pkt-line.c 708420106f01ee9d): upload-pack over SSH exited 0 when the client's input ended
  inside a request; git's process_request is gentle on EOF only for a request's first packet, then get_packet_data dies
  128 "the remote end hung up unexpectedly". Red test 335dba43 (the_end_of_input_inside_a_request_dies_as_gits_reader_does),
  fix b22dd97c (UploadCommand::ended + hung_up; doc notes git's other words when a line before the end is refused).
  Pushed agents/g294-a651-part2 at b22dd97c. Not built yet: its red-check = git.rs from aea60bf7 over b22dd97c's tests.
- Part 3 read: coherent (deferred request + KeyWanted set together; Answered answers once, never Unknown again).
- Part 1 docs to update after r35: SLATES_DESIGN.md A-651 entry's red-check bullet (~line 47403) and Owed line; GAPS
  line ~265-267 ("four of five red-checks (the Studio ran out of memory)").

## Relaunch 2 2026-11-19 (fresh g294; builds on jupiter, Windows x86_64; main e3234e618)
- Policy read: origin/main CLAUDE.md sha256/16 0070255077dbba7d (327 lines, §8 binding); role: implementer subagent.
  No readback to main (brief); report only when a piece lands or I am blocked.
- Part 1: merged origin/main e3234e618 into agents/g294-a651-edge -> da480e91 (no conflict; main's 71f05f93 and
  e3234e61 touch crates/merge + GAPS only), pushed. diff --stat = part 1's 16 files only; no markers.
- slates-ssh unchanged on main since 455893fe -> main's jupiter baseline 110/110 (g294-a651p1-logs/main-ssh.log) stands.
- Run r88 (jupiter, clone ~/.cache/g294-a651p1 ff'd to da480e91, target renamed to ~/.cache/g294-target, logs
  ~/.cache/g294-a651p1-logs/run3): ssh lib, server lib --test-threads=4 vs 23 baseline names, fmt per crate, 4 mutations.
- 18:3xZ main (CLAUDE.md §8, overriding the brief): DROP part 1's four remaining red-checks; land part 1 on a green lib
  run + fmt; parts 2 and 3 are separate pieces main starts after the one before lands (so END after part 1 lands);
  part 2's EOF fix keeps its failing unit test first; no reruns/seed replays/integration; don't wait on a queued machine.
  Done: moved jupiter's ~/.cache/g294-a651p1-logs/mut -> mut-dropped so r88's mutation loop is a no-op.
- r88 so far: slates-ssh lib 111/111 at da480e91 (17:15:39Z, compiled fresh). Killed the predecessor's orphaned
  15:55:35Z run tree on jupiter (bash 331372 -> rustc 331088, all in g294-target-a651p1, verified gone) and deleted
  that 5.5G target (jupiter 32G -> 47G free).
- r88 result: jupiter's C: filled to 100% (13 MB) at ~18:40Z; slates-server lib test compile died in LLVM "IO failure
  on output stream: no space on device" (os error 112) after analysis: 3 warnings, all main's (live_ship.rs:1901
  ship_to_silent_holders; vcs_hooks.rs:1204 Lesson, :1223 teach). fmt-ssh clean; fmt-server unrecorded (log write
  failed). Mutations never ran (patches moved). Clone deleted; target ~/.cache/g294-target (4.4G) kept.
- Room returned (27G free); r187 (jupiter): fresh clone of da480e91 at the same path, tracked files dated 16:00Z (before
  r88's build of the same commit) so finished units stay fresh; slates-server lib --test-threads=4 vs 23 names + fmt.
- r187 lesson: dating only `git ls-files` was not enough; vendor/aws-lc-sys rebuilt (its build script watches a
  directory, and the fresh clone's directories were new), so everything over aws-lc rebuilds. Next time date the
  directories too (find <clone> -path <clone>/.git -prune -o -exec touch -c -d ... {} +), or keep the clone.

## Relaunch 3 2026-11-19 ~19:15Z (fresh g294; Mac Studio): A-651 part 1 LANDED main 2e443304..e8ce55a8
- Policy read: origin/main CLAUDE.md sha256/16 0070255077dbba7d (327 lines) at start; re-read after main changed it:
  4bf5cee1f4d4fb98 (329 lines; §8 now "focused unit and integration tests", `--lib <filter>` + `--test <target> <filter>`).
  Role: implementer subagent. Main's corrections this run: (1) Ada: focused unit AND integration tests; (2) main fixed
  its PackRefused::Stalled breakage at 71a43cd97, run the server tests on the merged tree that lands.
- No integration target reaches the edge: `vcs_ssh` is private (`mod vcs_ssh;`), no caller outside its tests; slates-ssh
  has no tests/; server tests/ mention SSH only for signed-work signatures (A-321). So the focused set = slates-ssh lib +
  slates-server `--lib vcs_ssh` (19 tests: 7 serve over loopback TCP, 6 site, 6 waits) + fmt.
- One commit: worktree ~/slates-work/g294-a651e (removed after landing), branch agents/g294-a651-edge-land from main
  cd84e3811, `git merge --squash agents/g294-a651-edge` -> 6141837d (the piece + docs evidence lines: landing run,
  red-checks now main's). Merges 229ebee6 (main b1926d201) and e8ce55a8 (main 2e4433040; conflicts: Cargo.toml and
  crates/server/Cargo.toml, both additions kept, slates-ssh beside slates-git then main's slates-hg/slates-vcs; the log's
  end, main's entries first). Run copy agents/g294-a651-edge-run (b07e0143, c6c8e52a, df04ca82; worktree g294-a651f
  removed) never lands: it carried the code for runs whose numbers went into the docs/merge messages.
- Runs (Studio d1323898, macOS arm64, debug, CARGO_BUILD_JOBS=2, target ~/.cache/g294-target, logs ~/.cache/g294-a651):
  r16 b07e0143 (cd84e3811+piece): slates-ssh lib 111/111 (compiled fresh), fmt clean; slates-server lib test did NOT
  build: main's own vcs_http/tests/git.rs:490,500 `PackRefused::Stalled`, removed by A-640 part 3's 18831413 (landed
  f607f424, 11-19 09:51) -> fixed by main at 71a43cd97 (main found it too). r45 c6c8e52a (71a43cd97+piece): vcs_ssh 19/19, fmt clean.
  r56 df04ca82 (b1926d201+piece, code == 229ebee6's): 19/19. r65 e8ce55a8 (the landed commit, 2e4433040+piece): 19/19,
  652 filtered out, 2.17 s, 0 warnings, --locked held, fmt clean.
- Studio cleanup: predecessor's mutated clone ~/.cache/g294-a651/slates + its target deleted (its mut log: never-reap
  17/19, both reap tests failed); ~/.cache/g294-a651-run + ~/.cache/g294-target deleted. Logs kept (132K).
  ~/.cache/g294-link-role (16G: kiln clone + target of the PARKED link-role piece) left for that piece's resume.
- Ledger: amendments.tsv A-651 row "part 1 landed e8ce55a8 ..."; run-requests.md A-651 entry: update appended (landed,
  merged-tree numbers, item 3's four red-checks are main's).
- LEFT (main starts each): part 2 (agents/g294-a651-part2 b22dd97c, worktree ~/slates-work/g294-a651c: UploadCommand
  over StoreLender + the EOF exit-128 fix, test 335dba43 red first then fix b22dd97c); part 3 (agents/g294-a651-part3
  b194940d, worktree g294-a651d: deferred key lookup, after g299's credentials record). Both branches descend from the
  old edge branch (da480e91), not from main's squash 6141837d: merging origin/main into them brings part 1's identical
  code cleanly, but the A-651 entry in SLATES_DESIGN.md and GAPS will conflict where main's landed text added the
  landing-run lines: take main's A-651 text, then add the part's own lines. Each part lands as one commit + merge, so
  squash it onto a fresh branch from origin/main (`git worktree add -b`, `git merge --squash`) as part 1 was.
- Removable when convenient (clean, pushed): worktrees ~/slates-work/g294-a651b (agents/g294-a651-edge) and
  ~/slates-work/g294-a651 (agents/g294-a651-ssh-edge, abandoned, no commits).

## A-651 part 2 (main started it ~19:50Z): git's upload-pack over SSH -- in flight (see the landing note below)
- Worktrees: ~/slates-work/g294-a651p2 (branch agents/g294-a651-part2-run: red 8dfd557f = main 77e5ffb46 + 8cf81d67 +
  15cc6b6a + 335dba43 + the test pack source's no_pack (PackRefused::Stalled gone, A-640 part 3); green caa2fc0f = +
  b22dd97c + the v0/v1 refusal; red also pushed as agents/g294-a651-part2-red); ~/slates-work/g294-a651p2l (branch
  agents/g294-a651-part2-land: ONE commit 9fbba556 over main af520142f = part 2 code + docs; merge 69a7615c of main
  3329211e0 = A-625 part 3 in; pushed).
- Changes beyond the branch as written: (1) v0/v1 sessions refused by the edge itself (one ERR packet naming v2, then
  128): main's A-640 part 3 made slates-git answer v0 statelessly, which over SSH would lose a client's wants after its
  first round (slates-git's upload/v0.rs "Stateless only"; stateful v0 owed); the v0 test now covers v1. (2) StoreLender
  imported through vcs_http::git's existing re-export; vcs_http/git.rs untouched (A-625 part 3 rewrote it). (3) doc
  lines: A-662 landed the store map, but the StoreLender over it is on A-662's owed list, so no lender holds a store.
- Runs (Studio, macOS arm64, debug): r111 red 8dfd557f 23/25 (the exit-128 test: Ok(Exited(0)) vs git's
  Err(hung up, 128); the old v0 test: given slates-git's v0 advertisement), green caa2fc0f 25/25, 0 warnings, fmt clean.
  r125: the final gate on 69a7615c (the tree that lands).
- Main's message for after part 2 (19:5xZ): part 3 = the credential record (g299, main cceaa51b2):
  CredentialRecord { id: [u8;32], principal, kind: CredentialKind::SshKey { algorithm, blob } | HttpToken,
  expires_ns: Option<u64> }, record.live_at(now_ns); slates_server::credentials::ssh_key_id(blob) = SHA-256 of the
  blob; credentials::find(id).await -> Result<Option<CredentialRecord>, Unanswered> (NoShard | Silent), any shard;
  accept only Some(record) with kind SshKey of the same blob bytes and live_at(now); principal = record.principal;
  replaces NoKeyRecord. Two findings, each its own piece after part 3: (a) the SSH site calls Repositories::find, which
  answers only its own shard's partition: a repository another shard holds needs locate() and a move, as the HTTP edge
  does (g299 gave the test stub locate -> Here); (b) vcs_ssh::tests::a_held_key_runs_its_command_and_its_connection_
  teaches_the_edge failed once under load (Closing samples 0, expected 1); main's unconfirmed guess: StopsShard stops the
  runtime before the server reads the client's close. Report with its log if seen; never rerun-hunt.

## A-651 part 2 LANDED main 7d020df4..b04c552b (2026-11-19 ~20:27Z)
- One commit 9fbba556 (7 files, +713/-7: crates/server/src/vcs_ssh.rs, vcs_ssh/git.rs, vcs_ssh/site.rs,
  vcs_ssh/tests/git.rs; SLATES_DESIGN §4.17.2 status + entry "A-651 part 2" at the log's end; GAPS line after A-651's;
  research/vcs-servers.md §5.10 row + bullet for git's serve.c/pkt-line.c at e1ec09b2) + merges 69a7615c (main
  3329211e0), 4ae81a39 (545181b30), 3f9ec62a (e50757a1a), b49f131d (1ef718820), b04c552b (7d020df47). Main moved during
  every run; the last merge cited g303's accepted practice (main's commits changed nothing the filtered tests call).
- Runs (Studio, macOS arm64, debug, --lib vcs_ssh): red 8dfd557f 23/25, green caa2fc0f 25/25 (r111); 69a7615c r125,
  4ae81a39 r142, 3f9ec62a r151, b49f131d r171: 25/25 each, 0 warnings, fmt clean. Main's flaky test (finding b) passed
  in all six; nothing to report. Logs in ~/.cache/g294-a651 on the Studio (192K); clones and targets deleted.
- Ledger: amendments.tsv A-651 row "part 2 landed b04c552b ..."; run-requests.md: new "A-651 part 2" entry (clippy,
  xtask, doc, cross-lint; Linux/Windows `--lib vcs_ssh`; three red-checks; live v2 clone and the v0 refusal's words).
- Worktrees removed: g294-a651p2, g294-a651p2l, g294-a651c (branches pushed: agents/g294-a651-part2-run caa2fc0f,
  agents/g294-a651-part2-red 8dfd557f, agents/g294-a651-part2-land b04c552b, agents/g294-a651-part2 b22dd97c).

## Next: A-651 part 3 (main starts it; the credential record is on main, cceaa51b2)
- Branch agents/g294-a651-part3 (b194940d, worktree ~/slates-work/g294-a651d) sits on the OLD part 2 branch, so take
  only part 3's own commits (red e6a64f20, green a23ec511; check with `git log --no-merges b22dd97c..b194940d`) onto a
  fresh branch from origin/main (`git worktree add -b`, `git cherry-pick -n`), never part 2's old code (main's is the
  landed one: re-export import, no_pack, v0/v1 refusal).
- Then replace NoKeyRecord with the credential record per main's recipe (in "Main's message for after part 2" above):
  id = credentials::ssh_key_id(blob); credentials::find(id).await; accept only Some(record) with kind SshKey of the
  same blob bytes and record.live_at(now); principal = record.principal; Unanswered (NoShard | Silent) -> a typed
  refusal. Red first: a test that an enrolled live key proves its principal and an expired or other-blob one is refused.
- After part 3, findings (a) the site's Repositories::find vs locate() + a move, and (b) the flaky held-key test, are
  their own pieces (main's call).

## A-651 part 3 (main started it ~20:3xZ; resumed after the usage-limit restart): in flight
- Worktree ~/slates-work/g294-a651p3, branch agents/g294-a651-part3-run over main 1c6ac5006: red 6a6b0c84 (e6a64f20 +
  the record's failing tests and stubs; also pushed as agents/g294-a651-part3-red), green 9af6531f (a23ec511 + the record:
  RecordKeys answers Unknown; key_holder (SshKey of the same blob, live_at); recorded_holder (find(ssh_key_id(blob)),
  wall clock, consumer_revoked for a consumer's key); ToolSite::lookup -> Holds{principal, KEYS_REQUIRED=1} or Nobody
  counted by KeyUnproven (vcs_ssh.key_unknown / key_not_this_key / key_expired / key_revoked / key_unanswered);
  credentials::consumer_revoked = daemon_edge's revoked moved beside find, daemon_edge calls it). NoKeyRecord gone.
- Beyond main's recipe: the consumer check (the HTTP edge's rule, §4.13 "every later effect refuses"); an unanswered
  lookup answered as nobody, counted apart (key_unanswered).
- Tests: tests/site.rs: the_production_key_holders_leave_every_key_to_the_sites_lookup,
  a_record_proves_its_principal_for_its_own_key_while_live, the_production_lookup_proves_a_key_by_the_credential_record_
  from_any_shard (two-shard audit daemon; enrolls live, expired, token-at-key-id, unenrolled-consumer keys; asks 5 keys).
- Run r7 (Studio): red then green, slates-ssh `--lib -- userauth session`, slates-server `--lib -- vcs_ssh credentials
  daemon_edge --test-threads=4`, fmt. Logs ~/.cache/g294-a651/p3-*.log there. No integration target reaches the edge.
- Then: landing worktree from origin/main, squash the run branch, docs (§4.17.2 status, "A-651 part 3" entry, GAPS),
  one commit, merge main, gate run on the merged head, push.

## Relaunch 4 2026-11-19 ~21:20Z (fresh g294; Mac Studio): A-651 part 3 landing
- Policy read: origin/main CLAUDE.md sha256/16 4bf5cee1f4d4fb98 (329 lines) at start, then 275ec430156edff5 (332, main
  added the run_until_idle gotcha); lint-habits read. Role: implementer subagent.
- Found: run branch agents/g294-a651-part3-run at 7ebd7639 (pushed; red 6a6b0c84, green 9af6531f, pair fix 7ebd7639).
  Studio logs (~/.cache/g294-a651): red ssh 32/34, red server 34/38 (new tests failing); green ssh 33/34 at 9af6531f
  -> 7ebd7639 ssh 34/34; green server 37/38 at 9af6531f: a_held_key_runs_its_command_and_its_connection_teaches_the_edge
  left (1,1,1,0,0) right (1,1,1,1,0) = main's finding (b), Closing sample 0. Junk ~/slates-work/g294-a651p3l (interrupted
  worktree add, no metadata, no edits) removed and re-added on agents/g294-a651-part3-land (= main 3612d1ef).
- Landing worktree ~/slates-work/g294-a651p3l: `git merge --squash agents/g294-a651-part3-run` staged (10 files) + docs
  (§4.17.2 part 3 status; part 1 + A-625 part 3 status lines; GAPS part 1 + A-625 p3 owed lists, part 3 line with
  @@GATE@@ placeholder; amendment entry "A-651 part 3" at the log's end with @@GATE@@). RFC 4252 fetched to
  ~/slates-work/g294-scratch/rfc4252-src (sha256/16 a36b882cf77b1073): §5.1 server processes each request before the next.
- Gate copy ~/slates-work/g294-a651p3g, branch agents/g294-a651-part3-gate: 45ecc528 (squash over 3612d1ef), r86: ssh
  34/34, fmt clean, server 37/38 = (b) again. Merge 0d8ad1b4 (main 0577ab0b, A-654 part 2 touches vcs_policy/vcs_http):
  r112 in flight (logs p3-gate2-*.log).
- (b) mechanism confirmed in code: rt shard.rs step() drains controls before polling ready tasks; Control::Shutdown
  cancel_all; StopsShard sends Shutdown right after the client writes its close and drops its socket, so a shard woken
  late cancels the edge's task before it reads the close. Fix (b's piece): the client waits for the server's end
  (`peer.ended()`) before returning. Part 3 leaves the test's path unchanged but adds concurrent load in the filter.
- Main's call (~21:45Z): split (b) from part 3, land the harness fix first, then part 3 on its own green, then (a)
  (SSH site cross-shard serving: Repositories::find -> locate() + a move of the connection, as the HTTP edge does;
  failing test first).
- **(b) LANDED: main 34d10acb..9d54e0f9** (4a74c118 fix + docs/bugs/2026-11-19-the-ssh-edge-tests-stopped-the-shard-
  before-the-edge-read-the-close.md; merge 9d54e0f9, main's A-636 part 2 in slates-db only). Run copy
  agents/g294-ssh-close-run a0c665d3 (main 2410011d + fix): r122 `--lib -- vcs_ssh --test-threads=4` 25/25, fmt clean.
  Worktrees ~/slates-work/g294-close-run, g294-close-land (removable). Studio clone ~/.cache/g294-close-run (delete).
- Part 3: gate copy merged main 9d54e0f9 -> 92303c5c (pushed); r154 in flight (logs p3-gate3-*.log). p3l docs updated
  (entry Tests bullet names the fix; GAPS line over main 9d54e0f9); two @@GATE@@ placeholders left for r154's count.
- Part 3 green on main 9d54e0f9: gate 92303c5c r165 (after r154 compiled slates-server against a STALE slates-ssh rlib:
  the close-run clone shared ~/.cache/g294-target and rebuilt slates-ssh with main's code, newer than the gate clone's
  untouched crates/ssh sources -- lint-habits' shared-target trap; fixed by touching crates/ssh): ssh 34/34, server
  38/38, fmt clean. Landing commit 9cc9a378 (p3l, 12 files, one commit). Main moved to 225d7eea (A-625 p3 cont. (2)
  daemon_edge.rs listener + daemon.rs; A-658 p2): p3l merge staged (log-end conflict resolved: main's 4 entries, then
  A-651 part 3), NOT committed yet; gate merge bfc61a3f (code identical to p3l's staged merge) pushed; r175 in flight
  (p3-gate5-*.log). Next: commit p3l's merge citing r175, diff --stat, markers, push HEAD:main.

## Relaunch 5 2026-11-19 ~22:15Z (fresh g294; Mac Studio): A-651 part 3 LANDED main b94ae6e9..17f4f7d6
- Policy read: origin/main CLAUDE.md sha256/16 275ec430156edff5 (332 lines, §8 binding); relaunch brief (incl. Ada's
  ~22:45Z "completeness first": findings by reading go to main in one line, never chased); lint-habits. Role: implementer.
- Found: p3l mid-merge of main 6a9d2f9a (log-end conflict), landing branch never pushed (pushed 458242b6 at once);
  gate copy c925db8f pushed, no run on it. Resolved (main's A-678 entry first; exact union checked by numstat) ->
  4edc73b7 (intermediate merge, run deferred to the next tree); main moved to b94ae6e9 (A-653 cont. (4): vcs_ssh/git.rs
  keeps its admission) -> gate db322a29 (pushed), kiln r251 (r245 lost: Studio link dropped before it started):
  ssh 34/34, server `vcs_ssh credentials daemon_edge` 40/40, no warning, fmt clean -> p3l merge 17f4f7d6 (log end again,
  main's A-653 (4) entry first) citing r251; diff --stat = part 3's 12 files; pushed HEAD:main.
- **LANDED: main b94ae6e9..17f4f7d6** (9cc9a378 + merges 301b50e5, 458242b6, 4edc73b7, 17f4f7d6). amendments.tsv A-651
  row updated; run-requests.md entry "A-651 part 3" appended. Branch agents/g294-a651-part3-land = 17f4f7d6.
- Next (my brief, queue item 2): (a) the SSH site serves across shards: Repositories::find via locate() and a move of
  the connection to its repository's owner shard, as the HTTP edge does; failing test first.

## A-651 part 4 (main's finding (a)): the SSH site across shards — LANDED main 693bc8fe..2f73eb07 (2026-11-19 ~23:00Z)
- One commit 2f73eb07 (7 files, +822/-70; fast-forward, main had not moved): vcs_ssh.rs (an exec naming a repository
  another shard holds moves its connection there whole: Exit/Exec/Moving/migrate/arrived/serve_arrival/seated/
  seat_arrival/Arriving; keep_here/here/EdgeState::arrivals (no host key off the listener's shard); seat waiters a
  Vec<Waker> + arriving bound; counts moved/arrived/arrivals_refused; refusals vcs_ssh.move_refused/arrival_lost/
  arrival_refused; serve's maker Fn + Clone + Send + 'static, S::Keys: Send), site.rs (SshSite::locate, ToolSite's via
  Repositories::locate; module doc), vcs_http.rs (ShardRepositories re-exported), tests (site: an_exec_is_located_on_
  the_shard_its_repository_lives_on; serve: an_exec_moves_its_connection_to_the_shard_that_holds_its_repository on a
  two-shard audit daemon over the production ToolSite<RecordKeys, ShardRepositories, StartsEvery>), docs (§4.17.2
  status, A-651 part 4 log entry, GAPS line after part 3's whose owed tail it closes).
- Runs (Studio, macOS arm64, debug, `--lib -- vcs_ssh --test-threads=4`): red aa16b067 r298 28/30 (both new failing);
  green ae8071d7 r324 30/30; gate 679b5677 (= landed tree) r335 30/30, 0 warnings, fmt clean. Studio trick that saved
  the full rebuild: fresh clone with each file identical to the previous clone's dated by `touch -r` (7109 of 7138), so
  only changed crates rebuilt (server build ~2.5 min).
- Ledger: amendments.tsv A-651 row "part 4 landed 2f73eb07 ..."; run-requests.md "A-651 part 4" entry (clippy/xtask/doc,
  Linux+Windows, 3 red-checks, the untested arrival-wait/full paths). g299's inbox (g299-http-edge.md): A-684 must keep
  an SSH edge state on every shard (keep_here with EdgeState::new on the listener's, ::arrivals on others) and pass a
  Clone + Send site maker.
- Cleanup: Studio clones g294-a651p4, g294-a651p3-gate, g294-close-run and target g294-target (14G) deleted; logs kept
  in ~/.cache/g294-a651 (440K). Local worktrees p3, p3g, p3l, close-run, close-land, p4, p4l removed (all pushed or on
  main). Branches: agents/g294-a651-part4{,-red,-land}.
- END of g294's queue (brief: part 3, then (a)). Left for main: HTTP edge's module doc says "no connection outlives its
  listener" though moved connections are tasks of other shards (finding by reading, reported); arrivals' wait/full paths
  untested in both edges.

## A-647 part 4 (main's audit: g302's unlanded draft) — LANDED main 23b566fd..20ae700a (2026-11-20 ~00:20Z)
- Source: g302's agents/g302-a647p4-red (bbd4bad12, never run) and agents/g302-a647p4-wip (1067842cb, never compiled);
  notes ~/.local/state/slates-fanout/progress/g302-derived-data.md. main had parts 1-3 only (checked).
- Red c44d98d8 (agents/g294-a647p4-red): bbd4bad12's stubs (Shape, value_bytes moved after bases_found) + g302's final
  storage test; kiln r383: `--lib -- t_a647` 6/7, the storage test failing (15 versions past bound, merge 7,740 B).
- Green 64494654: 1067842cb's reach.rs/mod.rs/tests whole + its repository.rs/vcs_store patches (vcs_store conflict:
  main's `paths`/`heads` fields and made()'s path map kept, GitStore::new(format, window)). Built at once: r402 git
  208/208; r404 census 466/938 B, 378,007 total, 0 past; server 46; fmt clean; r413 CLI repository test 1/1.
- Landing: one commit fbf4a24d over main 5206419b + ten merges (main moved ten times in ~40 min); gates r428, r444,
  r465, r476, r487, r500, r514 (final: 4f82e130 = landed code minus main's A-691 hg-only change): git 218, server 110
  (`vcs_ssh` whole, A-684's daemon tests among them), stock git interop over the HTTP edge 2/2, fmt clean, no warning.
  GAPS line 6 conflicted once (main's A-658 3a): resolved = main's line + part 4's clause (scripts/resolve_gaps_a645.py;
  my first hand resolution dropped the following `>` line, caught by numstat 1/2, fixed before commit).
- Ledger: amendments.tsv A-647 row (parts 3 and 4); run-requests.md "A-647 part 4" (clippy/xtask/doc, Linux+Windows,
  red-checks, node-size measurement). Studio: clone + target (19G) deleted; logs ~/.cache/g294-a647 (708K).
- Finding (by reading, reported): `read_tree`'s root-level check has no hostile test (a valid tree one level too tall).
- The `ShardRepositories` unused-import warning from A-651 part 4 is gone: A-684 (vcs_ssh/daemon_edge.rs) uses it.
