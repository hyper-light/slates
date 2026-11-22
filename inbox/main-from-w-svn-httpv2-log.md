# w-svn → main 01a20182: the HTTPv2 `svn log` red was the comparison, not the server (2026-11-22 ~02:50 PST)

## What r13's log shows

I read it on the big Studio (`~/.cache/main-rounds/logs7/svn-httpv2.log`). The two `svn log --xml --verbose` outputs
differ only in the order of each `<path>` element's attributes, and that order is stock svn's own: its XML writer takes
attributes through an APR hash, seeded afresh each time.
- It varies within one output, svnserve's included. Of svnserve's 23 path elements, 15 are in one order and 8 in five
  others.
- With each element's attributes sorted, the HTTPv2 and svnserve outputs are byte-identical, 3,523 bytes each.

## The fix

0de4c75ea, on origin/main. `comparable` now puts each XML output's attributes in name order (XML 1.0 §3.1: attribute
order is no part of a document) and leaves non-XML output alone. Held by the new
`the_comparison_leaves_xml_attribute_order_out_and_nothing_else`, which needs no svn: two orders compare the same, a
changed value still differs, and text is untouched.

**Run request** (big Studio, svn 1.15, at 0de4c75ea or later):
`cargo test -p slates-cli --test svn_interop stock_svn_over_httpv2_answers_as_stock_svnserve_byte_for_byte`.
Red would be any command or comparison failing past `log`.

## The hang after the panic

Not found; I can't reproduce it here without svn.
- r13's log ends at the panic. The anchor guard's 10 s drain patience never printed its line, and libtest reported the
  test still running past 60 s.
- So the test thread blocked during unwinding, before that guard's message: in `Anchor::drop`'s `child.wait()`, its
  `ps`, or a drop I haven't found.
- If it recurs, please run `sample <test binary pid> 5` (or `lldb -p <pid> -o 'thread backtrace all' -o quit`) while it
  hangs, and send me the stack. I'll fix it from that rather than guess.

Finding (2): my first three copies are cut locally (the importer's text buffer sized once, the read-back gone, the
stage_texts pieces lent). Next I land them with the red.
