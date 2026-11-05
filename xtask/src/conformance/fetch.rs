//! The three suites' sources, fetched from pinned upstream commits and verified by SHA-256 before
//! they are built with the system `cc` (Part 6 "Conformance": pjdfstest, fsx, fsstress). They are
//! not vendored: fsx is Apple Public Source License 2.0 and fsstress is GPL-2.0, and a change that
//! adds licences to this MIT tree is Ada's to make; the pins here (URL at a commit, digest,
//! licence) make every build reproducible on this laptop and in the CI lane alike, and a digest
//! mismatch is a loud refusal before any compile. Nothing is installed: the binaries live in the
//! run's scratch directory.
//!
//! fsx builds unchanged with `-include time.h` (the FreeBSD copy calls `clock_gettime` without
//! including `<time.h>`; Apple clang refuses the implicit declaration). fsstress needs the build
//! environment LTP's `configure` would generate — a `config.h` and an empty `lapi/fcntl.h` — which
//! the harness writes as shim headers; on Darwin the shim also aliases the LFS names (`off64_t`,
//! `stat64`, `lseek64`, `readdir64`) to the native 64-bit ones and defines `O_DIRECT` as 0, and the
//! run disables the two direct-I/O operations (`-f dread=0 -f dwrite=0`) rather than run them
//! buffered under a false name. pjdfstest's `configure.ac` is reproduced by the harness's own
//! probes (one link test per `AC_CHECK_FUNC`, one compile test per header and struct member), so
//! no autotools are needed on any host.

use std::path::{Path, PathBuf};
use std::process::Command;

use slates_conformance::capability::HostOs;

use super::{create_dir, tool_on_path, write_file};
use crate::Failure;

/// A pinned upstream file.
pub(crate) struct Pin {
  /// The file name in the scratch directory.
  pub(crate) name: &'static str,
  /// The URL at the pinned commit.
  pub(crate) url: &'static str,
  /// The SHA-256 of the bytes, hexadecimal (computed 2026-09-14 from the fetched file).
  pub(crate) sha256: &'static str,
  /// The licence the upstream file carries.
  pub(crate) license: &'static str,
  /// The upstream, for the record.
  pub(crate) upstream: &'static str,
}

/// Format: the FreeBSD fsx at `freebsd-src` main `42c69445ca336b13e27e3e5960ace344c64ae0eb`.
pub(crate) const FSX_C: Pin = Pin {
  name: "fsx.c",
  url: "https://raw.githubusercontent.com/freebsd/freebsd-src/42c69445ca336b13e27e3e5960ace344c64ae0eb/tools/regression/fsx/fsx.c",
  sha256: "b064208bec8519e80038ee1da8cb9c0f7c512a3242bbf4c06809a88ce15ae019",
  license: "APSL 2.0",
  upstream: "freebsd/freebsd-src@42c6944 tools/regression/fsx/fsx.c",
};

