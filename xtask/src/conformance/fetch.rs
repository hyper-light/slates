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

use sha2::{Digest, Sha256};
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

/// Format: the pjdfstest commit the tarball is pinned at.
pub(crate) const PJDFSTEST_COMMIT: &str = "85a8aea9e685999ef0540392fd80535f873d7ff7";

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
  let digest = Sha256::digest(bytes);
  digest.iter().map(|b| format!("{b:02x}")).collect()
}

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
    .args(["-fsSL", "-o"])
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
    let _ = Command::new("rm").arg("-f").arg(&path).output();
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
pub(crate) fn build_fsx(dir: &Path) -> Result<Built, Failure> {
  fetch(&FSX_C, dir)?;
  cc(
    dir,
    &["-O2", "-w", "-include", "time.h", "-o", "fsx", FSX_C.name],
  )?;
  Ok(Built {
    binary: dir.join("fsx"),
    note: format!(
      "fsx: {} ({}), sha256 {}, built `cc -O2 -w -include time.h` unchanged",
      FSX_C.upstream, FSX_C.license, FSX_C.sha256
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

/// Fetches and builds fsstress with the shim headers.
pub(crate) fn build_fsstress(dir: &Path, os: HostOs) -> Result<BuiltFsstress, Failure> {
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
  )?;
  cc(
    dir,
    &[
      "-O2",
      "-w",
      "-DNO_XFS",
      "-D_GNU_SOURCE",
      "-include",
      "shim/config.h",
      "-I.",
      "-Ishim",
      "-o",
      "fsstress",
      FSSTRESS_C.name,
    ],
  )?;
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

/// Format: the functions `configure.ac` probes with `AC_CHECK_FUNC` (pjdfstest at the pinned
/// commit), each defining `HAVE_<NAME>` when it links.
const PJDFSTEST_FUNCTIONS: &[&str] = &[
  "bindat",
  "chflags",
  "chflagsat",
  "connectat",
  "faccessat",
  "fchflags",
  "fchmodat",
  "fchownat",
  "fstatat",
  "lchflags",
  "lchmod",
  "linkat",
  "lpathconf",
  "mkdirat",
  "mkfifoat",
  "mknodat",
  "openat",
  "posix_fallocate",
  "readlinkat",
  "renameat",
  "symlinkat",
  "utimensat",
];
/// Format: the headers `configure.ac` probes with `AC_CHECK_HEADERS`.
const PJDFSTEST_HEADERS: &[(&str, &str)] = &[
  ("sys/mkdev.h", "HAVE_SYS_MKDEV_H"),
  ("sys/sysmacros.h", "HAVE_SYS_SYSMACROS_H"),
];
/// Format: the `struct stat` members `configure.ac` probes with `AC_CHECK_MEMBERS`.
const PJDFSTEST_STAT_MEMBERS: &[(&str, &str)] = &[
  ("st_atim", "HAVE_STRUCT_STAT_ST_ATIM"),
  ("st_atimespec", "HAVE_STRUCT_STAT_ST_ATIMESPEC"),
  ("st_birthtim", "HAVE_STRUCT_STAT_ST_BIRTHTIM"),
  ("st_birthtime", "HAVE_STRUCT_STAT_ST_BIRTHTIME"),
  ("st_birthtimespec", "HAVE_STRUCT_STAT_ST_BIRTHTIMESPEC"),
  ("st_ctim", "HAVE_STRUCT_STAT_ST_CTIM"),
  ("st_ctimespec", "HAVE_STRUCT_STAT_ST_CTIMESPEC"),
  ("st_mtim", "HAVE_STRUCT_STAT_ST_MTIM"),
  ("st_mtimespec", "HAVE_STRUCT_STAT_ST_MTIMESPEC"),
];
/// Format: what `AC_USE_SYSTEM_EXTENSIONS` defines (the feature macros every platform honours or ignores).
const SYSTEM_EXTENSIONS: &str = "#define _ALL_SOURCE 1\n#define _DARWIN_C_SOURCE 1\n#define _GNU_SOURCE 1\n\
  #define _POSIX_PTHREAD_SEMANTICS 1\n#define _TANDEM_SOURCE 1\n#define __EXTENSIONS__ 1\n";

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
  let mut config =
    String::from("/* slates conformance harness: configure.ac's checks, probed by cc */\n");
  config.push_str(SYSTEM_EXTENSIONS);
  for function in PJDFSTEST_FUNCTIONS {
    // autoconf's `AC_CHECK_FUNC` shape: declare and *call* the function, so the link decides.
    // (Comparing its address with zero is folded by clang without ever linking the symbol.)
    let source = format!("char {function}();\nint main(void) {{ return {function}(); }}\n");
    if probe(dir, &source, false)? {
      config.push_str(&format!("#define HAVE_{} 1\n", function.to_uppercase()));
    }
  }
  for (header, macro_name) in PJDFSTEST_HEADERS {
    let source = format!("#include <{header}>\nint main(void) {{ return 0; }}\n");
    if probe(dir, &source, true)? {
      config.push_str(&format!("#define {macro_name} 1\n"));
    }
  }
  for (member, macro_name) in PJDFSTEST_STAT_MEMBERS {
    let source = format!(
      "{SYSTEM_EXTENSIONS}#include <sys/types.h>\n#include <sys/stat.h>\nint main(void) {{ struct stat s; (void)s.{member}; return 0; }}\n"
    );
    if probe(dir, &source, true)? {
      config.push_str(&format!("#define {macro_name} 1\n"));
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
pub(crate) fn build_pjdfstest(dir: &Path) -> Result<PjdfstestTree, Failure> {
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
