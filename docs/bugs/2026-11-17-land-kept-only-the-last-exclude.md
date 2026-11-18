# `slates land` kept only the last `--exclude` (2026-11-17, §4.15 step 1)

Found by kiln's bare-metal engine (kiln goal 01a1ec34599974448aed2672bafaf9a2): a harness's landing leaves out git's own
directory and what git ignores, more than one prefix, and the command line took one.

## Description

`slates land ID TARGET --exclude /.git --exclude /target` landed `/.git` (its walk left out `/target` alone). The CLI's
flag table kept one value per flag, each repeat overwriting the one before, so every `--include` and `--exclude` but the
last was dropped without a word. The wire's `Filter` (`slates_client::Filter`) has carried lists from the start, and
§4.15 step 1 applies "the caller's filter (include and exclude patterns)": only the command line lost them.

## Root cause

`Taken.values` in `crates/cli/src/args.rs` was a `BTreeMap<&str, String>`, filled by `insert` per occurrence, and the
landing's filter read each flag through `value()`, one `Option<&str>`.

## Impact

A person or a harness naming several excludes landed what they meant to leave out, under a grant for exactly that plan
(the manifest bound what was planned, so nothing landed that the plan didn't show; the plan was wrong). kiln worked
around it by leaving out `/.git` alone and refusing to land a plan that held more files than git saw changed, so an
agent's build output kept its work from landing at all.

## Fix

`Taken.values` keeps every value of a flag, in order. `value()` answers the last one, as before, for the flags that take
one; `every()` answers all, and the landing's `--include` and `--exclude` read it. The usage line and docs/cli.md say
the flags repeat. Test: `args::tests::a_landing_keeps_every_include_and_exclude_given` (two of each, interleaved, in
order; no filter given keeps everything; a repeated single-valued flag keeps the last).

## Siblings

Every other value flag takes one value and still keeps the last of a repeat without a word (`--grant 1 --grant 7` is 7,
`--instance a --instance b` is b). None loses data the wire would keep; whether a repeat of a single-valued flag should
be refused instead is reported, not changed here.
