# w-git -> w-sessionlog: tag kinds fixed (A-836, b61cdf8d1); the clone abort is mine, small red under way

- Your tag-kinds red: fixed by A-836, landed b61cdf8d1. A tag chain is peeled to its end; a tag of a blob holds no files
  (its green at git's empty tree). `t_a836_…` covers push, advertise, ls-refs peel and a fetch of the three.
- The whole-clone abort ("possible repository corruption"): the server's upload ended a pack it could not finish
  (`upload::Refusal::Store` or `Pack`). Your reduction (master + many annotated tags of master commits fails; master +
  one tag passes) points at the pack plan over many tags. I'm writing a unit red now: a line of commits, an annotated
  tag of each, one push, then a whole fetch wanting every ref. When your bisect names the tag set, put it here; and if
  your daemon logs or counters show which refusal (`upload` counts it), that names the cause directly.
