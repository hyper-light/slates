use strict; use warnings;
# A-655 part 2a's red copies, each new guard removed by name (ALL for every one); generated from a655-2a-red.py.
my %only = map { $_ => 1 } @ARGV;
my @mutations = (
  ["M1", "crates/svn/src/index.rs", "    if planned.revision != next {\n      return Err(IndexRefusal::OutOfDate {", "    if false && planned.revision != next {\n      return Err(IndexRefusal::OutOfDate {"],
  ["M2", "crates/svn/src/migration/records.rs", "    if revision.head != youngest {", "    if false && revision.head != youngest {"],
  ["M3", "crates/svn/src/lock.rs", "          if held != Some(token) {", "          if false && held != Some(token) {"],
  ["M4", "crates/svn/src/migration/records.rs", "      let bytes = brought.remove(&identity).ok_or(StepRefusal::Corrupt)?;", "      let Some(bytes) = brought.remove(&identity) else {\n        continue;\n      };"],
  ["M5", "crates/svn/src/migration/records.rs", "    if held > self.room.bytes {", "    if false && held > self.room.bytes {"],
  ["M6", "crates/svn/src/commit/revision.rs", "    properties.insert(REVISION_DATE.to_vec(), date.into_bytes());", "    drop(date);"],
  ["M7", "crates/svn/src/commit/drive.rs", "      validate_prop(name, Some(value.as_slice()))?;", "      drop((name, value));"],
  ["M8", "crates/svn/src/commit/drive.rs", "    revision_properties.retain(|(name, _)| !name.starts_with(TXN_PREFIX));\n", ""],
  ["M9", "crates/svn/src/commit/drive.rs", "    Ok(Outcome::Hook(Due::StartCommit {\n      transaction: self.setup.transaction.clone(),\n    }))\n", "    self.make_root()\n"],
  ["M10", "crates/svn/src/commit/drive.rs", "      EditCommand::CloseEdit => self.close_edit(),", "      EditCommand::CloseEdit => self.close_edit().and_then(|_| self.judge(world)),"],
);
for my $mutation (@mutations) {
  my ($name, $path, $old, $new) = @$mutation;
  next unless $only{ALL} || $only{$name};
  local $/;
  open(my $in, '<:raw', $path) or die "$path: $!";
  my $text = <$in>;
  close $in;
  my $count = () = $text =~ /\Q$old\E/g;
  die "$name: $count matches in $path\n" unless $count == 1;
  $text =~ s/\Q$old\E/$new/;
  open(my $out, '>:raw', $path) or die "$path: $!";
  print $out $text;
  close $out;
  print "$name applied to $path\n";
}
