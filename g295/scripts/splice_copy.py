import sys
d='/Users/adalundhe/.local/state/slates-fanout/g295/scratch/p2/'
p=sys.argv[1]
s=open(p).read()
def between(a, b):
    i=s.index(a); j=s.index(b, i)
    return i, j
def splice(a, b, new):
    global s
    i,j=between(a,b)
    s=s[:i]+new+s[j:]
splice("  /// The verdict of each copy the increment declares (module docs), its effect or its conflict: a copy into another's",
       "  /// `effect`, or a type conflict on `path` when `refused`.", open(d+'verdicts.rs').read())
splice("  /// Whether a file, directory, symbolic link, hard link or IPC node stands at `path`.",
       "  /// Applies the copies among a version's accepted effects", open(d+'readers.rs').read())
splice("  /// Applies the copies among a version's accepted effects",
       "  /// Takes `dst`'s tree away key by key at `version`", open(d+'commit.rs').read())
# types after the CopySpec struct
anchor='''  /// Whether it replaces what its destination holds.
  pub(super) replace: bool,
}
'''
assert s.count(anchor)==1
s=s.replace(anchor, anchor+open(d+'types.rs').read())
# imports
old='''use crate::dims::{Block, Held, HeldAttributes};'''
assert s.count(old)==1
s=s.replace(old,'''use crate::chain_entry::{Applied, CopyFix};
use crate::dims::{Block, Held, HeldAttributes};''')
open(p,'w').write(s)
print("ok")
