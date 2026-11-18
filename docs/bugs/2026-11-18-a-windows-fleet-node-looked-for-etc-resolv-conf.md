# A Windows fleet node looked for /etc/resolv.conf and could not join a fleet that trusts a root (2026-11-18, §4.8 "Deployment", A-490)

## Description

Kiln's runners join a slates fleet through a manifest kiln writes for each runner, and kiln always names its own CA in
it as the fleet's enrollment root (kiln goal 95101ef3, K2). Before it starts the runner's instance, kiln has
`slates_api::role::check_fleet` read the manifest exactly as the start will (contract 39). On Windows that read was
refused, so a Windows runner could never join: `fleet manifest <path>: reading /etc/resolv.conf: ...` (the file does
not exist there). g267 found it while building K3 and reported it to main; it was bug 5 of UI goal 85.

## Root cause

`plan_from` (`crates/api/src/role/fleet.rs`) asks for the host's resolver when a node of the manifest is named or an
enrollment root is trusted. Asking on a root is right: a peer enrolled under a root announces its addresses as strings
the node parses as an IPv4 literal or a DNS name (`crates/server/src/discovery.rs`, `peer_of`), so a node that trusts
a root may have to resolve a name. But the one reader, `read_resolver`, knew only `/etc/resolv.conf`, which Windows
does not have. So every Windows node whose manifest trusted a root, which is every kiln runner's, was refused at its
start, even with every address a literal.

## Impact

No Windows runner could join a kiln fleet, and no Windows node could join any fleet with a trusted root or a named
peer. Nothing was lost or written: the refusal came before the instance started, by name.

## Fix (exact edits)

- `crates/api/src/role/fleet.rs`: `read_resolver` is a seam with two halves of one signature. `#[cfg(not(windows))]`
  reads `/etc/resolv.conf` as before. `#[cfg(windows)]` reads the DNS servers the network adapters list
  (`windows_dns::adapters`) and judges them by `resolver_of_adapters`, which is cfg-free and tested on every host. It
  applies the rule Go's resolver applies on Windows (`net/dnsconfig_windows.go`): the servers of every adapter that is
  up and has a gateway, in order, each once, with `resolv.conf(5)`'s default timeout and attempts, since Windows states
  none an application can read. With one nameserver those give up 10 s after the first query, as the Windows DNS
  client does (Microsoft's KB 2834226). `ManifestError::Resolver` names where it looked (a path on Unix, the adapters on
  Windows), and its sentence names the roots case too; `ManifestError::Adapters` (Windows) carries the platform's
  refusal. The resolv.conf parser and its caps are compiled where they are used (Unix, and every host's tests).
- `crates/api/src/role/fleet/windows_dns.rs` (new, Windows only): one `GetAdaptersAddresses` call, asked first for
  its size and then into a buffer of that size, and a walk over the lists it writes. Each walk is bounded by the
  entries its buffer can hold, and a link or an address is read only once it is checked to lie wholly inside the
  buffer, aligned. An adapter added between the two calls is a refusal by name, not a retry.
- `crates/api/Cargo.toml`: windows-sys' `Win32_NetworkManagement_IpHelper` and `Win32_NetworkManagement_Ndis`.
- `unsafe-budget.toml`: slates-api 5 → 9 (the call and the three reads of the walk).
- Docs: `docs/cli.md` and `docs/deploy.md` (each platform's nameservers), `crates/server/src/dns.rs` and
  `crates/server/src/deploy.rs` (doc comments only), design §4.8 "Deployment" and A-490, GAPS.

## Tests

- `a_windows_hosts_adapters_give_its_resolver` (`crates/api/src/role/fleet.rs`): a host with an adapter down, one
  with no gateway and two that count (one server listed by both) gives the three servers in order, each once, with
  the default timeout and attempts. A host none of whose adapters counts, and a host with no adapter, give no resolver.
- The Windows half runs only on Windows: its live check is filed for main's Windows lanes.

## Siblings

A sweep of every hard-coded host path in shipped code (`/etc`, `/proc`, `/sys`, `/dev`, `/run`, `/var`) found none
other on a path a Windows build runs: each other sits in Linux-only code (FUSE, AppArmor, `/proc/self/*`) or in tests.
