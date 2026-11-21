# w-k90-close -> main: goal 90c closed (2026-11-20 ~21:30 PST)

- Kiln commit: b928a782a96 on kiln main (docs 2c1224efdca: docs/agentic.md W10 status: Slates volumes attach to
  Kubernetes pods, as cycle 10 proved; kiln's Kubernetes engine rework is a separate task). Earlier landings this goal:
  83a03cd1ac4 (dae14868d30, agents.start takes isolation "pod"), d582cee16fc (2b4b69b5564, a pod session's
  land/drift/change routed to its home; k8s_agent_live).
- The run: cycle 10, k8s_agent_live at bec9fdf5f93, adacachy, minikube profile k90-close (2 nodes, kicbase + tlshd from
  ktls-utils 0.11, `modprobe tls`; server+home in a container on the cluster network, ufw drops bridge->host).
  PROVEN: fleet node; node tlshd certs issued by kiln; agents.start isolation pod; the pod's folder = kubelet nfs4 mount of
  the home's export with xprtsec=mtls (vers=4.2,nosuid,nodev); the agent's files in the volume (plan: create 1,
  replace 1); review branch holds the pod's file; drift `/README.md (Modified)`; landing over it refused (ModifyModify).
  Cycles 11/12 also landed through the person's grant (written 2, 89 bytes).
- Cleanup: cluster deleted, units stopped, Mac scratch and the branch deleted; adacachy ~/.cache/w-k90-close and images
  k90-close-agent:live, k90-close-kicbase:{tls,base} pending kiln's reconnect (in my progress file if I can't).
- GAPS (not chased): kiln lib rebuilds on every commit (build.rs embeds the commit), ~45-100 min a cycle on adacachy.
