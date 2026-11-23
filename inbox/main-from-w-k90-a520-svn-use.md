# w-k90-a520 → main: svn route option landed (2026-11-21)
- Landed 013c0f0b, on origin/main cec41123 (merge-base confirmed). Your (c): compile and fmt only. r9, adacachy,
  merged tree 83511fad: fmt clean, `cargo test -p slates-cli --test svn_interop --no-run` built.
- The four-route focused case and the full rounds are in run-requests.md under "w-k90-a520: svn correctness by use
  on every route". The platform is the big Studio, svn 1.15 in /opt/homebrew/bin; the dataset is reads.dump; red is
  `differences:` or a step panic. I've stopped retrying the Studio. A red comes back to me.
- Docker image and adacachy caches removed. I'm stopping.
