#!/usr/bin/env python3
"""A-699 (UI goal 93): a Subversion dump (format 2) of a git repository's first-parent history, so the Subversion
baseline (svn_scale.sh) is measured over the same dataset as git's (git/git).

Each first-parent commit of REF, oldest first, becomes one revision under trunk/: its log message, its author's name as
svn:author and its committer date as svn:date. A revision holds the commit's changes against its first parent (`git
diff-tree -r --no-renames`): a deleted file is a delete node, an added file an add node with its parent directories
added before it, a changed file a change node with its full text. An executable file carries svn:executable, a
symbolic link svn:special with the "link TARGET" text Subversion stores; a submodule (a gitlink) has no Subversion
form and is left out. A directory left empty is deleted, as git keeps no empty directory; a path that turns from a
file into a directory (or back) is deleted before it is added again. Texts are written whole (no deltas).

Usage: git_to_svn_dump.py GIT_DIR REF [LIMIT] > dump
  GIT_DIR  a git repository (bare or not)
  REF      the history's tip, its first parents walked
  LIMIT    keep only the newest LIMIT commits (the oldest one kept is written whole as revision 1); default all
"""

import subprocess
import sys
import time


def revision_properties(author, date, message):
    """A revision's property block: svn:author, svn:date and svn:log."""
    block = b""
    for name, value in ((b"svn:author", author), (b"svn:date", date), (b"svn:log", message)):
        block += b"K %d\n%s\nV %d\n%s\n" % (len(name), name, len(value), value)
    return block + b"PROPS-END\n"


def node_properties(mode):
    """A file node's property block for its git mode."""
    if mode == "100755":
        return b"K 14\nsvn:executable\nV 1\n*\nPROPS-END\n"
    if mode == "120000":
        return b"K 11\nsvn:special\nV 1\n*\nPROPS-END\n"
    return b"PROPS-END\n"


