"""Resolve the merge of origin/main a7b5e4292 into agents/g303-a680-p2b-land2.

Each conflict is resolved per file by a named rule; the result is written through a temporary file and a rename.
"""
import os
import sys

ROOT = sys.argv[1]


def blocks(lines):
    """Split lines into a list of ('common', [lines]) and ('conflict', ours, theirs)."""
    out, i = [], 0
    common = []
    while i < len(lines):
        line = lines[i]
        if line.startswith('<<<<<<< '):
            out.append(('common', common))
            common = []
            ours, theirs = [], []
            i += 1
            while not lines[i].startswith('======='):
                ours.append(lines[i]); i += 1
            i += 1
            while not lines[i].startswith('>>>>>>> '):
                theirs.append(lines[i]); i += 1
            i += 1
            out.append(('conflict', ours, theirs))
            continue
        common.append(line)
        i += 1
    out.append(('common', common))
    return out


def write(path, text):
    tmp = path + '.g303-tmp'
    with open(tmp, 'w') as handle:
        handle.write(text)
    os.replace(tmp, path)


def resolve(rel, rule):
    path = os.path.join(ROOT, rel)
    with open(path) as handle:
        lines = handle.readlines()
    parts = blocks(lines)
    conflicts = [p for p in parts if p[0] == 'conflict']
    result = []
    index = 0
    for part in parts:
        if part[0] == 'common':
            result.extend(part[1])
        else:
            result.extend(rule(index, part[1], part[2]))
            index += 1
    text = ''.join(result)
    for marker in ('<<<<<<< ', '\n=======\n', '>>>>>>> '):
        assert marker not in text, (rel, marker)
    write(path, text)
    print(rel, 'resolved', len(conflicts), 'conflicts')


def theirs(index, ours, theirs_):
    return theirs_


def ours_then_theirs(index, ours, theirs_):
    sep = [] if (ours and ours[-1].strip() == '') else ['\n']
    return ours + sep + theirs_


def daemon_edge(index, ours, theirs_):
    # Both sides appended blocks; the text after each conflict closes the last item of both sides.
    if index == 0:
        return ours + ['    })\n', '    .collect()\n', '}\n', '\n'] + theirs_
    if index == 1:
        return ours + ['}\n', '\n'] + theirs_
    raise SystemExit('unexpected conflict %d' % index)


def hg_imports(index, ours, theirs_):
    assert ours == ['use crate::repository::hg_push::Pushing;\n',
                    'use crate::vcs_http::{Answer, Decided, Exchange, Failed};\n'], ours
    assert theirs_ == ['use crate::vcs_http::{Answer, Decided, Exchange, Failed, Site};\n'], theirs_
    return ['use crate::repository::hg_push::Pushing;\n',
            'use crate::vcs_http::{Answer, Decided, Exchange, Failed, Site};\n']


def design(index, ours, theirs_):
    # The amendment log's end: main's entries first, A-680 part 2b last.
    assert ours[0].startswith('### A-680 part 2b'), ours[0]
    assert theirs_[0].startswith('### A-686'), theirs_[0]
    body = theirs_
    if body and body[-1].strip() != '':
        body = body + ['\n']
    return body + ours


resolve('crates/cli/tests/cli.rs', theirs)
resolve('crates/server/src/vcs_http/tests/daemon_edge.rs', daemon_edge)
resolve('crates/server/src/vcs_http/tests/hg.rs', hg_imports)
resolve('docs/wip/GAPS.md', theirs)
resolve('docs/wip/SLATES_DESIGN.md', design)
