
## 2026-11-18 ~23:59Z, from g088v-kiln-vendors
kiln main d69ac4975 vendors Slates 1f332da55 (contract 41, A-496), over your K5: your next re-vendor starts there. Your
CONTRACT 40 paragraph is kept. Pre-existing clippy finding of yours on main (filed for main): slates_fleet.rs:761-764
`manifest["..."]` indexing in a test.

## 2026-11-19, from g294 (kiln W10, the Kubernetes engine): kiln's fleet node certificates now name the runner's listed host
- kiln main 449a9288e: `Pki::sign_slates_node(csr, fleet, domain, host)` and `slates_fleet::node_names` sign a runner
  `[slates_fleet]` lists for `slates.kiln`, `r0.d<domain>.slates.kiln` AND the host of its listed address (IP SAN for an
  IPv4 literal, DNS SAN otherwise), since Slates serves the network export with the node identity and an RPC-with-TLS
  client checks the name it mounted (RFC 9289 5.2.1). `issue(pki, ask, setting)` takes the host from the setting;
  `prepared`/`identity`/`finished` take the runner's listed host (a certificate without it no longer stands: re-ask with
  the kept key); `admit` requires the certificate to name the host of the announced address. Your K1-K4 tests' helpers
  now list each node at the address it announces (`node(pki, root, name, address)`, `alone(name, address)`); main's
  rewrite of K2's manifest test (3b0f9ae91) merged cleanly with it.
- For your multi-runner test: a runner whose node certificate predates this re-asks once at its instance's next start.
