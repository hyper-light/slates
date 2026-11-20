# w-k90-close progress (kiln goal 90: KIND proof of W10 + A-520 piece 5; Adas-Mac-Studio-4; started 2026-11-20 ~07:15 PST)

POLICY_READ: briefs/common.md + briefs/w-k90-close.md + relaunch-2026-11-19.md. Kiln worktree ~/.cache/k90-close/wt
(detached, from origin/main ed8991d2930).

## State found (2026-11-20 ~07:30 PST)
- Both parts' CODE is on kiln main (w-k90-containers): W10 k8s::volume/agent, kubeapi PV/claim, rbac.agentPods, node
  tlshd cert, workspace::held, agents.start isolation "pod" + reaper (75757b07d); A-520 5a/5b-i/5b-ii (4eda3a697),
  W9 sign-in (82e81809f). What's missing for both: PROOF BY USE.
- adacachy has NO KIND: no `kind` binary; the kindnetd processes are minikube's 3-node cluster "kiln" (kicbase
  v0.0.50, k8s v1.35.1, ns kiln-jobs) = kiln's live k8s pool. NOT to be touched.
- adacachy: kernel 7.2.9 CONFIG_TLS=m (module not loaded), passwordless sudo, minikube present, no tlshd on host.
- A-520 on adacachy: rootful docker only -> container engine refused NeedsSharedMount (owed piece per docs/agentic.md W9).

## Plan
1. Own minikube profile `k90-close` (docker driver), base image = kicbase + ktls-utils (docker build; nothing on host),
   `sudo modprobe tls` (Ada authorized for slates' KIND export leg 2026-10-01). Told main.
2. kiln linux build on adacachy (own clone ~/.cache/w-k90-close-clone, target ~/.cache/w-k90-close-target.noindex).
3. Live: kiln server + home runner (fleet node, export) + agents_image (fake-claude + act) -> agents.start pod ->
   kubelet NFS mount -> write -> review/drift/land through grant.

## Done so far (2026-11-20 ~08:05 PST)
- Told main (agents_send) the KIND finding + plan.
- adacachy: `sudo modprobe tls` done; node image k90-close-kicbase:tls built (kicbase v0.0.50 + tlshd from
  oracle/ktls-utils 0.11 source + openssl; ~/.cache/w-k90-close/node-image/Dockerfile).
- kiln branch agents/w-k90-close (394670a97, pushed over SSH from this Mac): crates/kiln/tests/k8s_agent_live.rs
  (KILN_K8S_LIVE=1 gate), fixtures/k8s-agent/{Dockerfile,claude,act}, common::Tables{listen,kubernetes}.
- r19: clone ~/.cache/w-k90-close/kiln2 (branch) + `cargo test --no-run` (target ~/.cache/w-k90-close/target).
  r20: `minikube start -p k90-close` 2 nodes, KUBECONFIG=~/.cache/w-k90-close/kubeconfig (never ~/.kube/config).
- NEXT: agent image k90-close-agent:live (stripped debug kiln) -> `minikube -p k90-close image load`; ns
  kiln-agents-live; run the test; red -> fix.

## For main to delete
- adacachy ~/.cache/w-k90-close/kiln (a partial clone stuck on HTTPS creds; 84K)
