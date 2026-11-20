import sys
d='/Users/adalundhe/.local/state/slates-fanout/g295/scratch/'
work=sys.argv[1]
head=open(d+'head-engine.rs').read()
main=open(d+'main-engine.rs').read()
START="  /// Remove the live inode and its metadata while retaining their historical values.\n  fn remove_special("
END="  /// Records what version `version` changed in the change index (`changes`)"
def region(s):
    return s[s.index(START):s.index(END)]
h=region(head)
m=region(main)
def between(s, a, b):
    i=s.index(a); j=s.index(b, i)
    return s[i:j]
# main's remove_special + drop_metadata (up to its seed_of doc)
main_special=between(m, START, "  /// The [`Seed`] of a change at")
main_holds=m[m.index("  /// Whether anything stands at `path`: a file, a directory"):]
# my seed_of .. end, without remove_mode / remove_special
mine=h[h.index("  /// The [`Seed`] of a change at"):]
def rep(old,new):
    global mine
    assert mine.count(old)==1, old[:70]
    mine=mine.replace(old,new)
rep('''    for file in staged.set {
      self.set_file(file, version, &mut delta);
    }
    self.deltas.push(delta);''','''    for file in staged.set {
      self.set_file(file, version, &mut delta);
    }
    // A file, a directory or a link this version removed takes its mode and attributes with it once nothing stands at
    // its path (a rename or a file made again in the same version keeps them), as a special's do: they are the inode's,
    // so a path made again later starts without them (A-618; a file a client deleted and pushed again kept the deleted
    // one's mode 0755).
    for path in staged.emptied {
      if !self.holds(&path) {
        self.drop_metadata(&path, version);
      }
    }
    self.deltas.push(delta);''')
rep('''      Effect::RemoveContent(path) => staged.removed.push(path),''','''      Effect::RemoveContent(path) => {
        staged.emptied.push(path.clone());
        staged.removed.push(path);
      }''')
rep('''      Effect::RemoveDir(path) => {
        let stood''','''      Effect::RemoveDir(path) => {
        self.file_removed.insert(path.clone(), version);
        staged.emptied.push(path.clone());
        let stood''')
rep('''      Effect::RemoveSymlink(path) => self.set_symlink(path, None, version),''','''      Effect::RemoveSymlink(path) => {
        self.file_removed.insert(path.clone(), version);
        staged.emptied.push(path.clone());
        self.set_symlink(path, None, version);
      }''')
rep('''      Effect::RemoveHardlink(path) => self.set_hardlink(path, None, version),''','''      Effect::RemoveHardlink(path) => {
        self.file_removed.insert(path.clone(), version);
        staged.emptied.push(path.clone());
        self.set_hardlink(path, None, version);
      }''')
main_special=main_special.replace('''  /// Removes `path`'s mode and attributes, keeping their values in the history and marking what the identity and the
  /// verdict read: what a removed special node's metadata does (`remove_special`), and since A-618 a removed file's,
  /// directory's and link's, once nothing stands at the path.''','''  /// Removes `path`'s mode and attributes, keeping their values in the history and marking what the identity and the
  /// verdict read: what a removed special node's metadata does (`remove_special`), since A-618 a removed file's,
  /// directory's and link's once nothing stands at the path, and a replaced tree's (A-626).''')
resolved=main_special+mine+main_holds
w=open(work).read()
i=w.index(START); j=w.index(END)
w=w[:i]+resolved+w[j:]
# Staged gains `emptied`
old='''#[derive(Default)]
struct Staged {
  removed: Vec<String>,
  set: Vec<StagedFile>,
}'''
new='''#[derive(Default)]
struct Staged {
  removed: Vec<String>,
  set: Vec<StagedFile>,
  /// The paths whose file, directory or link the commit removes: their mode and attributes go with them once nothing
  /// stands there (A-618).
  emptied: Vec<String>,
}'''
assert w.count(old)==1
w=w.replace(old,new)
open(work,'w').write(w)
print("ok", w.count("<<<<<<<"), w.count("======="), w.count(">>>>>>>"))
