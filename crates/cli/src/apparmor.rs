//! `slates apparmor profile` and `slates apparmor check` (§4.6 "The launcher", A-370): slates' AppArmor profile for the
//! chosen-path launcher, and whether `exec` can make its namespace on this host. Ubuntu's AppArmor (24.04 and 26.04)
//! confines every unprivileged user namespace, and the profile lifts that for one executable. Loading it needs root,
//! which slates never asks for (R10), and writes `/etc/apparmor.d`, which slates never writes (R1). So `profile` prints
//! it, `check` prints the commands an administrator runs, and the system's own `tee` and `apparmor_parser` write and
//! load it. Whether `exec` uses it is still its caller's `--apparmor-profile`, off unless given.

use slates_api::exec::{Confinement, Readiness};
use slates_client::exec::{ProfileRefusal, load_command, profile_file, unload_command};

use crate::Failure;

/// Prints slates' AppArmor profile for `executable`, else for this binary's own path as the kernel records it.
pub(crate) fn profile(executable: Option<&str>) -> Result<(), Failure> {
  let executable = match executable {
    Some(path) => path.to_owned(),
    None => slates_api::exec::confinement().executable.ok_or_else(|| {
      Failure::Refused(
        "this platform does not name this binary's path for an AppArmor profile (AppArmor is Linux's); name the \
         executable with --executable PATH"
          .to_owned(),
      )
    })?,
  };
  let text = slates_client::exec::profile(&executable)
    .map_err(|refusal| Failure::Refused(refusal.to_string()))?;
  slates_machine::out_line!("{}", text.trim_end_matches('\n'));
  Ok(())
}

/// What `check` reports beside the readiness: the profile's file and the commands that load or unload it, or why this
/// binary's path cannot name a profile.
struct Commands {
  file: Option<String>,
  load: Option<String>,
  unload: Option<String>,
  refusal: Option<ProfileRefusal>,
}

/// The commands for `executable`: printing the profile through this binary, then the system's own tools placing and
/// loading it as root.
fn commands(executable: Option<&str>) -> Commands {
  let Some(executable) = executable else {
    return Commands {
      file: None,
      load: None,
      unload: None,
      refusal: None,
    };
  };
  match (
    profile_file(executable),
    load_command(executable),
    unload_command(executable),
  ) {
    (Ok(file), Ok(load), Ok(unload)) => Commands {
      load: Some(format!(
        "{executable} apparmor profile | sudo tee {file} >/dev/null && {load}"
      )),
      file: Some(file),
      unload: Some(unload),
      refusal: None,
    },
    (Err(refusal), _, _) | (_, Err(refusal), _) | (_, _, Err(refusal)) => Commands {
      file: None,
      load: None,
      unload: None,
      refusal: Some(refusal),
    },
  }
}

/// The readiness as `check` names it.
fn readiness_word(readiness: Readiness) -> &'static str {
  match readiness {
    Readiness::Unsupported => "unsupported",
    Readiness::Unrestricted => "unrestricted",
    Readiness::ProfileLoaded => "profile-loaded",
    Readiness::ProfileMissing => "profile-missing",
  }
}

/// Reports whether this host restricts unprivileged user namespaces, the label this binary runs under, and whether
/// `exec --apparmor-profile` can make its namespace; with the commands that load or unload the profile. Succeeds where
/// `exec` can make its namespace (no restriction, or the profile loaded); refused, naming what is missing, elsewhere.
pub(crate) fn check(json: bool) -> Result<(), Failure> {
  let (confinement, readiness) = slates_api::exec::check();
  let commands = commands(confinement.executable.as_deref());
  if json {
    print_json(&confinement, readiness, &commands);
  } else {
    print_lines(&confinement, readiness, &commands);
  }
  match readiness {
    Readiness::Unrestricted | Readiness::ProfileLoaded => Ok(()),
    Readiness::Unsupported => Err(Failure::Refused(
      slates_api::exec::ExecFault::Unsupported.to_string(),
    )),
    Readiness::ProfileMissing => Err(Failure::Refused(match (&commands.load, &commands.refusal) {
      (Some(load), _) => format!(
        "this host's AppArmor confines unprivileged user namespaces and no slates profile is loaded for this \
         binary; an administrator loads it as root: {load}"
      ),
      (None, Some(refusal)) => format!(
        "this host's AppArmor confines unprivileged user namespaces and no slates profile can be loaded for this \
         binary: {refusal}"
      ),
      (None, None) => "this host's AppArmor confines unprivileged user namespaces and the kernel does not name this \
                       binary's path, so no slates profile can attach to it"
        .to_owned(),
    })),
  }
}

fn print_lines(confinement: &Confinement, readiness: Readiness, commands: &Commands) {
  let restricted = match confinement.restricted {
    Some(true) => "yes",
    Some(false) => "no",
    None => "unknown",
  };
  slates_machine::out_line!("restricted: {restricted}");
  let privileged = match confinement.privileged {
    Some(true) => "yes (CAP_SYS_ADMIN exempts it from the restriction)",
    Some(false) => "no",
    None => "unknown",
  };
  slates_machine::out_line!("privileged: {privileged}");
  slates_machine::out_line!(
    "label: {}",
    confinement.label.as_deref().unwrap_or("unknown")
  );
  slates_machine::out_line!(
    "executable: {}",
    confinement.executable.as_deref().unwrap_or("unknown")
  );
  slates_machine::out_line!("readiness: {}", readiness_word(readiness));
  if let Some(file) = &commands.file {
    slates_machine::out_line!("profile-file: {file}");
  }
  match readiness {
    Readiness::ProfileMissing => {
      if let Some(load) = &commands.load {
        slates_machine::out_line!("load: {load}");
      }
    }
    Readiness::ProfileLoaded => {
      if let Some(unload) = &commands.unload {
        slates_machine::out_line!("unload: {unload}");
      }
    }
    Readiness::Unsupported | Readiness::Unrestricted => {}
  }
  if let Some(refusal) = &commands.refusal {
    slates_machine::out_line!("refusal: {refusal}");
  }
}

fn print_json(confinement: &Confinement, readiness: Readiness, commands: &Commands) {
  slates_machine::out_line!(
    "{}",
    serde_json::json!({
      "restricted": confinement.restricted,
      "privileged": confinement.privileged,
      "label": confinement.label,
      "executable": confinement.executable,
      "readiness": readiness_word(readiness),
      "profile_file": commands.file,
      "load": commands.load,
      "unload": commands.unload,
      "refusal": commands.refusal.as_ref().map(ToString::to_string),
    })
  );
}
