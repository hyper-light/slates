"""Check that each doc differs from its index copy only by the fills: tokens put back, words compared in order."""
import ast, re, subprocess, sys

source = open(sys.argv[1], encoding="utf-8").read()
fills = ast.literal_eval(re.search(r"FILLS = (\{.*?\n\})", source, re.S).group(1))

def words(text):
  return [word for word in re.split(r"[\s>]+", text) if word]

ok = True
for path, pairs in fills.items():
  now = open(path, encoding="utf-8").read()
  before = subprocess.run(["git", "show", f":{path}"], capture_output=True, text=True, check=True).stdout
  now_words = " ".join(words(now))
  for token, fill in pairs.items():
    fill_words = " ".join(words(fill))
    if now_words.count(fill_words) != 1:
      print(f"{path}: fill for {token} found {now_words.count(fill_words)} times"); ok = False
    now_words = now_words.replace(fill_words, token)
  same = now_words == " ".join(words(before))
  print(f"{path}: words equal to the index copy with tokens put back: {same}")
  ok = ok and same
sys.exit(0 if ok else 1)