/// Format: LTP's fsstress, at `linux-test-project/ltp` master `6af38cf1e7ab6ba42e261f739bcdbae75fd8b159`
/// (every fsstress file below is pinned at that commit).
pub(crate) const FSSTRESS_C: Pin = Pin {
  name: "fsstress.c",
  url: "https://raw.githubusercontent.com/linux-test-project/ltp/6af38cf1e7ab6ba42e261f739bcdbae75fd8b159/testcases/kernel/fs/fsstress/fsstress.c",
  sha256: "9a80bbe1f1ad933845b9272b5057776e74923503bbb6f392bf1e135c1744d644",
  license: "GPL-2.0",
  upstream: "linux-test-project/ltp@6af38cf testcases/kernel/fs/fsstress/fsstress.c",
};
/// Format: fsstress's own `global.h`.
const FSSTRESS_GLOBAL_H: Pin = Pin {
  name: "global.h",
  url: "https://raw.githubusercontent.com/linux-test-project/ltp/6af38cf1e7ab6ba42e261f739bcdbae75fd8b159/testcases/kernel/fs/fsstress/global.h",
  sha256: "decedb7939fa723932053020c5fc05482731706460d6bbcb87586c0310e5fc48",
  license: "GPL-2.0",
  upstream: "linux-test-project/ltp@6af38cf testcases/kernel/fs/fsstress/global.h",
};
/// Format: fsstress's `xfscompat.h` (the `-DNO_XFS` build's stand-in for the XFS headers).
const FSSTRESS_XFSCOMPAT_H: Pin = Pin {
  name: "xfscompat.h",
  url: "https://raw.githubusercontent.com/linux-test-project/ltp/6af38cf1e7ab6ba42e261f739bcdbae75fd8b159/testcases/kernel/fs/fsstress/xfscompat.h",
  sha256: "74be3b8d5276ba4b16bc94ec491266db0c673884aa401abb31db6815e429f668",
  license: "GPL-2.0",
  upstream: "linux-test-project/ltp@6af38cf testcases/kernel/fs/fsstress/xfscompat.h",
};
/// Format: LTP's `tst_common.h`, the one library header fsstress includes.
const FSSTRESS_TST_COMMON_H: Pin = Pin {
  name: "tst_common.h",
  url: "https://raw.githubusercontent.com/linux-test-project/ltp/6af38cf1e7ab6ba42e261f739bcdbae75fd8b159/include/tst_common.h",
  sha256: "35dc31d81863a89bc280e89b36c1248f77e02428cf0a96e93c67e5543e0b265e",
  license: "GPL-2.0",
  upstream: "linux-test-project/ltp@6af38cf include/tst_common.h",
};

/// Format: the commit of `billziss-gh/secfs.test` WinFsp v2.1 pins as its `ext/test` submodule
/// (`winfsp/winfsp@ddca7bd`, `git ls-tree HEAD ext/test`): the WinFsp fsx port and winfstest, as WinFsp's own CI runs
/// them (`tools/run-tests.bat`, `memfs-x64-disk-fsx`, `memfs-x64-disk-winfstest`).
pub(crate) const SECFS_TEST_COMMIT: &str = "6ac65cda46abc2be39c7b137debf9521052edbaf";

