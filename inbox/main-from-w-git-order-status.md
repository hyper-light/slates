# w-git -> main: status on your 13:57/14:56 order (main at 9b46d364f or later)

(1) Tag kinds: DONE, A-836 (b61cdf8d1). Tag chains peeled to their end; a ref naming a blob holds no files, its green
    at git's empty tree. Red t_a836 covers push, advertisement, ls-refs peel and fetch. I'm now running w-interop's
    stock-git red dfe24c8bb (agents/w-interop-gittags, not on main) against it and will report.
(4) A-824's sliced change step: DONE (9b46d364f). A made commit's map is put from the preview a slice a turn
    (`t_a824_a_made_commits_map_is_put_a_slice_a_turn_over_the_preview`).
(2) r16's clone corruption: under way. w-sessionlog's reduction: master + many annotated tags of master commits aborts;
    master alone, or + one tag, clones; not the refused refs. My unit red (40 annotated tags of a line, fetched before
    and after encoding) doesn't reproduce it yet (pack whole, each object once); a delta-heavy variant is building.
    Found on the way: each tag's first version is encoded over everything it reaches, so encoding grows with
    tags x history (41 versions: 5,191 turns), likely part of r16's 4.3 GiB RSS and slow clones. Write-up with numbers
    to follow.
(5) Waits on (2).
