# The `slates` command

One binary, three roles. The design is `docs/wip/SLATES_DESIGN.md` §2.5, §2.6 and §4.12.

## Running a daemon

```
slates anchor [--instance NAME] [--quick] [--shards N]
```

The anchor measures the machine profile (seconds; `--quick` for tests), creates the shared
segment in RAM, publishes the profile into it, and supervises `slates daemon` as a child with
the segment handed over in its environment. It restarts a daemon that exits, kills one whose
heartbeat lapses, and gives up on a crash loop (the bound is derived from the recovery budget
and the measured daemon start). `SIGINT` or `SIGTERM` stops both. Nothing is written to disk.

`slates daemon` run alone measures a profile and serves without an anchor (development).

## Client verbs

The instance is `--instance`, else `SLATES_ENDPOINT`, else `default`.

```
slates volume create NAME (--bounded SIZE | --dynamic MAX) [--fold] [--locked] [--base DIR]
slates volume list
slates volume stat ID
slates volume snapshot ID
slates volume clone ID SNAPSHOT NAME
slates volume resize ID (--bounded SIZE | --dynamic MAX)
slates volume destroy ID
slates volume placed ID [--snapshot N] [--mirror]
slates attach ID [--read | --write] [--snapshot N]
slates detach ATTACHMENT
slates status ID [--drift]
slates base read ID PATH
slates base rewitness ID [PATH ...]
slates base pin ID [PATH ...]
slates profile [--quick] [--json]
```

Sizes take a binary unit (`512MiB`, `4GiB`; `B`, `KiB`, `MiB`, `GiB`, `TiB`) or plain bytes.
Ids are 32 hexadecimal characters as `create` prints them.

Output is plain and stable: one `key: value` per line (`create`, `stat`, `status`, `attach`,
`snapshot`, `clone`, `pin`), one record per line (`list`: id, name, `referenced=`, `unique=`,
`overlay=`; `status --drift` and `rewitness`: one path per line), `ok` for the verbs that
return nothing, and the raw bytes for `base read`.

Exit codes: 0 done; 1 the daemon refused (the refusal is named on stderr, e.g.
`AlreadyExists`); 2 usage; 3 no daemon at the instance; 4 the command itself failed.

There is no grant verb yet: grants arrive with the landing surface (design Phase 2 task 8),
and the ring channel refuses the kind by design (§4.13).
