//! The chosen-path launcher's request and its AppArmor profile (§4.6 "The launcher", §4.12; A-370): one grammar and
//! one profile text for every surface that starts the launcher. Those are the `slates` command's `exec`, an embedding
//! host's role entry (`slates_api::role::main`), and the Python and Node SDKs, which build the command a program
//! spawns. So no surface drifts from another. The launcher itself (the namespace, the bind, the exec) is
//! `slates_api::exec`, Linux's alone; everything here is pure, so it is tested on every host.
//!
//! **Why a profile (A-370).** Ubuntu's AppArmor (24.04 and 26.04 by default) confines every unprivileged user namespace
//! (`kernel.apparmor_restrict_unprivileged_userns=1`). `unshare` succeeds, then the process moves into the
//! `unprivileged_userns` profile, which denies the namespace every capability. So the launcher can neither write its
//! maps nor bind the volume, whoever writes the maps (CI's runners, diagnostics run 42169647315). A profile for the
//! launcher's executable that grants `userns,` lifts that for the executable alone. Measured on 2026-11-18,
//! diagnostics run 44344947954, on Ubuntu 24.04 (AppArmor 4.0.1, Linux 7.0) and Ubuntu 26.04 (AppArmor 5.0.2, with
//! `apparmor_restrict_unprivileged_unconfined=1`), the launcher's sequence run as the runner's unprivileged user:
//! - with [`profile`]'s text loaded, it writes its maps, makes the tree private and binds, and its command reads the
//!   volume;
//! - a copy of the same binary at another path is still refused, since a profile attaches to its path;
//! - without the profile, every step past `unshare` is refused, as before.
//!
//! **The flag (A-370).** Loading the profile needs root, so slates never loads it (R10): [`profile`] renders it, and an
//! administrator places it at [`profile_file`] and loads it with [`load_command`]. Whether the launcher may use it is
//! its caller's [`AppArmor`] option. That option is off unless the user opts in, the same in every surface:
//! - off, the launcher leaves the profile before it makes its namespace, so a loaded profile changes nothing for a
//!   caller who did not ask;
//! - on, it makes its namespace under the profile and runs its command outside it, so the command holds nothing the
//!   host withholds from it.

use std::fmt;

/// Whether the chosen-path launcher may make its namespace under slates' AppArmor profile for its executable (A-370).
/// Off unless the launcher's user opts in, in every surface: `slates exec --apparmor-profile`, slates-api's
/// [`ExecSpec::apparmor`], the Python SDK's `apparmor_profile=True` and the Node SDK's `{ apparmorProfile: true }`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum AppArmor {
  /// The launcher does not use the profile. Where its executable runs under it, the launcher leaves it before it makes
  /// its namespace, so the host's restriction applies as though the profile were not loaded.
  #[default]
  Off,
  /// The launcher makes its namespace under the profile, then runs its command outside it.
  Profile,
}

/// Format: the launcher's word, which begins its arguments.
pub const EXEC_WORD: &str = "exec";
/// Format: the flag naming the volume, whose directory under the daemon's root mount the launcher binds.
pub const VOLUME_FLAG: &str = "--volume";
/// Format: the flag naming the existing directory the launcher shows the volume at.
pub const AT_FLAG: &str = "--at";
/// Format: the switch that turns [`AppArmor::Profile`] on.
pub const APPARMOR_PROFILE_SWITCH: &str = "--apparmor-profile";
/// Format: the word that ends the launcher's flags; every argument after it is the command's, untouched.
pub const COMMAND_BOUNDARY: &str = "--";

/// A request of the chosen-path launcher: the volume, the existing directory to show it at, the command, and whether
/// the launcher may use slates' AppArmor profile. The volume is a name under the daemon's root mount, which the
/// launcher reads from `SLATES_ROOT` in its environment (§4.6; the daemon publishing its root mount is owed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecSpec {
  /// The volume: its directory is `$SLATES_ROOT/<volume>`.
  pub volume: String,
  /// The existing directory the command sees the volume at. The launcher never creates it (AC-3.5).
  pub at: String,
  /// The command and its arguments.
  pub command: Vec<String>,
  /// Whether the launcher may make its namespace under slates' AppArmor profile (A-370): off unless asked.
  pub apparmor: AppArmor,
}

