# Conformance reports root availability as the identity running pjdfstest

Date: 2026-09-17
Design: Part 6 conformance, AC-3.1, R5 and R10.

## Evidence and root cause

[Ubuntu conformance job 105312670403](https://github.com/hyper-light/slates/actions/runs/35253899553/job/105312670403)
reports 6202 unexpected failures out of 8798 pjdfstest cases. Its artifact says privilege `root`
but records `sh <each test>`, without sudo. Ownership results repeatedly show uid/gid 1001/1001;
1665 failed commands attempt `-u` identity switches and receive no output.

`Run::privilege()` returned Root when passwordless sudo was available. `run_pjdfstest` then used
that value to ask whether the current process was already root. As a result, an unprivileged
caller with sudo available never invoked sudo. The parser nevertheless judged the output as root
and selected the root expected-failure list. Other suite records also mislabeled the tested
process's privilege by using availability instead of its actual identity.

The extracted production decision, driven through a simulated root-only ownership operation and
TAP classification, fails in 0.00 s:

```
cargo test -p xtask conformance::suites::tests::a_sudo_capable_caller_actually_elevates_the_root_cases_it_reports -- --exact --nocapture
```

It returns a failed `chown file 65534 65534`, expected 0, got EPERM, while claiming a root runner.
The simulated child avoids requiring root on the developer's machine; it is not a native mount run.

## Fix

Derive elevation from both inputs: root is available, and the caller's **actual** identity is not
root. Derive the parser identity from that same invocation decision. Use that identity for the
expected-failure list and the result's recorded privilege. Other suite results record the actual
identity running their workload; mount brokers and tracing helpers do not turn the workload into
root. No expected-failure list is expanded, and the daemon acquires no new privilege.

The original pjdfstest run also contains declared special-file limitations and their consequences.
Correcting its runner does not establish that every reported failure will disappear; the corrected
native run remains the authority for those verdicts.

## Validation

2026-09-17, macOS arm64, Rust 1.98.0: `cargo test -p xtask conformance:: -- --nocapture`
passes all four selected tests in 0.00 s (including the two invocation histories).
`cargo clippy -p xtask --all-targets -- -D warnings` passes in 0.59 s. The sibling sweep
replaced every use of available authority as a suite's reported effective identity.
