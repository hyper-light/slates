---
name: working-in-slates-volumes
description: Creates, clones, snapshots, attaches and reads slates volumes, the RAM-only copy-on-write workspaces an agent gets in place of the host disk. Use when you need a private, disposable copy of a project or directory, when a task asks for a sandbox, scratch space or an isolated checkout, or when you must read what is in a slates volume.
license: MIT
metadata:
  server: slates
---

# Working in slates volumes

A slates volume is a copy-on-write directory tree held in RAM by the slates daemon. Nothing you write in it touches
the host's disk. A volume can sit over a host directory (its *base*): it then reads the base's files until you
change them, and your changes stay in the volume.

The tools below are on the `slates` MCP server (fully qualified, for example `slates:slates.volume.create`).

## Ids

Every volume id is opaque lowercase hex. Pass back exactly what a tool returned; never build or shorten one. A
failed call returns `isError: true` with `structuredContent.error.code` (`-32602` a bad argument, `-32000` a typed
refusal from the daemon whose message names it, `-32001` the daemon is not running).

## Make a workspace

1. Over an existing project: `slates.volume.create` with `name`, a size, and `base` set to the project's absolute
   path. The size is either `bounded` (a fixed byte limit) or `dynamic` (a maximum it may grow to).
2. From scratch: the same call without `base`.
3. A copy of a volume as it is now: `slates.volume.snapshot` (returns a snapshot id), then `slates.volume.clone`
   with that `snapshot` and a new `name`. Clones share unchanged bytes, so they are cheap; destroying the original
   leaves its clones intact.

## Read and inspect

- `slates.fs.read` with `volume` and `path`: a file's bytes as text plus its exact length.
- `slates.volume.stat`: bytes used, attachments, snapshots, and drift from the base.
- `slates.volume.list`: every volume with its referenced and unique bytes.
- `slates.base.read_base`: a file as the base holds it, ignoring the volume's changes.
- `slates.base.rewitness`: re-checks base entries; it returns the paths whose host file changed under you.

## Survey or search with one query

Rather than listing and reading file by file, ask `slates.query` once; only its answer comes back:

```text
FROM files("my-volume", under = "src") WHERE ext = "rs" AND content CONTAINS "unsafe" SELECT path, size ORDER BY size DESC LIMIT 20
FROM lines("my-volume") WHERE text CONTAINS "TODO" SELECT path, line, text LIMIT 50
```

Sources are `volumes()`, `files(...)`, `lines(...)` and `changed("green", since = N)`; a volume is its id or its name.
A query that would read or return too much is refused naming the ceiling: narrow it with `WHERE`, `under =` or
`LIMIT`. `slates.fs.list` lists one directory.

## Attach

`slates.attach.attach` gives an attachment id. Add `write: true` to take the volume's write lease. To hand the
volume to a container, the human first runs `slates mount <volume> <mount point>` on the host; then pass
`oci_source` (that mount point) and `oci_destination` (the path inside the container), and give the returned
`mounts` entry to the OCI runtime. Release with `slates.attach.detach`.

## Write files without a mount

With a write attachment (`slates.attach.attach` with `write: true`), change a plain volume's files directly; pass the
`volume` and that `attachment` to each:

- `slates.fs.write` with `path` and `text`: the file's whole new content. An absent file is created (its directory
  must exist); a present one is replaced. Large files are fine: they are sent in pieces and written at once.
- `slates.fs.mkdir` with `path`; `slates.fs.move` with `from` and `to`; `slates.fs.remove` with `path` (a file, a
  link, or an empty directory).

Paths are relative to the volume's root and may not use `..`. A refusal names why (`NotFound` for a missing
directory, `Forbidden` for an attachment that is not yours or not for writing).

## Clean up

`slates.volume.destroy` frees a volume's RAM at once. Snapshots and clones that still need its bytes keep them.

## What this skill cannot do

It cannot put anything on the host's disk. That takes a landing, and only a human can grant one (see the
`landing-slates-work-to-disk` skill).