/// Why the launcher's arguments do not parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecArgsError {
  /// No `exec` word before the command boundary.
  NoWord,
  /// No `--` and command after it.
  NoCommand,
  /// A flag the launcher needs, absent.
  Missing(&'static str),
  /// A flag that takes a value, given none.
  MissingValue(String),
  /// A switch given a value.
  SwitchValue(String),
  /// A flag the launcher does not take.
  UnknownFlag(String),
  /// A word the launcher does not take.
  Extra(String),
}

impl fmt::Display for ExecArgsError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::NoWord => write!(f, "no `{EXEC_WORD}` before the command"),
      Self::NoCommand => write!(f, "missing `{COMMAND_BOUNDARY}` then the command"),
      Self::Missing(flag) => write!(f, "missing {flag}"),
      Self::MissingValue(flag) => write!(f, "`{flag}` takes a value"),
      Self::SwitchValue(flag) => write!(f, "`{flag}` takes no value"),
      Self::UnknownFlag(flag) => write!(f, "unknown flag `{flag}`"),
      Self::Extra(word) => write!(f, "unexpected argument `{word}`"),
    }
  }
}

impl std::error::Error for ExecArgsError {}

impl ExecSpec {
  /// The launcher's arguments, which the `slates` command and an embedding host's role entry read back to this request:
  /// `exec --volume=V --at=PATH [--apparmor-profile] -- CMD [ARG ...]`. Each value follows its flag after `=`, so a value
  /// that begins with a dash stays a value.
  pub fn arguments(&self) -> Vec<String> {
    let mut arguments = vec![
      EXEC_WORD.to_owned(),
      format!("{VOLUME_FLAG}={}", self.volume),
      format!("{AT_FLAG}={}", self.at),
    ];
    if self.apparmor == AppArmor::Profile {
      arguments.push(APPARMOR_PROFILE_SWITCH.to_owned());
    }
    arguments.push(COMMAND_BOUNDARY.to_owned());
    arguments.extend(self.command.iter().cloned());
    arguments
  }

  /// Reads the launcher's arguments as [`ExecSpec::arguments`] makes them and a person types them: the `exec` word and
  /// the flags in any order before `--`, each value after its flag or joined by `=`; the command after `--`, untouched.
  pub fn parse(arguments: &[String]) -> Result<ExecSpec, ExecArgsError> {
    let boundary = arguments
      .iter()
      .position(|argument| argument == COMMAND_BOUNDARY)
      .ok_or(ExecArgsError::NoCommand)?;
    let (flags, rest) = arguments.split_at(boundary);
    let command: Vec<String> = rest.get(1..).map(<[String]>::to_vec).unwrap_or_default();
    if command.is_empty() {
      return Err(ExecArgsError::NoCommand);
    }
    let mut word = false;
    let mut volume = None;
    let mut at = None;
    let mut apparmor = AppArmor::Off;
    let mut given = flags.iter();
    while let Some(argument) = given.next() {
      if !argument.starts_with("--") {
        if argument == EXEC_WORD && !word {
          word = true;
          continue;
        }
        return Err(ExecArgsError::Extra(argument.clone()));
      }
      let (flag, inline) = match argument.split_once('=') {
        Some((flag, value)) => (flag, Some(value.to_owned())),
        None => (argument.as_str(), None),
      };
      if flag == APPARMOR_PROFILE_SWITCH {
        if inline.is_some() {
          return Err(ExecArgsError::SwitchValue(flag.to_owned()));
        }
        apparmor = AppArmor::Profile;
        continue;
      }
      let slot = match flag {
        VOLUME_FLAG => &mut volume,
        AT_FLAG => &mut at,
        other => return Err(ExecArgsError::UnknownFlag(other.to_owned())),
      };
      let value = match inline {
        Some(value) => value,
        None => given
          .next()
          .cloned()
          .ok_or_else(|| ExecArgsError::MissingValue(flag.to_owned()))?,
      };
      *slot = Some(value);
    }
    if !word {
      return Err(ExecArgsError::NoWord);
    }
    Ok(ExecSpec {
      volume: volume.ok_or(ExecArgsError::Missing(VOLUME_FLAG))?,
      at: at.ok_or(ExecArgsError::Missing(AT_FLAG))?,
      command,
      apparmor,
    })
  }
}

/// Format: the AppArmor policy ABI the profile is written in: 4.0, the first that mediates user namespaces (`userns,`).
/// Every host that restricts unprivileged user namespaces parses it: Ubuntu 24.04 ships AppArmor 4.0.1 and 26.04 ships
/// 5.0.2 (diagnostics run 44344947954). Ubuntu's own `userns,` profiles declare the same ABI.
const PROFILE_ABI: &str = "abi <abi/4.0>,";

