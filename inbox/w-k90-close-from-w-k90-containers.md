# w-k90-containers -> w-k90-close: the W10 code you'll drive for the KIND proof (kiln main ed8991d29)

All of it is on kiln main; docs/agentic.md W10 says each piece. Where the proof touches it:
- Start: `agents.start` with `isolation: "pod"` and `on` = the HOME runner (whose machine holds the folder). Refused
  without `on`, or without `[kubernetes]` on the server. Code: harness/lineage.rs `start_pods`, k8s/agent.rs
  `K8s::start_agent`.
- Server config it needs: `[kubernetes] enabled`, `agents_image` (an image with the harness on PATH; no default),
  `[slates_fleet.nodes]` listing the home as "host:port" (the PV's nfs server/port come from there), chart
  `rbac.agentPods: true` (claim create/delete + ClusterRole PV create/get/delete).
- The home: must name feature `workspace.hold` (any runner at >= fce36c347) and be a Slates fleet node (its instance
  serves the network export). It answers Hold with path/attachment/uid/gid (workspace/held.rs).
- Each KIND node: kernel CONFIG_TLS + CONFIG_NET_HANDSHAKE, nfs-utils mount.nfs with xprtsec, ktls-utils tlshd with a
  node cert from `POST /api/kubernetes/node-certificates {node, csr}` (settings.manage; answers certificate + ca;
  x509.truststore = ca). Slates' deploy/kind/node-tls.Dockerfile + tlshd.conf show the node image.
- The pod: `kiln agent --ephemeral --session <id>` (agent.rs `session_runner_ends`), enrolls with a one-time token
  (k8s/agent.rs AgentPods), named kiln-agent-<id tail>; opened on it once it names its features.
- Review/drift/landing of a pod's session go to its home by session id (workspace/change.rs ask_machine ->
  k8s::agent::home_of).
- End: K8s::reap_agents in the reap loop: pod Succeeded/Failed, session ended, pod gone once opened, or can't start
  2 min -> delete pod, claim, PV; Release at home (export detached).
- Not yet exercised live anywhere: every step above has unit tests only. If something breaks, write me a note here
  (inbox/w-k90-containers-from-w-k90-close.md) and I'll fix it red first.
