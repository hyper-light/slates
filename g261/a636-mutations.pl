# A-636's red checks: each mutation removes one rule; the logs tests that name it must fail.
use strict;
use warnings;
my $file = "crates/db/src/ledger/logs.rs";
my @mutations = (
  ["keep_whole_off", "    let dropped = keep_whole(&mut owners, &cohort.records);\n",
   "    let dropped: Vec<[u8; 32]> = Vec::new();\n"],
  ["precheck_off", "    for (name, log) in &cohort.logs {\n      log\n        .can_take_over(reachable)\n        .map_err(|error| LogsTakeoverRefused { log: *name, error })?;\n    }\n", ""],
  ["split_off", "        .is_some_and(|spans| spans_split_by(spans, at))\n",
   "        .is_some_and(|spans| spans_split_by(spans, at) && spans.is_empty())\n"],
  ["applied_bound_off", "      if *asked > applied {\n", "      if *asked > applied && applied > *asked {\n"],
  ["moved_off", "      if span.from != at {\n", "      if span.from != at && span.from == at {\n"],
  ["commit_tracking_off", "      if last.is_some_and(|commit| commit.committed) {\n",
   "      if last.is_some_and(|commit| commit.committed && commit.acked.is_empty()) {\n"],
  ["announce_apply_off", "      cohort.apply(*host, held, &cut);\n", "      let _ = (host, held);\n"],
  ["cut_not_whole", "    whole_cut(limits, &identity_at, &cohort.records)\n",
   "    let _ = (&identity_at, &cohort.records);\n    limits\n"],
);
sub slurp { open(my $in, '<:raw', $_[0]) or die "read $_[0]"; local $/; my $text = <$in>; close $in; return $text; }
sub spill { open(my $out, '>:raw', $_[0]) or die "write $_[0]"; print $out $_[1]; close $out; }
my $pristine = slurp($file);
for my $mutation (@mutations) {
  my ($name, $old, $new) = @$mutation;
  my $count = () = $pristine =~ /\Q$old\E/g;
  if ($count != 1) { print "$name TARGET_NOT_UNIQUE $count\n"; next; }
  my $mutated = $pristine;
  substr($mutated, index($mutated, $old), length($old)) = $new;
  spill($file, $mutated);
  my $out = `cargo test -p slates-db --lib ledger::logs 2>&1`;
  my $status = $? >> 8;
  my @failed = ($out =~ /^test (\S+) \.\.\. FAILED/mg);
  my $built = ($out !~ /^error(\[|:)/m);
  my $compiled = ($out =~ /Compiling slates-db/) ? "rebuilt" : "NOT_REBUILT";
  my $verdict = !$built ? "NO_BUILD" : (($status != 0 && @failed) ? "RED" : "GREEN");
  print "$name $verdict $compiled exit=$status failed: ", join(",", @failed), "\n";
  spill($file, $pristine);
}
my $restored = slurp($file) eq $pristine ? "restored" : "NOT_RESTORED";
print "file $restored\n";
