//! AC-9.7, §4.6, A-189: the VM harness must retain the VMM's final error after its probe reply. This real-process
//! fixture uses the same serial protocol and stderr forwarding without requiring a hypervisor or a mount.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod vm;

use std::io::{BufRead, Write};
use std::process::Command;

use slates_guest_probe::protocol::Command as Ask;

/// Shape: a child role's failure differs from a libtest process selecting zero tests.
const FAILED_VMM: i32 = 3;
/// Format: the final diagnostic whose observable retention the parent requires.
const FINAL_ERROR: &str = "the VMM's final shutdown diagnostic";
/// Shape: the fixture answers Ready and Done only, so no byte payload is admitted.
const NO_REPLY_BYTES: usize = 0;

/// AC-9.7/A-189. Do: acknowledge quit, then release a child to print its error and exit. Expect: its nonzero status
/// and final stderr both survive the harness's exit wait, even though the probe's reply was already consumed.
#[test]
fn a_vmm_failure_keeps_the_error_written_after_the_probes_final_reply() {
  let mut command = Command::new(std::env::current_exe().unwrap());
  command.args([
    "--ignored",
    "--exact",
    "late_diagnostic_role",
    "--nocapture",
    "--test-threads=1",
  ]);
  let mut guest = vm::Guest::spawn(&mut command, "fixture: ", NO_REPLY_BYTES);
  guest.done(&Ask::Quit);
  // The child cannot write its diagnostic until the parent has consumed the first Done and sends this release.
  guest.send(&Ask::Quit);
  let status = guest.wait_for_exit();
  assert_eq!(status.code(), Some(FAILED_VMM));
  assert!(guest.said(FINAL_ERROR), "{}", guest.transcript());
}

/// AC-9.7/A-189. The parent's owned child: its second command proves the first reply was consumed before stderr.
#[test]
#[ignore]
fn late_diagnostic_role() {
  let mut input = std::io::stdin().lock();
  // libtest's role banner has no trailing newline; keep the protocol's first reply on its own line.
  println!("\nPROBE ready");
  std::io::stdout().flush().unwrap();
  let mut line = String::new();
  input.read_line(&mut line).unwrap();
  assert_eq!(line.trim(), Ask::Quit.encode());
  println!("PROBE done");
  std::io::stdout().flush().unwrap();
  line.clear();
  input.read_line(&mut line).unwrap();
  assert_eq!(line.trim(), Ask::Quit.encode());
  eprintln!("{FINAL_ERROR}");
  std::io::stderr().flush().unwrap();
  std::process::exit(FAILED_VMM);
}
