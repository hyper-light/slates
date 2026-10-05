---
name: landing-slates-work-to-disk
description: Explains how work in a slates volume reaches the host's disk - a landing planned by the agent and granted only by a human - and how to ask for one. Use when a task's result must end up in a real directory on the host, when the user asks to save, apply, write out or land changes from slates, or when a tool answers GrantRequired.
license: MIT
metadata:
  server: slates
---

# Landing slates work to disk

Everything in a slates volume lives in RAM. The host's disk is written in one way only: a *landing*, and a landing
runs only after a human grants it. No tool you can call grants a landing, by design. You plan it and ask; the
person decides.

The tools below are on the `slates` MCP server (fully qualified, for example `slates:slates.land.materialize`).

## Plan

`slates.land.materialize` with `volume` and `target` (the absolute host directory to write). Optional: a
`snapshot` to land instead of the live tree, and `include` or `exclude` path lists. The result has
`grant_required: true`, the `landing` id, the `manifest` (the hash of exactly what would be written), a `summary`
of what it creates, changes and removes, any `conflicts` already known, and `grant_with`: the command that
authorizes that manifest. If a grant for it already exists, the call lands instead and returns the `outcome`.

## Ask

Show the person, briefly:

1. which directory will be written;
2. the summary's counts, and any conflicts;
3. the exact command from `grant_with`, for them to run themselves.

Do not run that command for them, and do not rephrase it: the grant is bound to that manifest's hash. If the
volume changes after you planned, plan again; the old grant no longer matches.

## After the grant

The person runs the command; slates writes the files, checking that each host file is still what the volume was
based on. A file someone changed on the host in the meantime is a conflict, never overwritten. Report the outcome
they see, and if there were conflicts, read both sides (`slates.base.read_base` for the host's, `slates.fs.read`
for the volume's) before proposing what to do.

## Never

- Never write project files to the host through another tool to get around the grant.
- Never describe the work as saved or applied before the person has granted and run the landing.
