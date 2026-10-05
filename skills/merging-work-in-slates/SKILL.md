---
name: merging-work-in-slates
description: Runs the slates merge loop - a shared green volume, one work volume per agent, edits declared as byte ranges, submit, and rebase on conflict - so several agents change one tree without overwriting each other. Use when more than one agent or task edits the same project, when you are asked to submit, merge or integrate changes in slates, or when a submit returns conflict windows.
license: MIT
metadata:
  server: slates
---

# Merging work in slates

slates merges by declared ranges, never by guessing. Each agent edits its own *work* volume; a submit is either
accepted at a new version of the shared *green*, or answered with the exact byte windows that conflict. Nothing
is three-way merged behind your back.

The tools below are on the `slates` MCP server (fully qualified, for example `slates:slates.merge.submit`).

## The loop

1. **Green.** One shared target: `slates.merge.create_green` with a `name`. To start from a project snapshot, pass
   `base_volume` and `base_snapshot` (pin the base first with `slates.base.pin`).
2. **Work.** Your own copy: `slates.merge.create_work` with the `green` id and a `name`. It is based on the green's
   head at that moment.
3. **Edit.** Declare each change as a splice: `slates.merge.edit` with `work`, `path`, `at` (byte offset),
   `delete_len` (bytes removed) and `text` (bytes inserted). For files and directories themselves use
   `slates.merge.declare` with `op.kind` one of `unlink`, `rename`, `mkdir`, `rmdir`, `set_mode`, `symlink`,
   `link`, `set_xattr`, `remove_xattr`.
4. **Submit.** `slates.merge.submit` with `work`. Accepted (`accepted: true`): the result names the green's new
   `version`. Conflict (`accepted: false`): the result lists `conflicts`, each a `path`, `at`, `len` and `class`.

## On a conflict

1. For each window, read what the green now holds there: `slates.fs.read` with the green's id, the `path`, and
   the green's head `version` (`slates.merge.versions`).
2. Rewrite your edit so it no longer overlaps what landed, keeping the other agent's change.
3. `slates.merge.rebase` with your `work` moves it onto the head, then submit again.

Never resolve a conflict by deleting the other change unless the user asked for that.

## Following the green

- `slates.merge.versions`: the head version.
- `slates.merge.changed_since` with a `version`: the files changed after it.
- A reader that must not see the tree move mid-task attaches to the green (`slates.attach.attach`); its view stays
  at that version until `slates.merge.advance`, which returns the version pinned and the paths that changed.

## Evidence

A green created with `require_evidence: true` accepts a submit only with `evidence`: opaque hex identities (for
example of a passing test run) the green's policy names.
