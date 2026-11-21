"""Rewrites author/committer/tagger lines of a fast-import stream (git refuses hg's free-form authors), passing
every `data N` block through untouched, so file contents and their identities are unchanged."""
import re, sys
src, dst = sys.stdin.buffer, sys.stdout.buffer
ident = re.compile(rb'^(author|committer|tagger) .*?(\d+ [+-]\d{4})$')
while True:
  line = src.readline()
  if not line:
    break
  if line.startswith(b'data '):
    n = int(line[5:].strip())
    dst.write(line); dst.write(src.read(n)); continue
  m = ident.match(line.rstrip(b'\n'))
  if m:
    line = m.group(1) + b' hg <hg@example.invalid> ' + m.group(2) + b'\n'
  dst.write(line)