/// Format: the WinFsp fsx port (Apple's fsx with a Windows build: `fstools/src/fsx/fsx.c`).
#[cfg(windows)]
const WINFSP_FSX_C: Pin = Pin {
  name: "fsx.c",
  url: "https://raw.githubusercontent.com/billziss-gh/secfs.test/6ac65cda46abc2be39c7b137debf9521052edbaf/fstools/src/fsx/fsx.c",
  sha256: "440742597d18b69edfea6ca49fd1905fbad4926953e446957e2ed1377ebe584c",
  license: "APSL 2.0",
  upstream: "billziss-gh/secfs.test@6ac65cd fstools/src/fsx/fsx.c",
};
/// Format: the port's `getopt` for MSVC (`fstools/src/fsx/win/getopt.c`).
#[cfg(windows)]
const WINFSP_FSX_GETOPT_C: Pin = Pin {
  name: "getopt.c",
  url: "https://raw.githubusercontent.com/billziss-gh/secfs.test/6ac65cda46abc2be39c7b137debf9521052edbaf/fstools/src/fsx/win/getopt.c",
  sha256: "bf89b68bef0b8d4dd590f3a0233ec1d9492bd4f243a0e776c913b678af5027cd",
  license: "AT&T public domain getopt",
  upstream: "billziss-gh/secfs.test@6ac65cd fstools/src/fsx/win/getopt.c",
};
/// Format: its header.
#[cfg(windows)]
const WINFSP_FSX_GETOPT_H: Pin = Pin {
  name: "getopt.h",
  url: "https://raw.githubusercontent.com/billziss-gh/secfs.test/6ac65cda46abc2be39c7b137debf9521052edbaf/fstools/src/fsx/win/getopt.h",
  sha256: "dd3d66a1ae81ed1b972450a0a92849654f3c1b05a631b36c7e7b83d160f61ef1",
  license: "AT&T public domain getopt",
  upstream: "billziss-gh/secfs.test@6ac65cd fstools/src/fsx/win/getopt.h",
};
/// Format: the port's `mmap` over file mappings (`fstools/src/fsx/win/mman.c`, mman-win32).
#[cfg(windows)]
const WINFSP_FSX_MMAN_C: Pin = Pin {
  name: "mman.c",
  url: "https://raw.githubusercontent.com/billziss-gh/secfs.test/6ac65cda46abc2be39c7b137debf9521052edbaf/fstools/src/fsx/win/mman.c",
  sha256: "013f0bbef39562ba2c9c6015d79bdc38d990d05c4e824864e6f466f5192b2792",
  license: "MIT",
  upstream: "billziss-gh/secfs.test@6ac65cd fstools/src/fsx/win/mman.c",
};
/// Format: its header.
#[cfg(windows)]
const WINFSP_FSX_MMAN_H: Pin = Pin {
  name: "mman.h",
  url: "https://raw.githubusercontent.com/billziss-gh/secfs.test/6ac65cda46abc2be39c7b137debf9521052edbaf/fstools/src/fsx/win/mman.h",
  sha256: "5a09cb0e39bf837b1d61eaf68e7d381b9943eef910fa94cc53a745be2ed73bf9",
  license: "MIT",
  upstream: "billziss-gh/secfs.test@6ac65cd fstools/src/fsx/win/mman.h",
};
/// Format: the port's `random` (BSD's, `fstools/src/fsx/win/random.c`).
#[cfg(windows)]
const WINFSP_FSX_RANDOM_C: Pin = Pin {
  name: "random.c",
  url: "https://raw.githubusercontent.com/billziss-gh/secfs.test/6ac65cda46abc2be39c7b137debf9521052edbaf/fstools/src/fsx/win/random.c",
  sha256: "a6f77f3282b5f38692210bee1773128038a2110d0698cee3b016428690233566",
  license: "BSD-3-Clause",
  upstream: "billziss-gh/secfs.test@6ac65cd fstools/src/fsx/win/random.c",
};
/// Format: winfstest's driver, one Win32 call per process (`winfstest/winfstest.c`).
#[cfg(windows)]
pub(crate) const WINFSTEST_C: Pin = Pin {
  name: "winfstest.c",
  url: "https://raw.githubusercontent.com/billziss-gh/secfs.test/6ac65cda46abc2be39c7b137debf9521052edbaf/winfstest/winfstest.c",
  sha256: "7a1ddf128b02a38145a1f3230fed5a64ffda6dc64c096b8a67da89be17632c94",
  license: "BSD-3-Clause",
  upstream: "billziss-gh/secfs.test@6ac65cd winfstest/winfstest.c",
};
/// Format: the driver's table of Win32 error names (`winfstest/winerror.i`, included by `winfstest.c`).
#[cfg(windows)]
const WINFSTEST_WINERROR_I: Pin = Pin {
  name: "winerror.i",
  url: "https://raw.githubusercontent.com/billziss-gh/secfs.test/6ac65cda46abc2be39c7b137debf9521052edbaf/winfstest/winerror.i",
  sha256: "87cabf8c439eaf8f5b9a0cd97c2cd1010a6d0bf526013dcda8eb20faa603a22b",
  license: "BSD-3-Clause",
  upstream: "billziss-gh/secfs.test@6ac65cd winfstest/winerror.i",
};

/// Format: the pjdfstest commit the tarball is pinned at.
pub(crate) const PJDFSTEST_COMMIT: &str = slates_conformance::pjdfstest::COMMIT;

/// Format: pjdfstest's source tarball at the pinned commit (GitHub's archive; its digest is the
/// tarball's, so a regenerated archive with different compression is a loud mismatch to re-pin).
pub(crate) const PJDFSTEST_TARBALL: Pin = Pin {
  name: "pjdfstest.tar.gz",
  url: "https://github.com/pjd/pjdfstest/archive/85a8aea9e685999ef0540392fd80535f873d7ff7.tar.gz",
  sha256: "2005cdd83b76204177cf136792b1f2058a7418b4fc5b203f51274a698547d754",
  license: "BSD-2-Clause",
  upstream: "pjd/pjdfstest@85a8aea",
};

fn sha256_hex(bytes: &[u8]) -> String {
  let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, bytes);
  digest.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// Shape: curl's retries of a fetch that fails transiently, passed with `--retry-all-errors` so a connection reset
