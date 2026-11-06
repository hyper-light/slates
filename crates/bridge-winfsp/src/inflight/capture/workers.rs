//! A-219's oracle: two receive workers, each with its own journal, over one serial model of WinFsp's queues, each sending
//! a batch's last response in the call that receives its next batch (A-215's combined call). The call is two steps of
//! the driver, the response finished and then the delivery, so a kill can fall between them: the response consumed,
//! nothing delivered to this worker, and the freed address free for the other worker's next IRP. Every step of every
//! worker passes a turnstile that orders the workers by a schedule and can kill the whole daemon at any step. A successor
//! recovers all journals as A-219 says, then serves on, and may itself be killed. Every IRP must be completed once, with
//! its own identity, none lost; every open the ledger keeps must be one the kernel was handed.
#[cfg(test)]
mod oracle {
  #![allow(
    clippy::unwrap_used,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_types,
    clippy::cognitive_complexity
  )]

  use super::super::super::{CREATE_KIND, REQUEST_BASE, opened_context};
  use super::super::*;
  use std::collections::{BTreeMap, BTreeSet, VecDeque};
  use std::sync::{Condvar, Mutex};

  /// Shape: the model's IRPs: enough for several batches a worker and a freed address reused across workers.
  const IRPS: u64 = 12;
  /// Shape: the workers, A-219's most.
  const WORKERS: usize = MAX_WORKERS;
  /// Format: where a model request carries its IRP's identity, and a model response the identity it answers.
  const ID_AT: usize = 16;
  /// Format: IoStatus.Information and IoStatus.Status in the response header.
  const INFORMATION_AT: usize = 16;
  /// Format: see INFORMATION_AT.
  const STATUS_FIELD_AT: usize = 20;
  /// Format: Create.Opened.UserContext2.
  const CONTEXT_AT: usize = 32;
  /// Format: STATUS_CANCELLED.
  const CANCELLED_STATUS: u32 = 0xC000_0120;

  #[derive(Clone, Copy, Debug)]
  struct Irp {
    id: u64,
    address: u64,
  }

  /// The driver's queues, the ledger's opens and the clock, shared by the workers.
  #[derive(Default)]
  struct World {
    pending: VecDeque<Irp>,
    processing: BTreeMap<u64, Irp>,
    free: Vec<u64>,
    /// The worker whose response last freed each address, for the cross-worker reuse count.
    freed_by: BTreeMap<u64, usize>,
    arrived: u64,
    calls: u64,
    clock: u64,
    completed: BTreeMap<u64, Option<u64>>,
    wrong: u64,
    twice: u64,
    bogus: u64,
    reused_across: u64,
    /// The ledger: unconfirmed opens (context, hint), confirmed and retired contexts.
    unconfirmed: Vec<(u64, u64)>,
    confirmed: BTreeSet<u64>,
    retired: BTreeSet<u64>,
    passed_over: u64,
    unstamped: u64,
    /// Saved responses of a hand-off that delivered nothing, passed over because the other worker's newer delivery
    /// holds their address: the case a second worker adds to A-215's.
    passed_handed_off: u64,
  }

  impl World {
    fn arrive(&mut self, count: u64) {
      for _ in 0..count {
        if self.arrived == IRPS {
          return;
        }
        let address = self.free.pop().unwrap_or(0x1000 * (self.arrived + 1));
        self.pending.push_back(Irp {
          id: self.arrived,
          address,
        });
        self.arrived += 1;
      }
    }

    fn respond(&mut self, worker: usize, response: &[u8]) {
      let hint = u64::from_le_bytes(response[8..16].try_into().unwrap());
      let Some(irp) = self.processing.remove(&hint) else {
        self.bogus += 1;
        return;
      };
      let status = u32::from_le_bytes(
        response[STATUS_FIELD_AT..STATUS_FIELD_AT + 4]
          .try_into()
          .unwrap(),
      );
      let outcome = if status == 0 {
        let answered = u32::from_le_bytes(
          response[INFORMATION_AT..INFORMATION_AT + 4]
            .try_into()
            .unwrap(),
        );
        if u64::from(answered) != irp.id {
          self.wrong += 1;
        }
        Some(u64::from_le_bytes(
          response[CONTEXT_AT..CONTEXT_AT + 8].try_into().unwrap(),
        ))
      } else {
        assert_eq!(status, CANCELLED_STATUS);
        None
      };
      if self.completed.insert(irp.id, outcome).is_some() {
        self.twice += 1;
      }
      self.free.push(irp.address);
      self.freed_by.insert(irp.address, worker);
    }

    fn deliver(&mut self, worker: usize, output: &mut [u8]) -> usize {
      let mut at = 0;
      while at + REQUEST_BASE <= output.len() {
        let Some(irp) = self.pending.pop_front() else {
          break;
        };
        let frame = &mut output[at..at + REQUEST_BASE];
        frame[2..4].copy_from_slice(&u16::try_from(REQUEST_BASE).unwrap().to_le_bytes());
        frame[4..8].copy_from_slice(&CREATE_KIND.to_le_bytes());
        frame[8..16].copy_from_slice(&irp.address.to_le_bytes());
        frame[ID_AT..ID_AT + 8].copy_from_slice(&irp.id.to_le_bytes());
        if self
          .freed_by
          .get(&irp.address)
          .is_some_and(|by| *by != worker)
        {
          self.reused_across += 1;
        }
        assert!(self.processing.insert(irp.address, irp).is_none());
        at += REQUEST_BASE;
      }
      at
    }

    fn exhausted(&self) -> bool {
      self.arrived == IRPS && self.pending.is_empty()
    }
  }

  struct Crashed;

  /// Orders the workers' steps by `schedule` (each bit of a step's index picks a worker) and kills the daemon after
  /// `budget` steps; `None` lives on. A worker that has finished is skipped.
  struct Turnstile {
    state: Mutex<Gate>,
    turned: Condvar,
  }

  struct Gate {
    step: usize,
    budget: Option<usize>,
    crashed: bool,
    schedule: u64,
    finished: [bool; WORKERS],
  }

  impl Turnstile {
    fn new(budget: Option<usize>, schedule: u64) -> Self {
      Self {
        state: Mutex::new(Gate {
          step: 0,
          budget,
          crashed: false,
          schedule,
          finished: [false; WORKERS],
        }),
        turned: Condvar::new(),
      }
    }

    fn whose(gate: &Gate) -> usize {
      let preferred = usize::try_from((gate.schedule >> (gate.step % 61)) & 1).unwrap() % WORKERS;
      if gate.finished[preferred] {
        1 - preferred
      } else {
        preferred
      }
    }

    /// One step of `worker`: waits for its turn, then takes it, or reports the daemon dead.
    fn pass(&self, worker: usize) -> Result<(), Crashed> {
      let mut gate = self.state.lock().unwrap();
      loop {
        if gate.crashed {
          return Err(Crashed);
        }
        if Self::whose(&gate) == worker {
          break;
        }
        gate = self.turned.wait(gate).unwrap();
      }
      if gate.budget == Some(gate.step) {
        gate.crashed = true;
        self.turned.notify_all();
        return Err(Crashed);
      }
      gate.step += 1;
      self.turned.notify_all();
      Ok(())
    }

    /// One step taken alone, by recovery, which runs before any worker exists.
    fn pass_alone(&self) -> Result<(), Crashed> {
      let mut gate = self.state.lock().unwrap();
      if gate.crashed {
        return Err(Crashed);
      }
      if gate.budget == Some(gate.step) {
        gate.crashed = true;
        return Err(Crashed);
      }
      gate.step += 1;
      Ok(())
    }

    fn finish(&self, worker: usize) {
      let mut gate = self.state.lock().unwrap();
      gate.finished[worker] = true;
      self.turned.notify_all();
    }

    fn steps(&self) -> usize {
      self.state.lock().unwrap().step
    }

    fn crashed(&self) -> bool {
      self.state.lock().unwrap().crashed
    }
  }

  fn operate(request: &Request<'_>, world: &Mutex<World>) -> Vec<u8> {
    let id = u64::from_le_bytes(request.bytes()[ID_AT..ID_AT + 8].try_into().unwrap());
    let mut response = request.cancelled_response().unwrap().to_vec();
    response[STATUS_FIELD_AT..STATUS_FIELD_AT + 4].fill(0);
    response[INFORMATION_AT..INFORMATION_AT + 4]
      .copy_from_slice(&u32::try_from(id).unwrap().to_le_bytes());
    // A context unique to this open, never reused, as the ledger's generations make it.
    let context = id + 1;
    response[CONTEXT_AT..CONTEXT_AT + 8].copy_from_slice(&context.to_le_bytes());
    let mut world = world.lock().unwrap();
    assert!(
      world.unconfirmed.len() < WORKERS,
      "a worker recorded a second unconfirmed open"
    );
    world.unconfirmed.push((context, request.hint()));
    response
  }

  fn confirm(response: &[u8], world: &Mutex<World>) {
    if let Some((_, context)) = opened_context(response).unwrap() {
      let mut world = world.lock().unwrap();
      let before = world.unconfirmed.len();
      world.unconfirmed.retain(|(pending, _)| *pending != context);
      assert_eq!(
        world.unconfirmed.len() + 1,
        before,
        "a confirmed open was not unconfirmed"
      );
      world.confirmed.insert(context);
    }
  }

  /// What a worker does next: receive with no response, or take the batch its last combined call delivered.
  enum Next {
    Receive,
    Take(usize),
  }

  /// The driver's next stamp, the model's performance counter.
  fn stamp(world: &Mutex<World>) -> u64 {
    let mut world = world.lock().unwrap();
    world.clock += 1;
    world.clock
  }

  /// One worker's live serving, as the dispatcher does it: receive, stamp, then every response saved; each but the last
  /// sent alone and acknowledged, the last handed to the call that receives the next batch into the other buffer.
  fn serve(
    worker: usize,
    memory: &mut [u8],
    world: &Mutex<World>,
    gate: &Turnstile,
  ) -> Result<(), Crashed> {
    let mut fill = Fill::new();
    let mut next = Next::Receive;
    loop {
      gate.pass(worker)?;
      if matches!(next, Next::Receive) && world.lock().unwrap().exhausted() {
        return Ok(());
      }
      let mut batch = match next {
        Next::Receive => {
          let mut receiving = Capture::new(memory).unwrap().begin(&fill).unwrap();
          gate.pass(worker)?;
          let reported = {
            let mut world = world.lock().unwrap();
            let call = world.calls;
            world.calls += 1;
            world.arrive(call % 3);
            world.deliver(worker, receiving.buffer_mut())
          };
          gate.pass(worker)?;
          let stamp = stamp(world);
          receiving.received(reported, &mut fill, stamp).unwrap()
        }
        Next::Take(reported) => {
          let stamp = stamp(world);
          Capture::new(memory)
            .unwrap()
            .handed_off(reported, &mut fill, stamp)
            .unwrap()
        }
      };
      next = Next::Receive;
      while let Some(request) = batch.next_request().unwrap() {
        gate.pass(worker)?;
        let response = operate(&request, world);
        gate.pass(worker)?;
        batch.record_response(&request, &response).unwrap();
        if batch.is_last() {
          gate.pass(worker)?;
          let mut hand_off = batch.hand_off(&request, &mut fill).unwrap();
          gate.pass(worker)?;
          {
            let mut world = world.lock().unwrap();
            world.respond(worker, &response);
            let call = world.calls;
            world.calls += 1;
            world.arrive(1 - call % 2);
          }
          gate.pass(worker)?;
          let reported = {
            let (_, buffer) = hand_off.transaction();
            world.lock().unwrap().deliver(worker, buffer)
          };
          gate.pass(worker)?;
          confirm(&response, world);
          gate.pass(worker)?;
          next = Next::Take(reported);
          break;
        }
        gate.pass(worker)?;
        {
          let mut world = world.lock().unwrap();
          world.respond(worker, &response);
          let call = world.calls;
          world.calls += 1;
          world.arrive(1 - call % 2);
        }
        gate.pass(worker)?;
        confirm(&response, world);
        gate.pass(worker)?;
        batch.acknowledge(request.hint()).unwrap();
      }
    }
  }

  /// A-219's recovery over every journal, on one thread before any worker receives: the ledger keeps each journal's saved
  /// open and retires the rest; each address's newest delivery is answered, the older ones passed over.
  fn recover(
    memories: &mut [Vec<u8>],
    world: &Mutex<World>,
    gate: &Turnstile,
  ) -> Result<(), Crashed> {
    let mut retained = Vec::new();
    let mut journals = Vec::new();
    for memory in memories.iter() {
      let mut copy = memory.clone();
      if let Some(open) = Capture::new(&mut copy).unwrap().retained_open().unwrap() {
        retained.push(open);
      }
      match Capture::new(&mut copy).unwrap().resume().unwrap() {
        Some(batch) => {
          if batch.stamp().is_none() && !batch.hints().is_empty() {
            world.lock().unwrap().unstamped += 1;
          }
          journals.push((batch.stamp(), batch.hints()));
        }
        None => journals.push((None, Vec::new())),
      }
    }
    {
      let mut world = world.lock().unwrap();
      for (context, hint) in std::mem::take(&mut world.unconfirmed) {
        if retained.contains(&(hint, context)) {
          world.confirmed.insert(context);
        } else {
          world.retired.insert(context);
        }
      }
    }
    let passed = superseded(&journals).unwrap();
    gate.pass_alone()?;
    for (memory, passed) in memories.iter_mut().zip(passed) {
      let handed_off = matches!(
        Capture::new(memory).unwrap().landing().unwrap(),
        Landing::HandingOffUndelivered
      );
      let Some(mut batch) = Capture::new(memory).unwrap().resume().unwrap() else {
        continue;
      };
      while let Some(request) = batch.next_request().unwrap() {
        gate.pass_alone()?;
        if passed.contains(&request.hint()) {
          let mut world = world.lock().unwrap();
          world.passed_over += 1;
          if handed_off && batch.response_for(&request).unwrap().is_some() {
            world.passed_handed_off += 1;
          }
          drop(world);
          batch.pass_over(request.hint()).unwrap();
          continue;
        }
        let response = match batch.response_for(&request).unwrap() {
          Some(saved) => saved.to_vec(),
          None => {
            let cancelled = request.cancelled_response().unwrap().to_vec();
            batch.record_response(&request, &cancelled).unwrap();
            cancelled
          }
        };
        gate.pass_alone()?;
        world.lock().unwrap().respond(0, &response);
        gate.pass_alone()?;
        batch.acknowledge(request.hint()).unwrap();
      }
    }
    Ok(())
  }

  /// One daemon's life: recovery, then both workers serving, every step through `gate`.
  fn life(memories: &mut [Vec<u8>], world: &Mutex<World>, gate: &Turnstile) -> Result<(), Crashed> {
    recover(memories, world, gate)?;
    let results: Vec<Result<(), Crashed>> = std::thread::scope(|scope| {
      let handles: Vec<_> = memories
        .iter_mut()
        .enumerate()
        .map(|(worker, memory)| {
          scope.spawn(move || {
            let result = serve(worker, memory, world, gate);
            gate.finish(worker);
            result
          })
        })
        .collect();
      handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect()
    });
    if results.iter().any(Result::is_err) || gate.crashed() {
      return Err(Crashed);
    }
    Ok(())
  }

  #[derive(Default, Debug)]
  struct Tally {
    scenarios: u64,
    lost: u64,
    wrong: u64,
    twice: u64,
    wrong_context: u64,
    bogus: u64,
    reused_across: u64,
    passed_over: u64,
    unstamped: u64,
    passed_handed_off: u64,
  }

  /// Kills the daemon after each of `crashes` steps in turn, the last life living; returns that life's steps.
  fn scenario(schedule: u64, crashes: &[usize], tally: &mut Tally) -> usize {
    let mut memories: Vec<Vec<u8>> = (0..WORKERS).map(|_| vec![0; BYTES]).collect();
    let world = Mutex::new(World::default());
    for crash in crashes {
      let gate = Turnstile::new(
        Some(*crash),
        schedule.rotate_left(u32::try_from(*crash % 64).unwrap()),
      );
      if life(&mut memories, &world, &gate).is_ok() {
        break;
      }
    }
    let gate = Turnstile::new(None, schedule);
    assert!(life(&mut memories, &world, &gate).is_ok());
    let world = world.into_inner().unwrap();
    tally.scenarios += 1;
    tally.lost += IRPS - u64::try_from(world.completed.len()).unwrap();
    tally.wrong += world.wrong;
    tally.twice += world.twice;
    tally.bogus += world.bogus;
    tally.reused_across += world.reused_across;
    tally.passed_over += world.passed_over;
    tally.unstamped += world.unstamped;
    tally.passed_handed_off += world.passed_handed_off;
    for context in world.completed.values().flatten() {
      if !world.confirmed.contains(context) || world.retired.contains(context) {
        tally.wrong_context += 1;
      }
    }
    gate.steps()
  }

  /// A-219. Do: serve twelve IRPs on two workers under several schedules, killing the daemon at every step of every worker,
  /// and again with its successor killed at a step too; the last life lives. Expect: every IRP completed once with its own
  /// identity and a context the ledger kept, none lost; and the oracle saw what it guards: an address freed by one worker's
  /// response delivered to the other, requests passed over at recovery, unstamped batches, and a hand-off that delivered
  /// nothing whose saved response the other worker's newer delivery superseded.
  #[test]
  fn two_workers_answer_every_irp_once_with_its_own_response_whatever_step_the_daemon_dies_at() {
    let mut tally = Tally::default();
    for schedule in [
      0x5555_5555_5555_5555u64,
      0x3333_3333_3333_3333,
      0x0F0F_0F0F_0F0F_0F0F,
      0x9E37_79B9_7F4A_7C15,
    ] {
      let steps = scenario(schedule, &[], &mut tally);
      for first in 0..steps {
        scenario(schedule, &[first], &mut tally);
        for second in (0..steps).step_by(7) {
          scenario(schedule, &[first, second], &mut tally);
        }
      }
    }
    eprintln!("A-219 model: {tally:?}");
    assert_eq!(
      (tally.lost, tally.wrong, tally.twice, tally.wrong_context),
      (0, 0, 0, 0),
      "{tally:?}"
    );
    assert!(
      tally.reused_across > 0
        && tally.passed_over > 0
        && tally.unstamped > 0
        && tally.passed_handed_off > 0,
      "{tally:?}"
    );
  }

  /// A-219. Do: plan two journals whose batches share an address. Expect: the older delivery passed over, the newest kept;
  /// an unstamped batch newest; two unstamped or equal stamps refused.
  #[test]
  fn recovery_keeps_only_the_newest_delivery_at_an_address() {
    let passed = superseded(&[(Some(5), vec![0x1000, 0x2000]), (Some(9), vec![0x1000])]).unwrap();
    assert_eq!(passed, vec![BTreeSet::from([0x1000]), BTreeSet::new()]);
    let passed = superseded(&[(None, vec![0x1000]), (Some(9), vec![0x1000])]).unwrap();
    assert_eq!(passed, vec![BTreeSet::new(), BTreeSet::from([0x1000])]);
    assert!(superseded(&[(None, vec![0x1000]), (None, vec![0x1000])]).is_err());
    assert!(superseded(&[(Some(3), vec![0x1000]), (Some(3), vec![0x1000])]).is_err());
  }
}
