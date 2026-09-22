# Workspace chart golden used an unpinned Helm renderer

CI run 35615970514 has two different verdicts for the same chart and commit:
the KIND job 106386649727 passes its chart tests with pinned Helm 4.3.0, while
workspace job 106386649929 fails the golden render using the runner's tool.
The downloaded failed render differs only in five blank lines before YAML
document separators. Resource content and ordering are identical. (The log's
interleaved Cargo failure line is not rendered YAML.)

The golden is intentionally a byte identity gate. Its generating tool is an
input, just like the fixture values. Pin the existing workspace chart check to
Helm 4.3.0, matching the KIND lane and the local renderer. Keep the byte comparison
and the golden unchanged. This prevents a runner-image tool update from changing
the test's input silently; it does not remove review of chart changes.

Validation: local Helm reports `v4.3.0+gbec5b06`; all 4 chart gates pass in
0.08 s. The intentionally ignored golden writer was not run.