/// Format: the directory AppArmor loads its profiles from at boot (`apparmor.service`), where a loaded profile stays
/// loaded across a restart.
pub const PROFILE_DIRECTORY: &str = "/etc/apparmor.d";

/// Format: the profile's header comment, before its rules. It says what the profile does and what loading it grants.
const PROFILE_HEADER: &str = "\
# slates (A-370): lets this executable's chosen-path launcher (`slates exec --apparmor-profile`) make its user and
# mount namespace on a host whose AppArmor restricts unprivileged user namespaces
# (kernel.apparmor_restrict_unprivileged_userns=1). It grants `userns,` and changes nothing else; the launcher runs its
# command outside it, and leaves it unless its caller asks for it. Whoever may run this executable may create user
# namespaces through it, as every process may on a host without the restriction: load it only where that is
# acceptable. Remove it with `apparmor_parser -R` on this file, then delete the file.
";

/// Why an executable's path cannot name an AppArmor profile exactly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProfileRefusal {
  /// The path is not absolute. A profile attaches to the absolute path the kernel records for an executable.
  NotAbsolute(String),
  /// The path holds a character outside ASCII letters, digits and `/ . _ - +`. AppArmor reads several such characters
  /// as patterns, quotes, variables or separators, so the profile could attach to other paths or to none; the rest are
  /// refused with them rather than judged one by one.
  Character {
    /// The path.
    path: String,
    /// The first character refused.
    character: char,
  },
  /// The path has an empty, `.` or `..` component, or ends with `/`. The kernel's path of an executable never does, so
  /// the profile would attach to nothing.
  Component(String),
}

impl fmt::Display for ProfileRefusal {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::NotAbsolute(path) => write!(
        f,
        "{path} is not an absolute path; an AppArmor profile attaches to the absolute path of an executable"
      ),
      Self::Character { path, character } => write!(
        f,
        "{path} holds {character:?}, which an AppArmor profile cannot name exactly; an executable's path for the slates \
         profile holds only ASCII letters, digits and `/ . _ - +`: move the executable"
      ),
      Self::Component(path) => write!(
        f,
        "{path} has an empty, `.` or `..` component or ends with `/`; name the executable by the path the kernel \
         records (`readlink -f`)"
      ),
    }
  }
}

impl std::error::Error for ProfileRefusal {}

/// Whether `character` may stand in an executable's path that names the profile: ASCII letters, digits and `/ . _ - +`,
/// none of which AppArmor reads as more than itself in a profile's name.
fn plain(character: char) -> bool {
  character.is_ascii_alphanumeric() || matches!(character, '/' | '.' | '_' | '-' | '+')
}

/// Checks that `executable` names an AppArmor profile exactly, or why not.
fn checked(executable: &str) -> Result<&str, ProfileRefusal> {
  let Some(rest) = executable.strip_prefix('/') else {
    return Err(ProfileRefusal::NotAbsolute(executable.to_owned()));
  };
  if let Some(character) = executable.chars().find(|character| !plain(*character)) {
    return Err(ProfileRefusal::Character {
      path: executable.to_owned(),
      character,
    });
  }
  if rest
    .split('/')
    .any(|component| matches!(component, "" | "." | ".."))
  {
    return Err(ProfileRefusal::Component(executable.to_owned()));
  }
  Ok(executable)
}

/// The AppArmor profile that lets `executable`'s chosen-path launcher make its namespace on a host whose AppArmor
/// restricts unprivileged user namespaces (A-370). `executable` is the absolute path the kernel records for the
/// executable that serves the launcher: the `slates` binary, or an embedding host's own. The profile is named by that
/// path, AppArmor's own convention, so two executables on one host never share a name, and the launcher knows its own
/// profile by its label. It grants `userns,` in `flags=(unconfined)` mode, which mediates nothing else: the least that
/// gives the launcher's namespace its capabilities, the form Ubuntu's own user-namespace profiles take
/// (`/etc/apparmor.d/chrome` on the runners).
pub fn profile(executable: &str) -> Result<String, ProfileRefusal> {
  let name = checked(executable)?;
  Ok(format!(
    "{PROFILE_HEADER}{PROFILE_ABI}\n\nprofile {name} flags=(unconfined) {{\n  userns,\n}}\n"
  ))
}

