# The embedding library, `slates-api`

A product that embeds slates — kiln first — vendors it and drives it through one Rust crate,
`slates-api` (`crates/api`), never through the `slates` command and never by parsing its output.
The [unified design](wip/SLATES_DESIGN.md) §4.12 "Embedding" and "The consumer contract", with
their amendments A-278 and A-299, describe it; the [gap ledger](wip/GAPS.md) row "Embedding library"
tracks what is still owed.

The crate is the typed client of the daemon plus what was the CLI's alone: the anchor and daemon
roles, the host mount, the grant issuer and the grant's default term. The `slates` command calls
the same code, so a host starts, mounts and grants exactly as the command does.

## Starting an instance with no slates binary

A host serves the anchor and daemon roles from its own executable. Its `main` hands the arguments
after a prefix word of its choosing to `slates_api::role::main`, and `Instance::start` runs that
executable as the anchor; the anchor starts its daemon from the same executable and prefix.

```rust
use std::process::ExitCode;
use slates_api::{Instance, StartSpec};

fn main() -> ExitCode {
  let args: Vec<String> = std::env::args().skip(1).collect();
  if args.first().map(String::as_str) == Some("slates-role") {
    return slates_api::role::main(&args[..1], &args[1..]);
  }
  // ... the host's own commands; when it needs its instance:
  let spec = StartSpec::new("kiln", std::env::current_exe().unwrap(), &["slates-role"]);
  let started = Instance::start(&spec);
  // ...
  ExitCode::SUCCESS
}
```

- `Instance::start(&spec)` refuses `StartFault::AlreadyRunning` when the instance already answers
  (attach to it instead), spawns `HOST_EXE slates-role --instance NAME anchor` as a child, waits
  until the instance answers, and makes the consensus group of an instance run alone. If the anchor
  exits first, the start is refused `StartFault::AnchorExited` with its exit code; the anchor's log
  lines on standard error say why.
- `StartSpec::anchor_command()` is the same command for a host that launches the anchor its own way
  (`anchor_command_under(&wrapper)`, with `wrapper` the `OsStr` words of `systemd-run --user --scope
  --`, runs it under a wrapper that execs it, keeping the launcher's secret on its standard input),
  for example under its service manager so the instance outlives the host's process. Such a host
  waits with `Instance::wait_for(name, None, Some(deadline))`.
- `Started.anchor.stop()` stops the instance: a graceful stop on Unix (`SIGTERM`), the process ended
  on Windows. The instance's volumes live in its memory, so they go with it.
- Every process of the instance carries `--instance NAME` in its arguments, so a host or a test can
  find exactly its own instance's processes.
- The starter is the launcher: it sets its agents' write modes and approves what an ask mode holds.
  Its authority is a secret of its own: make one once with `LauncherSecret::generate()`, keep its
  `bytes()` where the host keeps its secrets, and start with
  `StartSpec::new(..).with_launcher(LauncherSecret::from_bytes(kept)?)` at every start; then
  `Issuer::from_launcher_secret(&secret)` is the authority `approve_landing`, `approve_merge` and
  `set_write_policy` take, after the host's restart as before it. (The daemon's verification of it is
  being built, A-285.)

## The consumer contract

Everything a consumer touches goes through this crate, and every answer is typed: an outcome enum
for what happened, a closed `slates_api::Error` for what failed. Nothing is a string to parse. The
design's §4.12 "The consumer contract" (A-299) is the law; this is the user's view.

### Write permission

A landing (writing a volume's changes to a folder) and a merge (folding a work volume into its
green) follow the permission the user gave the harness or agent asking to write:

| `WriteMode` | A landing answers | A merge answers |
|---|---|---|
| `Bypass` | `LandOutcome::Written(report)` | `MergeOutcome::Merged { version, .. }` |
| `Ask` | `LandOutcome::ApprovalRequired(landing_approval)` | `MergeOutcome::ApprovalRequired(merge_approval)` |
| `Forbid` | `LandOutcome::Refused(policy_refusal)` | `MergeOutcome::Refused(policy_refusal)` |

