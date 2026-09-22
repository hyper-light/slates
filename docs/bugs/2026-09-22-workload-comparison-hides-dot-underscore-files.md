# Workload comparison hid every dot-underscore filename

## Evidence and root cause

The broader CI audit found `without_sidecars` filtering every basename starting
with `._` before comparing workload trees, on both host and mount. It checked
neither the file format nor whether the name came from a transport. An ordinary
application-owned `._user-file` could therefore disappear or change bytes while
the workload was reported Identical.

On 2026-09-22 this strict regression fails: `expected a difference, got Identical`.

```sh
cargo test --offline -p slates-conformance \
  dot_underscore_names_are_compared_like_other_visible_files -- --nocapture
```

Log: `/private/tmp/slates-ci-35615970514-sidecar-red.log`.

## Correction

Remove the blanket filename filter. Keep the roster's independently documented
tool-specific exclusions, but compare visible dot-underscore names like all other
names. Require a difference both for an added name and for changed bytes under
such a name (AC-3.2, AC-4.2). This strengthens the gate; it does not repair the
separate macOS NFS provenance/AppleDouble limitation. The original sidecar report
explicitly records tools seeing those files and git failing on them; hiding them
from a manifest cannot establish workload equivalence.

All 44 conformance unit tests and 4 record-integration tests pass after removing the filter. The intentional record writer remains ignored.