/// counts (plain `--retry` takes only timeouts and some HTTP statuses; CI run 37162029555 lost the fsstress lane to
/// one `curl: (35) Recv failure: Connection reset by peer`). curl waits a second before the first retry and doubles
/// each wait, so five retries wait at most 31 s, and the digest check after the fetch still refuses any wrong bytes.
/// The guest image's build script uses the same count (`ci/guest/build-exercisers.sh`, asserted below).
pub(crate) const FETCH_RETRIES: u32 = 5;

/// Fetches a pin into `dir` (or reuses a file already there with the right digest) and verifies it.
pub(crate) fn fetch(pin: &Pin, dir: &Path) -> Result<PathBuf, Failure> {
  create_dir(dir)?;
  let path = dir.join(pin.name);
  if let Ok(bytes) = std::fs::read(&path)
    && sha256_hex(&bytes) == pin.sha256
  {
    return Ok(path);
  }
  if !tool_on_path("curl") {
    return Err(Failure(format!("curl is needed to fetch {}", pin.upstream)));
  }
  let output = Command::new("curl")
    .args(["-fsSL", "--retry"])
    .arg(FETCH_RETRIES.to_string())
    .args(["--retry-all-errors", "-o"])
    .arg(&path)
    .arg(pin.url)
    .output()?;
  if !output.status.success() {
    return Err(Failure(format!(
      "fetching {} failed: {}",
      pin.url,
      String::from_utf8_lossy(&output.stderr)
    )));
  }
  let bytes = std::fs::read(&path)?;
  let found = sha256_hex(&bytes);
  if found != pin.sha256 {
    #[allow(clippy::disallowed_methods)]
    // the development tool's own scratch: a refused fetch leaves nothing
    let _ = std::fs::remove_file(&path);
    return Err(Failure(format!(
      "{} digest mismatch: pinned {}, fetched {found}; re-pin deliberately or refuse",
      pin.upstream, pin.sha256
    )));
  }
  Ok(path)
}

/// Runs `cc` with the given arguments in `dir`, failing with its diagnostics.
fn cc(dir: &Path, args: &[&str]) -> Result<(), Failure> {
  let output = Command::new("cc").args(args).current_dir(dir).output()?;
  if output.status.success() {
    Ok(())
  } else {
    Err(Failure(format!(
      "cc {} failed:\n{}",
      args.join(" "),
      String::from_utf8_lossy(&output.stderr)
    )))
  }
}

/// A built exerciser: its binary and the note the record carries.
pub(crate) struct Built {
  pub(crate) binary: PathBuf,
  pub(crate) note: String,
}

/// Fetches and builds fsx.
#[cfg(not(windows))]
pub(crate) fn build_fsx(dir: &Path) -> Result<Built, Failure> {
  fetch(&FSX_C, dir)?;
  // `-include time.h` for `clock_gettime` (the FreeBSD copy omits it); `-include stdint.h` for
  // `uintptr_t`, which fsx uses (lines 487/497) without including it — FreeBSD's `<sys/types.h>` pulls
  // it in transitively, Linux glibc's does not, so on Linux it is an "unknown type name" without this.
  cc(
    dir,
    &[
      "-O2", "-w", "-include", "time.h", "-include", "stdint.h", "-o", "fsx", FSX_C.name,
    ],
  )?;
  Ok(Built {
    binary: dir.join("fsx"),
    note: format!(
      "fsx: {} ({}), sha256 {}, built `cc -O2 -w -include time.h -include stdint.h` (source unchanged)",
      FSX_C.upstream, FSX_C.license, FSX_C.sha256
    ),
  })
}

/// Format: the flag that bounds fsx's file length: `-l` in FreeBSD's fsx, `-F` in the WinFsp port (Apple's fsx,
/// whose `-l` names an XILog file instead).
#[cfg(not(windows))]
pub(crate) const FSX_LENGTH_FLAG: &str = "-l";
/// Format: the flag that bounds fsx's file length in the WinFsp port (`-F flen`, its usage text).
#[cfg(windows)]
pub(crate) const FSX_LENGTH_FLAG: &str = "-F";