Conflicts are their own outcome in both (`Conflicted`), typed per path. The launcher sets a
`WritePolicy` per volume with `Instance::set_write_policy(PolicySubject::Volume(v), &policy, &issuer)`
and reads it with `write_policy` (`None` while unset: merges run at once, landings ask); an act
follows the stricter of
the volume's and the agent's own (`PolicySubject::Consumer`); an agent can never raise its own (the
daemon refuses the verb on an agent's channel). The launcher approves an ask with
`Instance::approve_landing(&approval, &issuer)`, which writes exactly the manifest shown or answers
the new presentation when the volume or the disk changed since, and
`Instance::approve_merge(&approval, &issuer)`, which merges exactly the increment shown or asks
again when the work changed. Both work from any `Instance`.

### Signed work

A merge may carry a signature by its author's key, and its outcome carries how the daemon judged
it: `Verification::{Verified { signer }, Unsigned, Invalid { reason }}`. Signatures never gate
permission; the write mode does.

1. The launcher trusts keys on a volume: `set_signers(volume, &[TrustedSigner::from_openssh(line)?],
   &issuer)` (an `ssh-ed25519` or FIDO `sk-ssh-ed25519@openssh.com` public key line); `signers(volume)`
   reads them back. A merge verifies under its work's and its green's signers.
2. The signer asks for the statement, `merge_statement(&work, &evidence)` (the work, its green, its
   base and the increment's identity, as plain text), signs it with
   `ssh-keygen -Y sign -n slates-work -f KEY FILE`, and merges with
   `merge(&work, &evidence, Some(&WorkSignature::from_armoured(&text)?))` before the work changes.
3. The verdict is recorded with the version: `merge_record(green, version)` reads it back.

A volume's policy may require signed merges (`WritePolicy::require_signed_merges`, on a green for
every merge into it, on a work for every merge from it): an unsigned or invalid merge is then
answered `MergeOutcome::Refused` with `PolicyReason::SignatureRequired`, at once in bypass and at its
approval in ask. A landing's signature is owed: a signed `land` is refused
`Error::Unsupported(Unsupported::SignedWork)` before anything is sent.

### Errors

The variant says what to do:

| Variant | What a consumer does |
|---|---|
| `NotFound(missing)` | treat the subject (`missing` names it) as gone |
| `AlreadyExists(existing)` | use what exists, or choose another name |
| `Denied(denied)` | ask whoever holds the right; retrying changes nothing |
| `Exhausted(bound)` | free something or wait for room |
| `Busy(holder)` | wait for the holder, or end it |
| `Stale(what)` | read the current state and ask again |
| `Retry { cause, pause_ns }` | ask again after `pause_ns`; `Instance::retry_pause_ns` paces later tries |
| `Unavailable { .. }` | start the instance, or report it down |
| `Unsupported(what)` | take another route |
| `Invalid(what)` | fix the request |
| `Unusable(why)` | the subject's state forbids it (destroying, not recovered, no group yet) |
| `Elsewhere(where)` | send it where it says |
| `Host(fault)`, `Mount(fault)`, `Start(fault)` | the OS refused; the error number is in the fault |
| `Protocol(fault)` | a fault in slates: report it |

No enum of the contract is `#[non_exhaustive]`. Pin the contract: `slates_api::CONTRACT` is raised
with every change to a type or a verb, so assert the one you wrote against, and match exhaustively
where you must decide; a moved contract then fails your build at the place to update.

### The seam and its simulator

A host whose rules reach other products only through a trait with a simulator (Hex's) holds
`slates_api::Volumes`: provision, snapshot, destroy a snapshot, destroy, list, serve to a Shards VMM,
mount, unmount and detach, each with the `Instance` verb's own signature. `Instance` implements it,
and so does `SimVolumes::new(most)`, an in-memory stand-in for tests with no daemon: it answers the
same outcomes and typed errors (names unique, clones pinning snapshots, works only over greens), holds
at most `most` volumes and `most` attachments, serves no bytes (a stream it hands out is a real end
nothing answers until detached) and mounts nothing (a mount is a record). The conformance suite in
`crates/api/tests/api.rs` (`the_seam_conforms`) runs over both.

## What is built (2026-11-18)

- `Instance::attach(name)`: a client of a running instance. With no daemon it is refused
  `Error::Unavailable` at once.
- `Instance::ensure_group()`: makes the consensus group of an instance the host runs alone (a
  laptop), or finds it made. Never call it on a fleet node: a fleet node imports its groups. Before
  it, provisioning is refused `Error::Unusable(Unusable::NoGroup)`.
- Volumes: `provision(&VolumeSpec)` for every source (`VolumeSpec::overlay(name, folder, max_bytes)`,
  `VolumeSpec::empty(name, bytes)`, `VolumeSpec::clone_of(name, volume, snapshot)`, a green,
  `VolumeSpec::work(name, green)`), `snapshot`, `destroy_snapshot`, `destroy`, `list`, `status`
  (typed, an overlay's base read or unreadable), `drift` (typed entries: `Modified`, `Deleted`,
  `Replaced`, `TypeChanged`), `rewitness`, `read`.
- Serving: `mount(volume, path, &MountOptions)` and `unmount(path)` (macOS: the loopback NFS mount;
  Linux: FUSE; Windows: a WinFsp drive; it answers `Unmounted`, whose `unreported` names each volume a
  Windows unmount could not ask for its mounts), `serve_shards(volume, intent, subtree)` (a stream end to
  hand `shards run --slates-fs-fd N`), `serve_vhost_user(..)` (Linux), `detach(attachment)`.
  `slates_api::mount::available()` says, with no daemon, whether this machine can mount a volume
  and through what (`MountKind`), or what is missing (`MountMissing`). `slates_api::mount::unmount_lazy(path)`
  lets go of a mount that a leftover process still uses (Linux, `fusermount3 -u -z`).
- Landing: `land(&request, signature)` under the caller's mode; `plan_landing(&request)` presents
  without writing, for a reviewer; `approval_entries(&approval)` lists what an approval writes, path
  by path (each file's BLAKE3 and length, a rename's origin, a symlink's target, the conflict found at
  it): a reviewer with no mount of the volume builds the change from it, checking each file it reads
  at the head against its entry, and presents the landing again on a mismatch; `approve_landing(&approval,
  &issuer)` writes exactly what was approved.
- Merging: `merge(&work, &evidence, signature)` and `rebase(&work)`, with typed conflict windows and
  the green's head; `approve_merge(&approval, &issuer)` for a merge an ask mode held;
  `merge_statement(&work, &evidence)`, `merge_record(green, version)`, `set_signers` and `signers`
  for signed merges. `Instance::client()` lends the raw client for the declared edits on a work volume (`edit`,
  `declare`) and the file verbs, outside the contract.
- The launcher's authority: `Issuer::from_environment()` (the anchor's handoff, as `slates grant`
  has it) or `Issuer::from_handoff(pairs)`. The SDKs and the MCP server cannot approve: they do not
  depend on this crate.
- Observation: `daemon_status`, `audit` (typed `AuditRecord`s), `grants` (typed `GrantRecord`s),
  `telemetry`.

Calls are synchronous: a provisioning verb answers in microseconds, and a landing or a mount waits
while the daemon lives. A host on its own async runtime runs them on its blocking pool until the
async form lands. `slates_api::VERSION` is the crate's version, the workspace's one version;
`slates_api::CONTRACT` is the contract's.

## A host's workspace flow

```rust
use slates_api::{Filter, Instance, Issuer, LandOutcome, LandingRequest, MountOptions, VolumeSpec};

fn workspace(folder: &std::path::Path, at: &str) -> Result<(), slates_api::Error> {
  let mut slates = Instance::attach("kiln")?;
  let volume = slates.provision(&VolumeSpec::overlay("agent-1", folder, 16 << 30))?.volume();
  let mounted = slates.mount(volume, at, &MountOptions::default())?;
  // ... the agent works in `mounted.path` ...
  let request = LandingRequest {
    volume,
    snapshot: None,
    target: folder.to_string_lossy().into_owned(),
    filter: Filter { include: vec![], exclude: vec!["/.git".to_owned()] },
  };
  match slates.land(&request, None)? {
    LandOutcome::Written(report) => println!("landed: {:?}, {} entries", report.state, report.written),
    LandOutcome::ApprovalRequired(approval) => {
      // Show `approval.summary` and `slates.approval_entries(&approval)?`; once approved:
      let issuer = Issuer::from_environment()?;
      match slates.approve_landing(&approval, &issuer)? {
        LandOutcome::Written(report) => println!("landed: {:?}", report.state),
        LandOutcome::ApprovalRequired(fresh) => println!("it changed; show {} instead", fresh.manifest_text()),
        LandOutcome::Conflicted(conflicts) => println!("{} conflicts", conflicts.len()),
        LandOutcome::Refused(refusal) => println!("{refusal}"),
      }
    }
    LandOutcome::Conflicted(conflicts) => {
      for conflict in &conflicts {
        println!("{}: {:?}", conflict.path, conflict.class);
      }
    }
    LandOutcome::Refused(refusal) => println!("{refusal}"),
  }
  slates.unmount(&mounted.path)?;
  slates.destroy(volume)
}
```

## What is owed

1. The starter handed the launcher's authority on every platform.
2. A landing's signature (signed merges are built).
3. Typed fields on the wire for drift, a presentation's conflicts and a merge's versions.
4. A paged volume listing.
5. An async form over the host's own reactor.
6. A typed class for a pinned snapshot's destroy, answered `Invalid::Request` today.
