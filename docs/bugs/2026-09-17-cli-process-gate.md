# CLI process gate: leading global flags and an obsolete fleet fixture

Date: 2026-09-17. Contracts: §4.12–§4.13 (CLI and consumer delivery), §4.8
(explicit bootstrap, fresh member identities and session replacement).

## Evidence

[macOS job 105312670519](https://github.com/hyper-light/slates/actions/runs/35253899553/job/105312670519)
passed format, Clippy, structural/literal/version checks, workspace tests and the million-operation
model history. Its explicitly enabled CLI process suite failed two of nine tests in 18.58 seconds:
`slates_run_spawns_the_command_as_an_ephemeral_consumer` rejected `--` as an unknown flag;
`three_daemon_processes_deploy_a_fleet_from_one_manifest_and_survive_the_owners_death` had
all three members and both probe peers but rejected four successful session replacements.

Local macOS arm64, Rust 1.98.0, HEAD `a4fe23a`: the two new argument histories fail with
`UnknownFlag("--")` in 0.00 seconds. The unchanged fleet process history reproduces the
replacement assertion in 1.82 seconds. Commands (serial, build concurrency two, each
supervised within 120 seconds):

```sh
cargo test --offline --locked -p slates-cli --bin slates args::tests::global_flags_before -- --nocapture
SLATES_TEST_CLI=1 cargo test --offline --locked -p slates-cli --test cli \
  three_daemon_processes_deploy_a_fleet_from_one_manifest_and_survive_the_owners_death -- --exact --nocapture
```

Logs: `$TMPDIR/slates-followups-o5314nq6/{macos-105312670519,cli-args-red,cli-fleet-red}.log`.

## Root causes and edits

The parser selects `run`/`exec` only when the verb is the first raw argument. With a leading
`--instance`, the generic scanner consumes the whole invocation and rejects the command
separator. Parse the slates side of the separator once, dispatch using the parsed positional
verb, and hand the untouched suffix to the child. Both verbs need regressions with global
flags before the verb, child flags, and a flag value equal to a verb name.

The fleet fixture equates session replacement and deliberate invalidation of a seed link
with a transport failure. First contact now learns fresh member identities; superseded
discovery and handshake work ends deliberately. Distinguish an accepted handshake closed
by replacement from an actual handshake failure, and allow only named lifecycle outcomes
in the fixture. Retain the queue/capacity and unexpected-refusal checks and prove placement,
retirement and takeover by use.

Both the issuer-surface helper and fleet fixture omit explicit first-time bootstrap; the
fleet fixture also always kills node zero. Bootstrap
once, preserve the root representative, and choose another owner. Otherwise passing the
first obsolete assertion merely reveals an uninitialized consensus group, or destroys the
singleton root authority instead of exercising takeover through a surviving quorum.

## Implemented and validated

The command boundary is now parsed before dispatch, with the suffix preserved exactly.
`exec` had the same leading-flag defect and is covered too. Stray positional arguments on
the slates side are now refused instead of discarded. Both accepted-session serve paths
classify `Closed` as `fleet.accept.replaced`; real TLS/socket failures remain
`fleet.accept.handshake`. The process fixture permits only that replacement, discovery
invalidation and stale-link return; queue/capacity loss and other refusals still fail.

Both fresh-deployment fixtures now bootstrap explicitly. The fleet fixture bootstraps
the lowest-id representative, keeps it alive, and waits through only the typed
`ConsensusNotInitialized` refusal on the joining owner's create, within the existing
40-second bound. There is no production auto-bootstrap or changed timeout.

Local validation on 2026-09-17: the consumer process history passes in **1.89 s**; the
fleet history passes in **8.90 s**, including kernel-mount write/read-back on macOS.
The intermediate failures after repairing the parser/formation assertions were both
`ConsensusNotInitialized`; these established the missing-bootstrap fixture errors.
Logs `cli-{consumer,fleet}-validated.log` are alongside the red evidence.

`cargo test --offline --locked -p slates-cli --bin slates`: **32 passed, one child-only
fixture ignored**, 0.95 s. `cargo clippy --offline --locked -p slates-cli -p slates-server
--all-targets -- -D warnings`: clean, 1.97 s including the command overhead. `cargo xtask
check`: structural, literal, unsafe and version gates pass, 1.49 s. Formatting and
`git diff --check` pass. The two changed process histories were run serially with
`SLATES_TEST_CLI=1`; the entire nine-history process suite has not been rerun locally.