/// MSVC's `cl.exe` for this process's own architecture, with the environment the Visual Studio installation gives it
/// (`INCLUDE`, `LIB`, `PATH`), found as the `cc` crate finds it for every build script, so no developer prompt is needed.
#[cfg(windows)]
fn msvc_cl() -> Result<Command, Failure> {
  let target = format!("{}-pc-windows-msvc", std::env::consts::ARCH);
  cc::windows_registry::find_tool(&target, "cl.exe")
    .map(|tool| tool.to_command())
    .ok_or_else(|| Failure(format!("no MSVC cl.exe for {target} on this host")))
}

/// Runs `cl` with the given arguments in `dir`, failing with its diagnostics (cl prints them on stdout).
#[cfg(windows)]
fn cl(dir: &Path, args: &[&str]) -> Result<(), Failure> {
  let output = msvc_cl()?.args(args).current_dir(dir).output()?;
  if output.status.success() {
    Ok(())
  } else {
    Err(Failure(format!(
      "cl {} failed:\n{}{}",
      args.join(" "),
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    )))
  }
}

/// Fetches and builds the WinFsp fsx port with MSVC, as its `Nmakefile` does (`cl /MT /W2 fsx.c win/*.c ntdll.lib`):
/// the port's own Windows shims (`win/`), the source unchanged.
#[cfg(windows)]
pub(crate) fn build_fsx(dir: &Path) -> Result<Built, Failure> {
  fetch(&WINFSP_FSX_C, dir)?;
  let win = dir.join("win");
  for pin in [
    &WINFSP_FSX_GETOPT_C,
    &WINFSP_FSX_GETOPT_H,
    &WINFSP_FSX_MMAN_C,
    &WINFSP_FSX_MMAN_H,
    &WINFSP_FSX_RANDOM_C,
  ] {
    fetch(pin, &win)?;
  }
  let args = [
    "/nologo",
    "/MT",
    "/W2",
    "fsx.c",
    "win\\getopt.c",
    "win\\mman.c",
    "win\\random.c",
    "ntdll.lib",
    "/Fe:fsx.exe",
    "/link",
    "/subsystem:console",
  ];
  cl(dir, &args)?;
  Ok(Built {
    binary: dir.join("fsx.exe"),
    note: format!(
      "fsx: the WinFsp port, {} ({}), sha256 {}, with its win/ shims at the same commit, built `cl {}` (source unchanged)",
      WINFSP_FSX_C.upstream,
      WINFSP_FSX_C.license,
      WINFSP_FSX_C.sha256,
      args.join(" ")
    ),
  })
}

/// Fetches and builds winfstest's driver with MSVC, as its `Nmakefile` does (`cl /MT /W2 winfstest.c advapi32.lib`).
#[cfg(windows)]
pub(crate) fn build_winfstest(dir: &Path) -> Result<Built, Failure> {
  fetch(&WINFSTEST_C, dir)?;
  fetch(&WINFSTEST_WINERROR_I, dir)?;
  let args = [
    "/nologo",
    "/MT",
    "/W2",
    "winfstest.c",
    "advapi32.lib",
    "/Fe:winfstest.exe",
    "/link",
    "/subsystem:console",
  ];
  cl(dir, &args)?;
  Ok(Built {
    binary: dir.join("winfstest.exe"),
    note: format!(
      "winfstest: {} ({}), sha256 {}, with winerror.i at the same commit, built `cl {}` (source unchanged)",
      WINFSTEST_C.upstream,
      WINFSTEST_C.license,
      WINFSTEST_C.sha256,
      args.join(" ")
    ),
  })
}

/// The shim `config.h` LTP's configure would generate, per host.
fn fsstress_config_h(os: HostOs) -> &'static str {
  match os {
    HostOs::Macos => concat!(
      "/* slates conformance harness: the build environment LTP's configure generates, for Darwin */\n",
      "#define _GNU_SOURCE 1\n",
      "#include <signal.h>\n#include <sys/types.h>\n#include <sys/stat.h>\n#include <unistd.h>\n",
      "#include <fcntl.h>\n#include <dirent.h>\n",
      "#define off64_t off_t\n#define stat64 stat\n#define lstat64 lstat\n#define fstat64 fstat\n",
      "#define lseek64 lseek\n#define truncate64 truncate\n#define ftruncate64 ftruncate\n",
      "#define readdir64 readdir\n#define dirent64 dirent\n",
      "#ifndef O_DIRECT\n#define O_DIRECT 0\n#endif\n"
    ),
    HostOs::Linux | HostOs::Windows => concat!(
      "/* slates conformance harness: the build environment LTP's configure generates, for Linux */\n",
      "#define _GNU_SOURCE 1\n#define _LARGEFILE64_SOURCE 1\n"
    ),
  }
}

