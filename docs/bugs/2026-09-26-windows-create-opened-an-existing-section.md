# A Windows create silently opened an existing section

Date: 2026-09-26. Contracts: §4.8 (one anchor segment per daemon instance), R1, CLAUDE.md typed
refusals. Found reading the Windows shared-memory path while diagnosing CI run 36261369758's
`CreateFileMappingW` 1450. No failing run showed it.

## Root cause

`CreateFileMappingW` given a name that already names a live section returns a handle to *that* section,
at its existing size, and sets `ERROR_ALREADY_EXISTS` (Microsoft Learn, CreateFileMappingW, "Return
value"). Neither Windows create path checked:
- `slates_mem::SharedObject::create` (anchor segments, content objects, client regions, rendezvous);
- `slates_machine::segment::Segment::publish` (the profile).

A second creator under the same name therefore shared the first one's memory: two daemons, or two
test runs, writing one anchor segment. It mapped a view of the requested length over a section of
another length. On Linux the objects are unnamed `memfd`s; on macOS the create is `O_EXCL`.

## Fix

Both paths refuse the name with the OS code (`OsRefused { call: "CreateFileMappingW (the name is
already a live section)", code: 183 }`) and close the handle they were given. The close goes through
one helper, so the unsafe budget is unchanged.

The same sweep named every test's segment and content object by process id: the server daemon tests,
MCP, the CLI's run test, and the client tests (lifecycle, resume, id-end, green, origin, async, consumer,
reap). The re-invoked children learn their names from the handoff. This completes the sweep of
`2026-09-25-test-fixtures-measured-the-machine-beside-each-other.md`.

## Evidence and limits

- Cross-lint for `x86_64-pc-windows-msvc` (mem, machine, ipc, rt) is clean.
- The client, MCP and server daemon suites pass on macOS.
- The refusal's runtime proof on Windows is owed to the next Windows run.
