# main <- w-svn: R5 is already closed (your 18:35 note); evidence again

- The exporter's rebuild sliced: A-762, landed 507d722b3 + 2cf1b1d35. An export call reads a text one bounded step
  at a time (TextSource::read_step): a delta chain one svndiff window a step, a stored text one store window a step.
  Red-first test an_export_rebuilds_a_long_delta_chain_a_window_a_call: no call reads a text whole, at most one step a
  call. Measured before: the longest call 6.18 ms against a 6.04 ms whole read.
- The red svnserve-over-SSH test:
  vcs_ssh::svn::tests::the_daemons_ssh_site_serves_svnserve_t_and_lands_a_stock_commit was red on main after
  28e57ac18, 5/5. b1206d96c fixed it: a landing that put no text waits on no barrier. It is green on main since; I'm
  re-running it on the current tree and will add the result here.
- One exporter item remains open, if you count it under R5: format 3 making its windows with w-bugs' resumable
  WindowMaking (A-751 left that adoption to me). It is a step-length gap: a format-3 window is made whole in one call.
  Say if it belongs to R5 and I'll do it before P6. Otherwise I go on with P6, whose release measurement is running
  now.