/// A built fsstress, with the operations the host cannot run honestly.
pub(crate) struct BuiltFsstress {
  pub(crate) built: Built,
  pub(crate) disabled_operations: Vec<String>,
}

/// Format: the compiler flags fsstress is built with on every lane (the shim's `config.h` included first).
pub(crate) const FSSTRESS_CFLAGS: &[&str] = &[
  "-O2",
  "-w",
  "-DNO_XFS",
  "-D_GNU_SOURCE",
  "-include",
  "shim/config.h",
  "-I.",
  "-Ishim",
];

/// Fetches the pinned fsstress sources into `dir` and writes the shim headers for `os`, without compiling:
/// what a container lane compiles inside its own Linux (`crate::conformance::container`).
pub(crate) fn stage_fsstress(dir: &Path, os: HostOs) -> Result<(), Failure> {
  for pin in [
    &FSSTRESS_C,
    &FSSTRESS_GLOBAL_H,
    &FSSTRESS_XFSCOMPAT_H,
    &FSSTRESS_TST_COMMON_H,
  ] {
    fetch(pin, dir)?;
  }
  let shim = dir.join("shim");
  create_dir(&shim.join("lapi"))?;
  write_file(&shim.join("config.h"), fsstress_config_h(os).as_bytes())?;
  write_file(
    &shim.join("lapi").join("fcntl.h"),
    b"/* slates conformance harness: LTP's lapi/fcntl.h is not needed with the system fcntl.h */\n",
  )
}

/// Fetches and builds fsstress with the shim headers.
pub(crate) fn build_fsstress(dir: &Path, os: HostOs) -> Result<BuiltFsstress, Failure> {
  stage_fsstress(dir, os)?;
  let mut args = FSSTRESS_CFLAGS.to_vec();
  args.extend(["-o", "fsstress", FSSTRESS_C.name]);
  cc(dir, &args)?;
  let disabled_operations = match os {
    HostOs::Macos => vec!["dread".to_owned(), "dwrite".to_owned()],
    HostOs::Linux | HostOs::Windows => Vec::new(),
  };
  Ok(BuiltFsstress {
    built: Built {
      binary: dir.join("fsstress"),
      note: format!(
        "fsstress: {} ({}), sha256 {}, built `cc -O2 -w -DNO_XFS -D_GNU_SOURCE -include shim/config.h` over the harness's shim config.h{}",
        FSSTRESS_C.upstream,
        FSSTRESS_C.license,
        FSSTRESS_C.sha256,
        if os == HostOs::Macos {
          " (Darwin: LFS names aliased to the native 64-bit ones, O_DIRECT defined 0, so dread/dwrite are disabled with -f)"
        } else {
          ""
        }
      ),
    },
    disabled_operations,
  })
}

/// Whether a probe source compiles (and links, unless `compile_only`).
fn probe(dir: &Path, source: &str, compile_only: bool) -> Result<bool, Failure> {
  let path = dir.join("probe.c");
  write_file(&path, source.as_bytes())?;
  let mut args = vec!["-std=gnu17", "-w", "-o", "probe.out"];
  if compile_only {
    args.push("-c");
  }
  args.push("probe.c");
  let output = Command::new("cc").args(&args).current_dir(dir).output()?;
  Ok(output.status.success())
}

/// The `config.h` `configure` would write on this host, from the harness's own probes.
fn pjdfstest_config_h(dir: &Path) -> Result<String, Failure> {
  let mut config = slates_conformance::pjdfstest::config_head();
  for probe_case in slates_conformance::pjdfstest::probes() {
    if probe(dir, &probe_case.source, probe_case.compile_only)? {
      config.push_str(&probe_case.define);
    }
  }
  Ok(config)
}

