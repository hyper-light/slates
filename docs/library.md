# The embedding library, `slates-api`

A product that embeds slates — kiln first — vendors it and drives it through one Rust crate,
`slates-api` (`crates/api`), never through the `slates` command and never by parsing its output.
The [unified design](wip/SLATES_DESIGN.md) §4.12 "Embedding" and its amendment A-278 describe it;
the [gap ledger](wip/GAPS.md) row "Embedding library" tracks what is still owed.

The crate is the typed client of the daemon plus what was the CLI's alone: the host mount, the
grant issuer and the grant's default term. The `slates` command calls the same code, so a host
mounts and grants exactly as the command does.

## What is built (2026-11-18)

- `Instance::attach(name)`: a client of a running instance. With no daemon it is refused
  `Error::Unavailable` at once.
- `Instance::ensure_group()`: makes the consensus group of an instance the host runs alone (a
  laptop), or finds it made. Never call it on a fleet node: a fleet node imports its groups.
- Volumes: `create` (with `slates_api::overlay(name, folder, max_bytes)` for a volume laid over a
  host folder, or `slates_api::scratch(name, bytes)`), `snapshot`, `clone_snapshot`, `destroy`,
  `list`, `status`, `drift`, `rewitness`.
- Serving: `mount(volume, path, &MountOptions)` and `unmount(path)` (macOS: the loopback NFS mount;
  Linux: FUSE; Windows: a WinFsp drive), `serve_shards(volume, intent, subtree)` (a stream end to
  hand `shards run --slates-fs-fd N`), `serve_vhost_user(..)` (Linux), `detach(attachment)`.
- Landing: `plan_landing(&LandingRequest)` presents a landing and returns its plan (the manifest a
  person approves, its summary, its conflicts); `land_approved(&issuer, &request, &manifest, ..)`
  grants and lands only that manifest, or returns the new plan when the volume or the disk changed
  since; `land(&request, grant)` lands under a grant already issued. `landing_entries(&plan)` lists
  what the plan writes, path by path (each file's BLAKE3 and length, a rename's origin, a symlink's
  target, the conflict found at it): a reviewer with no mount of the volume builds the change from
  it, checking each file it reads at the head against its entry, and presents the landing again on
  a mismatch.
- The issuer: `Issuer::from_environment()` (the anchor's handoff, as `slates grant` has it) or
  `Issuer::from_handoff(pairs)`. Only a confirmation surface a person drives should hold one. The
  SDKs and the MCP server cannot issue grants: they do not depend on this crate.
- Observation: `daemon_status`, `audit`, `grants`, `telemetry`.
- `Instance::client()` lends the whole typed client for every other verb (merges, file verbs).

Every failure is one variant of `slates_api::Error`: the daemon's typed `Refusal`, `Unavailable`,
the channel's own error, a `MountFault` carrying the OS's error number, an `IssuerFault`, or a
`PathFault`. Calls are synchronous: a provisioning verb answers in microseconds, and a landing or a
mount waits while the daemon lives. A host on its own async runtime runs them on its blocking pool
until the async form lands.

## A host's workspace flow

```rust
use std::path::Path;
use slates_api::{Approved, Filter, GrantScope, Instance, Issuer, LandingRequest, MountOptions, Planned};

fn workspace(folder: &Path, at: &str) -> Result<(), slates_api::Error> {
  let mut slates = Instance::attach("kiln")?;
  let volume = slates.create(&slates_api::overlay("agent-1", folder, 16 << 30)?)?;
  let mounted = slates.mount(volume, at, &MountOptions::default())?;
  // ... the agent works in `mounted.path` ...
  let request = LandingRequest {
    volume,
    snapshot: None,
    target: folder.to_string_lossy().into_owned(),
    filter: Filter { include: vec![], exclude: vec!["/.git".to_owned()] },
  };
  if let Planned::Plan(plan) = slates.plan_landing(&request)? {
    // Show `plan.summary`, `plan.conflicts` and `plan.manifest_text()` to a person. When they approve:
    let issuer = Issuer::from_environment()?;
    match slates.land_approved(&issuer, &request, &plan.manifest, (GrantScope::Once, slates_api::GRANT_TERM_NS))? {
      Approved::Landed(outcome) => println!("landed: {}", outcome.state),
      Approved::Changed(again) => println!("the plan changed; show {} instead", again.manifest_text()),
    }
  }
  slates.unmount(&mounted.path)?;
  slates.destroy(volume)
}
```

## What is owed

1. Starting an instance from the host's own executable, so a host runs no `slates` binary, with
   the issuer handed to the host that started it (on Linux too).
2. An async form over the host's own reactor.
3. A seam trait with a simulator and a conformance suite.