/// The file an administrator places [`profile`]'s text in, so it is loaded at every boot: AppArmor's convention, the
/// path without its leading `/` and with each `/` a `.` (`/usr/local/bin/slates` is `/etc/apparmor.d/usr.local.bin.slates`).
pub fn profile_file(executable: &str) -> Result<String, ProfileRefusal> {
  let name = checked(executable)?;
  let file = name.trim_start_matches('/').replace('/', ".");
  Ok(format!("{PROFILE_DIRECTORY}/{file}"))
}

/// The command an administrator runs, as root, to load the profile placed at [`profile_file`]: AppArmor's parser,
/// replacing any profile of the same name.
pub fn load_command(executable: &str) -> Result<String, ProfileRefusal> {
  Ok(format!(
    "sudo apparmor_parser -r {}",
    profile_file(executable)?
  ))
}

/// The commands an administrator runs, as root, to unload the profile and remove its file.
pub fn unload_command(executable: &str) -> Result<String, ProfileRefusal> {
  let file = profile_file(executable)?;
  Ok(format!("sudo apparmor_parser -R {file} && sudo rm {file}"))
}

/// The profile name in an AppArmor label as `/proc/self/attr/apparmor/current` reads: `/usr/local/bin/slates
/// (unconfined)` names `/usr/local/bin/slates`; `unconfined`, which carries no mode, names itself.
pub fn label_name(label: &str) -> &str {
  let label = label.trim_end_matches(['\n', '\0']);
  match label.rsplit_once(" (") {
    Some((name, mode)) if mode.ends_with(')') => name,
    _ => label,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn words(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_owned).collect()
  }

  /// A-370, the flag's plumbing. Do: make the launcher's arguments of a request with the AppArmor option on and off,
  /// with values that begin with a dash or hold `=`, and read them back; parse a person's spelling too. Expect: the
  /// switch present exactly when the option is on, every request read back to itself, and the option off unless named.
  #[test]
  fn the_apparmor_option_crosses_the_launchers_arguments_exactly_when_it_is_on() {
    for apparmor in [AppArmor::Off, AppArmor::Profile] {
      let spec = ExecSpec {
        volume: "-scratch=1".into(),
        at: "/home/u/proj/build".into(),
        command: words("cargo build --release -- --apparmor-profile"),
        apparmor,
      };
      let arguments = spec.arguments();
      let boundary = arguments.iter().position(|a| a == "--").unwrap();
      assert_eq!(
        arguments[..boundary].contains(&APPARMOR_PROFILE_SWITCH.to_owned()),
        apparmor == AppArmor::Profile,
        "{arguments:?}"
      );
      assert_eq!(ExecSpec::parse(&arguments), Ok(spec));
    }
    let typed = ExecSpec::parse(&words("--at /p exec --volume v -- sh -c true")).unwrap();
    assert_eq!(typed.apparmor, AppArmor::Off, "off unless named");
    assert_eq!((typed.volume.as_str(), typed.at.as_str()), ("v", "/p"));
    let asked =
      ExecSpec::parse(&words("exec --apparmor-profile --volume v --at /p -- true")).unwrap();
    assert_eq!(asked.apparmor, AppArmor::Profile);
    assert_eq!(AppArmor::default(), AppArmor::Off);
  }

  /// §4.6 "The launcher". Do: parse arguments missing each part, or carrying what the launcher does not take. Expect:
  /// each refused by name, never a request with a part made up.
  #[test]
  fn the_launchers_arguments_refuse_what_is_missing_or_foreign() {
    let cases = [
      ("exec --volume v --at /p", ExecArgsError::NoCommand),
      ("exec --volume v --at /p --", ExecArgsError::NoCommand),
      ("--volume v --at /p -- true", ExecArgsError::NoWord),
      ("exec --at /p -- true", ExecArgsError::Missing(VOLUME_FLAG)),
      ("exec --volume v -- true", ExecArgsError::Missing(AT_FLAG)),
      ("exec --volume v --at", ExecArgsError::NoCommand),
      (
        "exec --volume v --at /p --shards 2 -- true",
        ExecArgsError::UnknownFlag("--shards".into()),
      ),
      (
        "exec exec --volume v --at /p -- true",
        ExecArgsError::Extra("exec".into()),
      ),
      (
        "exec --volume v --at /p --apparmor-profile=yes -- true",
        ExecArgsError::SwitchValue(APPARMOR_PROFILE_SWITCH.into()),
      ),
    ];
    for (text, refusal) in cases {
      assert_eq!(ExecSpec::parse(&words(text)), Err(refusal), "{text}");
    }
    assert_eq!(
      ExecSpec::parse(&["exec".into(), "--volume".into(), "v".into(), "--at".into()]),
      Err(ExecArgsError::NoCommand)
    );
    let mut dangling = words("exec --volume v --at");
    dangling.extend(words("-- true"));
    assert_eq!(
      ExecSpec::parse(&dangling),
      Err(ExecArgsError::MissingValue(AT_FLAG.into())),
      "a flag whose value would be the boundary"
    );
  }

  /// A-370. Do: render the profile for the installed binary's path, and its file and commands. Expect: the 4.0 ABI, a
  /// profile named by the path in unconfined mode granting `userns,` and no other rule, its file under
  /// `/etc/apparmor.d` by AppArmor's dotted convention, and the parser's load and unload of that file.
  #[test]
  fn the_profile_names_the_executable_and_grants_user_namespaces_alone() {
    let text = profile("/usr/local/bin/slates").unwrap();
    assert!(text.contains("\nabi <abi/4.0>,\n"), "{text}");
    assert!(
      text.contains("\nprofile /usr/local/bin/slates flags=(unconfined) {\n  userns,\n}\n"),
      "{text}"
    );
    let rules: Vec<&str> = text
      .lines()
      .map(str::trim)
      .filter(|line| !line.is_empty() && !line.starts_with('#'))
      .collect();
    assert_eq!(
      rules,
      [
        "abi <abi/4.0>,",
        "profile /usr/local/bin/slates flags=(unconfined) {",
        "userns,",
        "}"
      ],
      "one rule, `userns,`; the header is comments alone"
    );
    assert!(
      text
        .lines()
        .filter(|line| line.starts_with('#'))
        .all(|line| !line.starts_with("#include")),
      "no comment line reads as an include"
    );
    assert_eq!(
      profile_file("/usr/local/bin/slates").unwrap(),
      "/etc/apparmor.d/usr.local.bin.slates"
    );
    assert_eq!(
      load_command("/home/runner/.cargo/bin/slates").unwrap(),
      "sudo apparmor_parser -r /etc/apparmor.d/home.runner..cargo.bin.slates"
    );
    assert_eq!(
      unload_command("/usr/bin/kiln").unwrap(),
      "sudo apparmor_parser -R /etc/apparmor.d/usr.bin.kiln && sudo rm /etc/apparmor.d/usr.bin.kiln"
    );
  }

  /// A-370, hostile paths. Do: render the profile for paths AppArmor would read as more than a path, or that the
  /// kernel never records. Expect: each refused naming why, so no profile ever attaches to other paths or to none.
  #[test]
  fn a_path_the_profile_cannot_name_exactly_is_refused() {
    assert_eq!(
      profile("slates"),
      Err(ProfileRefusal::NotAbsolute("slates".into()))
    );
    for (path, character) in [
      ("/opt/my tools/slates", ' '),
      ("/opt/*/slates", '*'),
      ("/opt/sl?tes", '?'),
      ("/opt/{a,b}/slates", '{'),
      ("/opt/@{HOME}/slates", '@'),
      ("/opt/\"q\"/slates", '"'),
      ("/opt/a\\b/slates", '\\'),
      ("/opt/#x/slates", '#'),
      ("/opt/[ab]/slates", '['),
      ("/opt/sl\u{e4}tes", '\u{e4}'),
      ("/opt/a\nprofile x {", '\n'),
    ] {
      assert_eq!(
        profile(path),
        Err(ProfileRefusal::Character {
          path: path.into(),
          character
        }),
        "{path:?}"
      );
    }
    for path in [
      "/usr//bin/slates",
      "/usr/./slates",
      "/usr/../slates",
      "/usr/bin/",
      "/",
    ] {
      assert_eq!(
        profile_file(path),
        Err(ProfileRefusal::Component(path.into())),
        "{path}"
      );
    }
  }

  /// A-370. Do: read the profile name out of labels as the kernel writes them. Expect: the name before the mode, and
  /// `unconfined` (which carries no mode) as itself.
  #[test]
  fn a_label_names_its_profile() {
    assert_eq!(
      label_name("/usr/local/bin/slates (unconfined)\n"),
      "/usr/local/bin/slates"
    );
    assert_eq!(
      label_name("unprivileged_userns (enforce)"),
      "unprivileged_userns"
    );
    assert_eq!(label_name("unconfined\n"), "unconfined");
    assert_eq!(label_name("slates (complain)\0"), "slates");
  }
}