/// A built pjdfstest tree: the extracted source root (`tests/` inside it) and the binary.
pub(crate) struct PjdfstestTree {
  pub(crate) root: PathBuf,
  pub(crate) binary: PathBuf,
  pub(crate) note: String,
}

/// Fetches, unpacks and builds pjdfstest without autotools.
/// Fetches the pinned pjdfstest tarball into `dir` and unpacks it, without building: the source root.
pub(crate) fn stage_pjdfstest(dir: &Path) -> Result<PathBuf, Failure> {
  let tarball = fetch(&PJDFSTEST_TARBALL, dir)?;
  let root = dir.join(format!("pjdfstest-{PJDFSTEST_COMMIT}"));
  if !root.join("pjdfstest.c").is_file() {
    let output = Command::new("tar")
      .arg("xzf")
      .arg(&tarball)
      .current_dir(dir)
      .output()?;
    if !output.status.success() {
      return Err(Failure(format!(
        "unpacking pjdfstest: {}",
        String::from_utf8_lossy(&output.stderr)
      )));
    }
  }
  Ok(root)
}

/// Fetches, unpacks and builds pjdfstest for this host.
pub(crate) fn build_pjdfstest(dir: &Path) -> Result<PjdfstestTree, Failure> {
  let root = stage_pjdfstest(dir)?;
  let config = pjdfstest_config_h(&root)?;
  write_file(&root.join("config.h"), config.as_bytes())?;
  cc(
    &root,
    &["-O2", "-w", "-I.", "-o", "pjdfstest", "pjdfstest.c"],
  )?;
  let defined: Vec<&str> = config
    .lines()
    .filter_map(|l| l.strip_prefix("#define HAVE_"))
    .filter_map(|l| l.split_whitespace().next())
    .collect();
  Ok(PjdfstestTree {
    binary: root.join("pjdfstest"),
    root,
    note: format!(
      "pjdfstest: {} ({}), tarball sha256 {}, built `cc -O2 -w -I. pjdfstest.c` over a config.h the harness probed (HAVE_: {})",
      PJDFSTEST_TARBALL.upstream,
      PJDFSTEST_TARBALL.license,
      PJDFSTEST_TARBALL.sha256,
      defined.join(" ")
    ),
  })
}

#[cfg(test)]
mod guest_tests {
  use super::*;

  /// AC-9.7 / AUD-29-78 (doc truth: the guest's exercisers are the harness's). Do: read the guest image's build
  /// script. Expect: every pin's URL and SHA-256, fsx's and fsstress's compiler flags and the Linux shim headers'
  /// text appear in it exactly, so the guest's fsx and fsstress legs run the very programs this harness runs.
  #[test]
  fn the_guest_image_builds_the_exercisers_the_harness_pins() {
    let script = std::fs::read_to_string(
      std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../ci/guest/build-exercisers.sh"),
    )
    .unwrap_or_default();
    assert!(
      script.contains(&format!("--retry {FETCH_RETRIES} --retry-all-errors")),
      "the guest image fetches with the harness's retries"
    );
    for pin in [
      &FSX_C,
      &FSSTRESS_C,
      &FSSTRESS_GLOBAL_H,
      &FSSTRESS_XFSCOMPAT_H,
      &FSSTRESS_TST_COMMON_H,
      &PJDFSTEST_TARBALL,
    ] {
      assert!(
        script.contains(&format!("fetch {} {} {}", pin.name, pin.url, pin.sha256)),
        "{} at its pin and digest",
        pin.name
      );
    }
    assert!(
      script.contains("cc -O2 -w -include time.h -include stdint.h -o /usr/local/bin/fsx fsx.c")
    );
    let fsstress_flags = FSSTRESS_CFLAGS.join(" ");
    assert!(script.contains(&format!(
      "cc {fsstress_flags} -o /usr/local/bin/fsstress fsstress.c"
    )));
    assert!(
      script.contains(&format!("/guest/pjdfstest-{PJDFSTEST_COMMIT}")),
      "the pinned pjdfstest tree is unpacked where the guest reads it"
    );
    for line in fsstress_config_h(HostOs::Linux).lines() {
      let in_script = line.replace('\'', "'\"'\"'");
      assert!(
        script.contains(&in_script),
        "the shim config.h line {line:?}"
      );
    }
  }
}
