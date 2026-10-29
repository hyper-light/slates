//! The rendezvous seal's cost per rendezvous (§4.7, A-187), measured from a release binary. Each round is one whole
//! exchange as the daemon and the client run it, timed alone: both halves made, both agreed, the client's hello sealed
//! and opened by the daemon, the daemon's handoff sealed and opened by the client. It prints the least, the median and
//! the 99th percentile over the rounds, so the seal's own share of a connect can be told from the round trips and the
//! daemon's rounds a connect also waits on.
//!
//! Run: `cargo run --release -p slates-seal --example seal_cost -- ROUNDS`

use std::process::ExitCode;
use std::time::{Duration, Instant};

use slates_ipc::error::IpcError;
use slates_ipc::seal::{Class, Role};

/// Format: the bytes of the client's hello: the id it wants.
const HELLO_BYTES: usize = 4;
/// Format: the bytes of the daemon's handoff: the client id and the region's length.
const HANDOFF_BYTES: usize = 12;
/// Format: the percentile printed beside the median.
const TAIL_PERCENT: usize = 99;
/// Format: the whole a percentile is a part of.
const WHOLE_PERCENT: usize = 100;

fn main() -> ExitCode {
  let rounds = match std::env::args().nth(1).map(|value| value.parse::<usize>()) {
    Some(Ok(rounds)) if rounds > 0 => rounds,
    _ => {
      eprintln!("usage: seal_cost ROUNDS (a count above zero)");
      return ExitCode::FAILURE;
    }
  };
  match measure(rounds) {
    Ok(mut took) => {
      took.sort_unstable();
      let at = |percent: usize| {
        let index = took.len().saturating_sub(1).saturating_mul(percent) / WHOLE_PERCENT;
        took.get(index).copied().unwrap_or_default()
      };
      println!(
        "seal: {rounds} exchanges, least {:?}, median {:?}, p{TAIL_PERCENT} {:?}",
        at(0),
        at(WHOLE_PERCENT / 2),
        at(TAIL_PERCENT)
      );
      ExitCode::SUCCESS
    }
    Err(refusal) => {
      eprintln!("seal: an exchange was refused: {refusal}");
      ExitCode::FAILURE
    }
  }
}

/// Times `rounds` whole exchanges, each on its own.
fn measure(rounds: usize) -> Result<Vec<Duration>, IpcError> {
  let mut took = Vec::with_capacity(rounds);
  for _ in 0..rounds {
    let started = Instant::now();
    exchange()?;
    took.push(started.elapsed());
  }
  Ok(took)
}

/// One exchange, both sides, as the rendezvous runs it.
fn exchange() -> Result<(), IpcError> {
  let daemon = slates_seal::fresh()?;
  let daemon_key = daemon.public_key();
  let client = slates_seal::fresh()?;
  let client_key = client.public_key();
  let mut client = client.agree(&daemon_key, Role::Client)?;
  let hello = client.seal(Class::Hello, &[0; HELLO_BYTES])?;
  let mut daemon = daemon.agree(&client_key, Role::Daemon)?;
  daemon.open(Class::Hello, &hello)?;
  let handoff = daemon.seal(Class::Handoff, &[0; HANDOFF_BYTES])?;
  client.open(Class::Handoff, &handoff)?;
  Ok(())
}
