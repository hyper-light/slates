//! pjdfstest built without autotools (§6 conformance; Part 6): the checks pjdfstest's `configure.ac` makes, as C
//! probes a compiler answers, and the head of the `config.h` they fill. Shared by every leg that builds pjdfstest
//! — the harness on its host and in the `oci-linux` container (`xtask/src/conformance`), and the live guest
//! (`crates/server/tests/virtiofs.rs`, AUD-29-78) — so each builds the very configuration the others would on its
//! system.

/// Format: the pjdfstest commit every leg builds (the harness's tarball pin, `xtask/src/conformance/fetch.rs`).
pub const COMMIT: &str = "85a8aea9e685999ef0540392fd80535f873d7ff7";

/// Shape: the wall bound of one pjdfstest file; the longest (`rename/00.t`, dozens of cases each spawning a
/// process over loopback NFS) finishes in seconds, so a file past this has hung.
pub const FILE_BOUND_SECONDS: u64 = 300;

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

/// One of `configure.ac`'s checks as a C source: whether it must link (a function) or only compile (a header, a
/// `struct stat` member), and the line `config.h` gains when it does.
pub struct ConfigProbe {
  /// The C source.
  pub source: String,
  /// Whether it need only compile (a header, a member) rather than link (a function).
  pub compile_only: bool,
  /// The line `config.h` gains when it succeeds.
  pub define: String,
}

/// Every check `configure.ac` makes, as probes a compiler answers: on the harness's host, inside a container (the
/// `oci-linux` lane), or inside the live guest (`crates/server/tests/virtiofs.rs`), whose answers differ.
pub fn probes() -> Vec<ConfigProbe> {
  let mut probes = Vec::new();
  for function in PJDFSTEST_FUNCTIONS {
    // autoconf's `AC_CHECK_FUNC` shape: declare and *call* the function, so the link decides (comparing
    // its address with zero is folded by clang without ever linking the symbol). The `__stub_` guard is
    // autoconf's too and is load-bearing on Linux: glibc provides a *linkable* stub for some functions it
    // does not implement — `chflags` among them — that always fails `ENOSYS`, so a bare link test is a
    // false positive. glibc marks such a stub with `__stub_<name>` (or `__stub___<name>`) in
    // `<gnu/stubs.h>`, which `<limits.h>` pulls in; rejecting the probe when that macro is defined is
    // exactly what real `AC_CHECK_FUNC` does. Without it, `HAVE_CHFLAGS` was defined on Linux and
    // pjdfstest's `st_flags` block (guarded by it) failed to compile against a `struct stat` that has no
    // `st_flags` member. On macOS (real `chflags`, no stub) the guard passes and the function is detected.
    probes.push(ConfigProbe {
      source: format!(
        "#include <limits.h>\n\
         char {function}(void);\n\
         #if defined __stub_{function} || defined __stub___{function}\n\
         #error stub\n\
         #endif\n\
         int main(void) {{ return {function}(); }}\n"
      ),
      compile_only: false,
      define: format!("#define HAVE_{} 1\n", function.to_uppercase()),
    });
  }
  for (header, macro_name) in PJDFSTEST_HEADERS {
    probes.push(ConfigProbe {
      source: format!("#include <{header}>\nint main(void) {{ return 0; }}\n"),
      compile_only: true,
      define: format!("#define {macro_name} 1\n"),
    });
  }
  for (member, macro_name) in PJDFSTEST_STAT_MEMBERS {
    probes.push(ConfigProbe {
      source: format!(
        "{SYSTEM_EXTENSIONS}#include <sys/types.h>\n#include <sys/stat.h>\nint main(void) {{ struct stat s; (void)s.{member}; return 0; }}\n"
      ),
      compile_only: true,
      define: format!("#define {macro_name} 1\n"),
    });
  }
  probes
}

/// Format: the head of the harness's `config.h`: its origin, then what `AC_USE_SYSTEM_EXTENSIONS` defines.
pub fn config_head() -> String {
  format!(
    "/* slates conformance harness: configure.ac's checks, probed by cc */\n{SYSTEM_EXTENSIONS}"
  )
}
