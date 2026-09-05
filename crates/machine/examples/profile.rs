//! Prints this machine's profile as JSON, then the derived constants with their formulas.
//!
//! `cargo run --release -p slates-machine --example profile`

use slates_machine::{MachineProfile, ProfileOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let profile = MachineProfile::measure(ProfileOptions::default());
  println!("{}", profile.to_json()?);
  for line in profile.derived().lines() {
    eprintln!("{line}");
  }
  eprintln!(
    "profile took {} ms; quick probes = {:?}; notes = {:?}",
    profile.elapsed_ns / 1_000_000,
    profile.quick_probes(),
    profile.facts.notes
  );
  Ok(())
}