class Converter:
    def __init__(self, git_dir, out):
        self.git_dir = git_dir
        self.out = out
        self.batch = subprocess.Popen(
            ["git", "-C", git_dir, "cat-file", "--batch"], stdin=subprocess.PIPE, stdout=subprocess.PIPE
        )
        # Every directory under trunk/ and the count of its entries (files and directories), and every file.
        self.directories = {"trunk": 0}
        self.files = set()

    def blob(self, object_id):
        self.batch.stdin.write(object_id.encode() + b"\n")
        self.batch.stdin.flush()
        header = self.batch.stdout.readline().split()
        size = int(header[2])
        body = self.batch.stdout.read(size)
        self.batch.stdout.read(1)
        return body

    def write_node(self, path, kind, action, properties=None, text=None):
        lines = [b"Node-path: " + path.encode("utf-8", "surrogateescape")]
        if kind:
            lines.append(b"Node-kind: " + kind)
        lines.append(b"Node-action: " + action)
        content = b""
        if properties is not None:
            lines.append(b"Prop-content-length: %d" % len(properties))
            content += properties
        if text is not None:
            lines.append(b"Text-content-length: %d" % len(text))
            content += text
        if properties is not None or text is not None:
            lines.append(b"Content-length: %d" % len(content))
        self.out.write(b"\n".join(lines) + b"\n\n" + content + b"\n\n")

    def parent_of(self, path):
        return path.rsplit("/", 1)[0]

    def add_directories(self, path):
        """Adds the directories above PATH that are not there yet, outermost first."""
        missing = []
        directory = self.parent_of(path)
        while directory not in self.directories:
            missing.append(directory)
            directory = self.parent_of(directory)
        for directory in reversed(missing):
            if directory in self.files:
                self.delete_file(directory)
            self.write_node(directory, b"dir", b"add", properties=b"PROPS-END\n")
            self.directories[directory] = 0
            self.directories[self.parent_of(directory)] += 1

    def delete_file(self, path):
        self.write_node(path, None, b"delete")
        self.files.discard(path)
        self.forget_entry(path)

    def forget_entry(self, path):
        """Counts one entry gone from PATH's directory, deleting each directory it leaves empty."""
        directory = self.parent_of(path)
        self.directories[directory] -= 1
        while directory != "trunk" and self.directories[directory] == 0:
            self.write_node(directory, None, b"delete")
            del self.directories[directory]
            directory = self.parent_of(directory)
            self.directories[directory] -= 1

    def delete_directory(self, path):
        """Deletes a whole directory (a file takes its place)."""
        self.write_node(path, None, b"delete")
        prefix = path + "/"
        for name in [name for name in self.directories if name.startswith(prefix)]:
            del self.directories[name]
        self.files.difference_update([name for name in self.files if name.startswith(prefix)])
        del self.directories[path]
        self.forget_entry(path)

    def put_file(self, path, mode, object_id):
        text = self.blob(object_id)
        if mode == "120000":
            text = b"link " + text
        if path in self.files:
            self.write_node(path, b"file", b"change", properties=node_properties(mode), text=text)
            return
        if path in self.directories:
            self.delete_directory(path)
        self.add_directories(path)
        self.write_node(path, b"file", b"add", properties=node_properties(mode), text=text)
        self.files.add(path)
        self.directories[self.parent_of(path)] += 1

    def changes(self, parent, commit):
        """The commit's changes against its first parent (or the empty tree), deletions first."""
        command = ["git", "-C", self.git_dir, "diff-tree", "-r", "--no-renames", "--raw", "-z"]
        command += ["--root", commit] if parent is None else [parent, commit]
        fields = subprocess.run(command, check=True, stdout=subprocess.PIPE).stdout.split(b"\0")
        if parent is None:
            fields = fields[1:]
        deletions, puts = [], []
        for header, raw_path in zip(fields[0::2], fields[1::2]):
            old_mode, new_mode, _, new_id, status = header.decode().lstrip(":").split()
            path = "trunk/" + raw_path.decode("utf-8", "surrogateescape")
            # A type change (file to link, file to submodule) is a deletion, then an add where the new kind has one.
            if status in ("D", "T") or (status == "M" and "160000" in (old_mode, new_mode)):
                if old_mode != "160000":
                    deletions.append(path)
            if status != "D" and new_mode != "160000":
                puts.append((path, new_mode, new_id))
        return deletions, puts

    def revision(self, number, commit, parent):
        info = subprocess.run(
            ["git", "-C", self.git_dir, "show", "-s", "--format=%an%x00%ct%x00%B", commit],
            check=True,
            stdout=subprocess.PIPE,
        ).stdout
        author, seconds, message = info.split(b"\0", 2)
        date = time.strftime("%Y-%m-%dT%H:%M:%S.000000Z", time.gmtime(int(seconds))).encode()
        properties = revision_properties(author, date, message.rstrip(b"\n"))
        self.out.write(
            b"Revision-number: %d\nProp-content-length: %d\nContent-length: %d\n\n%s\n"
            % (number, len(properties), len(properties), properties)
        )
        if number == 1:
            self.write_node("trunk", b"dir", b"add", properties=b"PROPS-END\n")
        deletions, puts = self.changes(parent, commit)
        for path in deletions:
            if path in self.files:
                self.delete_file(path)
        for path, mode, object_id in puts:
            self.put_file(path, mode, object_id)


def main():
    git_dir, ref = sys.argv[1], sys.argv[2]
    limit = int(sys.argv[3]) if len(sys.argv) > 3 else None
    commits = subprocess.run(
        ["git", "-C", git_dir, "rev-list", "--first-parent", "--reverse", ref], check=True, stdout=subprocess.PIPE
    ).stdout.split()
    commits = [commit.decode() for commit in commits]
    if limit is not None:
        commits = commits[-limit:]
    out = sys.stdout.buffer
    out.write(b"SVN-fs-dump-format-version: 2\n\nUUID: 0a699000-0000-4000-8000-00000000a699\n\n")
    converter = Converter(git_dir, out)
    # The oldest kept commit is written whole: its changes against nothing.
    parent = None
    for number, commit in enumerate(commits, start=1):
        converter.revision(number, commit, parent)
        parent = commit
    out.flush()


if __name__ == "__main__":
    main()
