#!/bin/sh
# Builds the conformance exercisers into the live-guest image (AC-9.7, AUD-29-78: the guest's fsx, fsstress and
# pjdfstest legs): the very sources the harness fetches (`xtask/src/conformance/fetch.rs`, the same pins and SHA-256s), the
# same compiler flags, installed at /usr/local/bin for the guest, which runs this container's root over 9p.
# `xtask/src/conformance/fetch.rs`'s test asserts every pin, digest and flag here equals the harness's, so the two
# never drift. A digest that does not match fails the image build.
set -eu
work=/guest/exercisers-build
mkdir -p "$work"
cd "$work"
fetch() {
  curl -fsSL --retry 5 --retry-all-errors "$2" -o "$1"
  echo "$3  $1" | sha256sum -c -
}
fetch fsx.c https://raw.githubusercontent.com/freebsd/freebsd-src/42c69445ca336b13e27e3e5960ace344c64ae0eb/tools/regression/fsx/fsx.c b064208bec8519e80038ee1da8cb9c0f7c512a3242bbf4c06809a88ce15ae019
cc -O2 -w -include time.h -include stdint.h -o /usr/local/bin/fsx fsx.c
fetch fsstress.c https://raw.githubusercontent.com/linux-test-project/ltp/6af38cf1e7ab6ba42e261f739bcdbae75fd8b159/testcases/kernel/fs/fsstress/fsstress.c 9a80bbe1f1ad933845b9272b5057776e74923503bbb6f392bf1e135c1744d644
fetch global.h https://raw.githubusercontent.com/linux-test-project/ltp/6af38cf1e7ab6ba42e261f739bcdbae75fd8b159/testcases/kernel/fs/fsstress/global.h decedb7939fa723932053020c5fc05482731706460d6bbcb87586c0310e5fc48
fetch xfscompat.h https://raw.githubusercontent.com/linux-test-project/ltp/6af38cf1e7ab6ba42e261f739bcdbae75fd8b159/testcases/kernel/fs/fsstress/xfscompat.h 74be3b8d5276ba4b16bc94ec491266db0c673884aa401abb31db6815e429f668
fetch tst_common.h https://raw.githubusercontent.com/linux-test-project/ltp/6af38cf1e7ab6ba42e261f739bcdbae75fd8b159/include/tst_common.h 35dc31d81863a89bc280e89b36c1248f77e02428cf0a96e93c67e5543e0b265e
mkdir -p shim/lapi
printf '/* slates conformance harness: the build environment LTP'"'"'s configure generates, for Linux */\n#define _GNU_SOURCE 1\n#define _LARGEFILE64_SOURCE 1\n' > shim/config.h
printf '/* slates conformance harness: LTP'"'"'s lapi/fcntl.h is not needed with the system fcntl.h */\n' > shim/lapi/fcntl.h
cc -O2 -w -DNO_XFS -D_GNU_SOURCE -include shim/config.h -I. -Ishim -o /usr/local/bin/fsstress fsstress.c
fetch pjdfstest.tar.gz https://github.com/pjd/pjdfstest/archive/85a8aea9e685999ef0540392fd80535f873d7ff7.tar.gz 2005cdd83b76204177cf136792b1f2058a7418b4fc5b203f51274a698547d754
# The pinned tree, unpacked where the guest reads it (over 9p, read-only): the guest copies it into its RAM and
# builds pjdfstest there from the harness's probes, as the oci-linux container does.
tar xzf pjdfstest.tar.gz -C /guest
chmod -R a+rX /guest/pjdfstest-85a8aea9e685999ef0540392fd80535f873d7ff7
cd /
rm -rf "$work"
