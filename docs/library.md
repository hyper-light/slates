# The embedding library, `slates-api`

A product that embeds slates — kiln first — vendors it and drives it through one Rust crate,
`slates-api` (`crates/api`), never through the `slates` command and never by parsing its output.
The [unified design](wip/SLATES_DESIGN.md) §4.12 "Embedding" and "The consumer contract", with
their amendments A-278 and A-299, describe it; the [gap ledger](wip/GAPS.md) row "Embedding library"
tracks what is still owed.

The crate is the typed client of the daemon plus what was the CLI's alone: the anchor and daemon
roles, the host mount, the grant issuer and the grant's default term. The `slates` command calls
the same code, so a host starts, mounts and grants exactly as the command does. Every Rust example
here compiles against the crate as a doctest (`cargo test -p slates-api --doc`), so none can drift
from the API it shows.

## Starting an instance with no slates binary

A host serves the anchor and daemon roles from its own executable. Its `main` hands the arguments
after a prefix word of its choosing to `slates_api::role::main`, and `Instance::start` runs that
executable as the anchor; the anchor starts its daemon from the same executable and prefix.

```rust no_run
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
  waits with `Instance::wait_for(name, None, Some(deadline))`; one that holds the anchor's child
  waits with `Instance::wait_for(name, Some(&mut child), None)` and needs no deadline: the wait lasts
  while the anchor lives, and the anchor judges its starting daemon by its progress, never by a
  second (A-366).
- `StartSpec::until_served()` asks the anchor to stop when its daemons crash-loop before any of
  them has served: the instance never answered, so no client can be waiting on it, and the wait ends
  `StartFault::NeverServed` (the anchor exits `role::EXIT_NEVER_SERVED`, 5) instead of lasting while
  the anchor loops on them. For a host that has attached nothing yet, as kiln's own start of its
  machine's instance. Once a daemon has served, an anchor never gives its instance up (A-119), asked
  or not.
- `StartSpec.fleet = Some(role::FleetSelection { manifest, node })` starts the instance as a node of the
  fleet the manifest names (`--fleet PATH --node NAME`). A starter that writes the manifest itself checks
  it first with `role::check_fleet(&selection)` (contract 39): the manifest and the files it names read
  and the node's deployment planned exactly as its anchor and daemon do at their start, answering the
  fleet's name, `f`, the node's peers and roots (`role::FleetCheck`), or the refusal that start would end
  with (kiln writes each runner's manifest and checks it so; kiln goal 95101ef3).
- `Started.anchor.stop()` stops the instance: a graceful stop on Unix (`SIGTERM`), the process ended
  on Windows. The instance's volumes live in its memory, so they go with it.
- Every process of the instance carries `--instance NAME` in its arguments, so a host or a test can
  find exactly its own instance's processes.
- The starter is the launcher: it sets its agents' write modes and approves what an ask mode holds.
  Its authority is a secret of its own: make one once with `LauncherSecret::generate()`, keep its
  `bytes()` where the host keeps its secrets, and start with
  `StartSpec::new(..).with_launcher(LauncherSecret::from_bytes(kept)?)` at every start; then
  `Issuer::from_launcher_secret(&secret)` is the authority `approve_landing`, `approve_merge`,
  `set_write_policy` and `set_signers` take, after the host's restart and the daemon's as before them:
  the daemon verifies every launcher proof under that secret beside its own issuer's (A-285).

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

A merge or a landing may carry a signature by its author's key, and its outcome carries how the
daemon judged it: `Verification::{Verified { signer }, Unsigned, Invalid { reason }}`. Signatures
never gate permission; the write mode does.

1. The launcher trusts keys on a volume: `set_signers(volume, &[TrustedSigner::from_openssh(line)?],
   &issuer)` (an `ssh-ed25519` or FIDO `sk-ssh-ed25519@openssh.com` public key line); `signers(volume)`
   reads them back. A merge verifies under its work's and its green's signers.
2. The signer asks for the statement, `merge_statement(&work, &evidence)` (the work, its green, its
   base and the increment's identity, as plain text), signs it with
   `ssh-keygen -Y sign -n slates-work -f KEY FILE`, and merges with
   `merge(&work, &evidence, Some(&WorkSignature::from_armoured(&text)?))` before the work changes.
3. The verdict is recorded with the version: `merge_record(green, version)` reads it back.

A landing is signed the same way. Every presentation states the landing's statement (the volume,
the target and the manifest hash, as plain text): `plan_landing(&request)` answers it in
`LandingApproval::statement`, as does a `land` an ask mode holds. Sign it with the same command and
land with `land(&request, Some(&signature))`. The verdict comes back in the approval
(`LandingApproval::verification`) and in what was written (`LandingReport::verification`), and the
landing's planned and finished audit records keep it: `audit(since)` reads it back as
`AuditRecord::verification`. An approval carries the landing's signature on to
`approve_landing(&approval, &issuer)` (`LandingApproval::signature`, which an approver may also set).
Nothing requires a signed landing.

A volume's policy may require signed merges (`WritePolicy::require_signed_merges`, on a green for
every merge into it, on a work for every merge from it): an unsigned or invalid merge is then
answered `MergeOutcome::Refused` with `PolicyReason::SignatureRequired`, at once in bypass and at its
approval in ask.

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
mount, unmount, bind a mount into a container, and detach, each with the `Instance` verb's own
signature. `Instance` implements it, and so does `SimVolumes::new(most)`, an in-memory stand-in for
tests with no daemon: it answers the same outcomes and typed errors (names unique, clones pinning
snapshots, works only over greens), holds at most `most` volumes and `most` attachments, serves no
bytes (a stream it hands out is a real end nothing answers until detached), mounts nothing (a mount is
a record) and binds only the mounts it recorded, judged as the daemon judges the kernel's table (a
bind is a record too, ending with its mount). The conformance suite in `crates/api/tests/api.rs`
(`the_seam_conforms`) runs over both.

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
  hand `shards run --slates-fs-fd N`; its guest is offered DAX per file, each file of a 2 MiB range or
  more marked for its guest to map while the shard's fill budget has room), `serve_vhost_user(..)`
  (Linux), `detach(attachment)`. `bind_into_container(volume, source, destination, intent)` (A-520) binds
  a volume's mount into a container: the daemon checks in the kernel's mount table that `source` is
  exactly this volume's mount, never touching the mount, records the binding (it ends at its `detach`
  or with its mount), and answers a `ContainerBind`: the binding's attachment and the entry to hand the
  container runtime as it is (`OciBinding`: `type: bind`, the non-recursive `bind`, `ro` or `rw` by the
  intent, `private`, `nosuid`, `nodev`). Linux binds the FUSE mount (mount it `shared` for a rootful
  engine: the engine and its containers reach it as users other than the one who mounted it, and the
  binding's evidence says whether it was), macOS the loopback NFS mount, which Docker
  Desktop's file sharing carries into its VM; Windows offers none (`Unsupported::Attachment`,
  `HostPlatform`).
  Before it, `container_engine::engine_profile(runtime)` asks the container engine for its profile and
  judges it (`EngineProfile`: the endpoint, the profile and the test it holds, with how a container's
  ids reach the volume and how hard links behave through it; else `EngineFault`, `Untested` naming the
  engine it refused); just before the runtime binds, `ContainerBind::check_source()` (or
  `container::check_source(source, identity)`) checks the source is still the mount verified, by the
  kernel's table and the mount id it never reuses (`SourceFault`: `Missing`, `Replaced`, ...).
  `slates_api::mount::available()` says, with no daemon, whether this machine can mount a volume
  and through what (`MountKind`), or what is missing (`MountMissing`; on Windows that includes
  `MountManager`, the one-time setting that lets WinFsp's driver make the drive; on macOS and the BSDs,
  `PrivateLoopback` where someone besides root can capture loopback traffic, since a mount's capability crosses it in
  the clear). `slates_api::mount::unmount_busy(path)`
  lets go of a mount that a leftover process still uses, with no daemon and no privilege, and answers how
  (`LetGo`): `Detached` on Linux (`fusermount3 -u -z`: the holder reads on until it closes), `Forced` on macOS and
  the BSDs (`unmount(2)` with `MNT_FORCE`: the holder's next call fails; the loopback NFS mount is `hard`, so this is
  the way off it once its daemon has gone). `mount::unmount(path)` and `unmount_busy` run no program on macOS and the
  BSDs, so no `PATH` is needed (A-507), and both are judged by the kernel's mount table: a path with nothing mounted
  is `MountFault::NotMounted` before any call, a held mount's plain unmount is `MountFault::Busy` (on Linux too since
  A-537: a plain `fusermount3 -u` refused while the table still lists the mount), and an unmount the table still lists
  is `MountFault::StillMounted`. A `Busy` names its holders (`MountHolders`): on Linux the processes whose descriptors,
  working or root directories are on the mount, read from `/proc` without touching it, and how many could not be read;
  on macOS none are read (`HoldersUnread::NoReader`), since every libproc answer stats the file, which a `hard` NFS
  mount whose daemon has gone waits on without bound.
- Landing: `land(&request, signature)` under the caller's mode; `plan_landing(&request)` presents
  without writing, for a reviewer, and states what a signer signs; `approval_entries(&approval)` lists what an approval writes, path
  by path (each file's BLAKE3 and length, a rename's origin, a symlink's target, the conflict found at
  it): a reviewer with no mount of the volume builds the change from it, checking each file it reads
  at the head against its entry, and presents the landing again on a mismatch; `approve_landing(&approval,
  &issuer)` writes exactly what was approved.
- Merging: `merge(&work, &evidence, signature)` and `rebase(&work)`, with typed conflict windows and
  the green's head; `approve_merge(&approval, &issuer)` for a merge an ask mode held;
  `merge_statement(&work, &evidence)`, `merge_record(green, version)`, `set_signers` and `signers`
  for signed merges. `Instance::client()` lends the raw client for the declared edits on a work volume (`edit`,
  `declare`) and the file verbs, outside the contract.
- The launcher's authority: `Issuer::from_launcher_secret(&secret)` (the starter's own secret, above),
  `Issuer::from_environment()` (the anchor's handoff, as `slates grant` has it) or
  `Issuer::from_handoff(pairs)`. The SDKs and the MCP server cannot approve: they do not depend on
  this crate.
- Moving a volume to another fleet member (A-291): `member()` is this instance's member id, and
  `move_volume(volume, to)` asks the volume's owner to move it to the member `to` (another
  instance's `member()`): one ask, answered `MoveProgress::Moving` until the switch and
  `MoveProgress::Moved` from it, so ask again until `Moved`; detach first. The new member serves the
  volume once it has adopted it (its verbs answer `Retry` until then), and the old one lets its copy
  go once the new one has placed it. Refusals: `Busy(Attached)`, `Busy(Moving)`,
  `Unusable(Unmovable(NotPlain | Lineage | Snapshots))`, `Invalid(MoveTarget(NotMember |
  OutsideNeighbourhood))`, `Retry { cause: MoveUnplaced, .. }`. A lone instance has no other member.
  An overlay does not move (its folder stays on its machine); a clone of a captured base does.
- Capturing a folder once for many workspaces that move (A-291): `capture_base(overlay)` makes
  every byte and link of the overlay's folder the volume's own and answers a snapshot holding the
  whole tree; `VolumeSpec::clone_of(name, overlay, snapshot)` then gives each workspace an O(1)
  clone that reads no folder at all, so it moves like an empty volume's and outlives the folder.
  Capture only a folder nothing writes while it runs (a commit's tree checked out for it): a file
  that changes while it is read is refused `Error::Retry { cause: ChangedWhileRead, .. }`, and a
  capture the folder moved under `Error::Invalid(Invalid::LiveBase)`; a volume with no folder is
  refused `Error::Unsupported`. A large file is read whole, so the capture costs its bytes in memory
  once more while it runs.
- Observation: `daemon_status`, `audit` (typed `AuditRecord`s), `grants` (typed `GrantRecord`s),
  `telemetry`. The audit is one log, written through the control shard, so an instance on any shard
  reads every landing's records. A granted landing that is refused ends its trail: a
  `LandingFinished` record whose `outcome` is `Refused`, or `Aborted` once it wrote an entry.
- Lists a page at a time: the daemon answers every list a reply at a time, so a host that shows a
  list a screen at a time reads one `Page { items, next }` a screen and holds no more:
  `list_page(after)` (volumes in id order), `grants_page(after)`, `audit_page(since)`,
  `changed_since_page(green, version, from)` (what a green changed after a version, in every
  dimension), and `drift_page(volume, from)` (an overlay's drift, typed). Pass a page's `next` back
  to read the next one; `None` ends the list. An item that stands for the whole walk is read
  exactly once. The whole-list verbs `list`, `grants`, `audit`, `changed_since` and `drift` walk the
  pages for you. A status carries the first drifted entries that fit and counts the rest in
  `DriftReport::elided`; `drift` reads every entry.
- The chosen-path launcher (`slates_api::exec`, Linux, A-370): a volume shown at an existing directory
  for one command, in a namespace of its own, served by the host's own executable as `role::main`
  serves the roles. `exec::command(&spec, host_exe, &["slates-role".into()])` builds the command
  (`HOST_EXE slates-role exec --volume=V --at=PATH [--apparmor-profile] -- CMD`); the host spawns it
  with `SLATES_ROOT` set and awaits it. `ExecSpec::apparmor` is the AppArmor option, `AppArmor::Off`
  unless the user opts in: on a host whose AppArmor restricts unprivileged user namespaces (Ubuntu
  24.04 and later) `AppArmor::Profile` makes the namespace under slates' profile for the host's
  executable, which an administrator renders with `slates_client::exec::profile(host_exe)` and loads
  as root; off, the launcher never uses it. `exec::check()` answers the host's `Readiness`
  (`Unrestricted`, `ProfileLoaded`, `ProfileMissing`, `Unsupported`), and every refusal is an
  `ExecFault` naming its remedy.

Calls on `Instance` are synchronous: a provisioning verb answers in microseconds, and a landing or a
mount waits while the daemon lives. A host on its own event loop awaits the verbs `AsyncInstance` serves:
provisioning from every source, a snapshot, a status and a destroy. It lends one wait, `Reactor::wait`:
the client's completion signal readable, or a timeout, whichever comes first. Nothing spawns and no runtime
is added; a future dropped mid-call cancels its call. `AsyncInstance::new(instance)` and
`into_instance()` move between the forms; deregister the completion descriptor from the loop first,
since handing back closes it. The other verbs run on the host's blocking pool until the async form
serves them too. `slates_api::VERSION` is the crate's version, the workspace's one version;
`slates_api::CONTRACT` is the contract's, and its doc comment names what each version changed (a
test holds every version to a line there).

## A host's workspace flow

```rust no_run
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

The gap ledger's consumer-contract entry audits this library clause by clause, naming the tests
that hold each one and where they run; every clause is held by a test on main. Still owed beyond
the goal's clauses:

1. A typed field on the wire for a merge's versions (the work's base beside the head).
2. The async form for the remaining verbs: the paged reads, landings, merges, policies and
   observation.
3. The container engine profiles slates has run since its judge was written (rootless podman, and
   podman's command line), which the judge still refuses.
