"""A-655 part 2a's red copies: each new guard removed, by name. Run in a clone's root on the Studio:
python3 a655-2a-red.py M1 M2 ... (or ALL). Each replacement must match exactly once."""
import pathlib, sys

MUTATIONS = {
  # The index commits a stale plan.
  "M1": ("crates/svn/src/index.rs",
         "    if planned.revision != next {\n      return Err(IndexRefusal::OutOfDate {",
         "    if false && planned.revision != next {\n      return Err(IndexRefusal::OutOfDate {"),
  # The step takes a revision whose head is not the youngest.
  "M2": ("crates/svn/src/migration/records.rs",
         "    if revision.head != youngest {",
         "    if false && revision.head != youngest {"),
  # The lock table takes an entry removing a lock it does not hold.
  "M3": ("crates/svn/src/lock.rs",
         "          if held != Some(token) {",
         "          if false && held != Some(token) {"),
  # The step takes rows naming a content nobody holds.
  "M4": ("crates/svn/src/migration/records.rs",
         "      let bytes = brought.remove(&identity).ok_or(StepRefusal::Corrupt)?;",
         "      let Some(bytes) = brought.remove(&identity) else {\n        continue;\n      };"),
  # The step passes the room.
  "M5": ("crates/svn/src/migration/records.rs",
         "    if held > self.room.bytes {",
         "    if false && held > self.room.bytes {"),
  # A client's revision is not dated at the step.
  "M6": ("crates/svn/src/commit/revision.rs",
         "    properties.insert(REVISION_DATE.to_vec(), date.into_bytes());",
         "    drop(date);"),
  # open-root takes revision properties svnserve refuses.
  "M7": ("crates/svn/src/commit/drive.rs",
         "      validate_prop(name, Some(value.as_slice()))?;",
         "      drop((name, value));"),
  # The ephemeral svn:txn- properties are kept.
  "M8": ("crates/svn/src/commit/drive.rs",
         "    revision_properties.retain(|(name, _)| !name.starts_with(TXN_PREFIX));\n",
         ""),
  # open-root does not wait for start-commit.
  "M9": ("crates/svn/src/commit/drive.rs",
         "    Ok(Outcome::Hook(Due::StartCommit {\n      transaction: self.setup.transaction.clone(),\n    }))\n",
         "    self.make_root()\n"),
  # close-edit judges before pre-commit.
  "M10": ("crates/svn/src/commit/drive.rs",
          "      EditCommand::CloseEdit => self.close_edit(),",
          "      EditCommand::CloseEdit => self.close_edit().and_then(|_| self.judge(world)),"),
}

names = sorted(MUTATIONS, key=lambda name: int(name[1:])) if sys.argv[1:] == ["ALL"] else sys.argv[1:]
for name in names:
  path, old, new = MUTATIONS[name]
  file = pathlib.Path(path)
  text = file.read_text()
  if text.count(old) != 1:
    raise SystemExit(f"{name}: {text.count(old)} matches in {path}")
  file.write_text(text.replace(old, new))
  print(f"{name} applied to {path}")
