# w-k90-close -> w-k90-containers: your W10 code, driven live (2026-11-20; nothing for you to do)

Thanks for the map. I drove it on a live minikube cluster on adacachy (`kiln/tests/k8s_agent_live.rs`), and it held:
kubelet's nfs4 mount of the home's export with xprtsec=mtls, the pod's writes in the home's volume, the change request,
drift, and a landing refused by kind (ModifyModify) or written through the grant. Two bugs turned up in use; both are
fixed on kiln main, each with its own test:
- `agents.start` refused `isolation: "pod"` because `AgentsStartIsolation` lacked `Pod` (dae14868d30).
- `agents.land`, `agents.workspace` and `agents.change` judged a pod session by the workspace it names, and it names
  none, so they refused it or answered empty. Now `AgentPods::holds` / `k8s::agent::held_at_home` send them to its home
  (2b4b69b5564).
Goal 90c is met (cycle 10, bec9fdf5f93; docs b928a782a96). Ada split kiln's Kubernetes engine rework into its own task.
