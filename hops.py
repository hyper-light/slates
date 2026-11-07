p='crates/server/src/live_ship.rs'
s=open(p).read()
def rep(o,n,c=1):
    global s
    assert s.count(o)==c,(o[:80],s.count(o))
    s=s.replace(o,n)
names=lambda h:'['+','.join([f'"live.hop.{h}.lt{b:04}us"' for b in (8,16,32,64,128,256,512,1024,2048)]+[f'"live.hop.{h}.ge2048us"',f'"live.hop.{h}.sum_ns"',f'"live.hop.{h}.n"'])+']'
table='const HOP_NAMES: [[&str; 12]; 5] = ['+','.join(names(h) for h in ('to_control','borrow','exchange','to_owner','round'))+'];'
rep('''struct Send {
  object: ObjectId,
  holder: HostId,
  stream: u64,
  bytes: Vec<u8>,
}''','''struct Send {
  object: ObjectId,
  holder: HostId,
  stream: u64,
  bytes: Vec<u8>,
  made_ns: u64, // TEMPORARY hop measurement
}

// TEMPORARY (item 2 hop measurement): counters `live.hop.<hop>.<bucket>`, `.sum_ns`, `.n`.
'''+table+'''

/// TEMPORARY: records `ns` for hop `hop` on this shard's counters.
fn hop(state: &mut ShardState, hop: usize, ns: u64) {
  let Some(names) = HOP_NAMES.get(hop) else {
    return;
  };
  let micros = ns / 1_000;
  let bounds: [u64; 9] = [8, 16, 32, 64, 128, 256, 512, 1024, 2048];
  let bucket = bounds.iter().position(|bound| micros < *bound).unwrap_or(9);
  if let Some(name) = names.get(bucket) {
    state.count(name, 1);
  }
  if let Some(name) = names.get(10) {
    state.count(name, ns);
  }
  if let Some(name) = names.get(11) {
    state.count(name, 1);
  }
}''')
rep('''            stream: slates_cluster::live::LIVE_ENTRY_STREAM,
            bytes: entry.encode(),
          });''','''            stream: slates_cluster::live::LIVE_ENTRY_STREAM,
            bytes: entry.encode(),
            made_ns: slates_machine::clock::monotonic_ns(),
          });''')
rep('''          stream: slates_cluster::live::LIVE_STATE_STREAM,
          bytes,
        });''','''          stream: slates_cluster::live::LIVE_STATE_STREAM,
          bytes,
          made_ns: slates_machine::clock::monotonic_ns(),
        });''')
rep('''  for send in sends {
    let sessions = borrow_session(holder, budget.max_deadline_ns()).await;''','''  for send in sends {
    let t_lane = slates_machine::clock::monotonic_ns();
    let sessions = borrow_session(holder, budget.max_deadline_ns()).await;
    let t_borrowed = slates_machine::clock::monotonic_ns();''')
rep('''    let answered = !exchanged.replies.is_empty();
    if !answered {''','''    let answered = !exchanged.replies.is_empty();
    let t_exchanged = slates_machine::clock::monotonic_ns();
    let made_ns = send.made_ns;
    let _ = state::with_state(|s| {
      hop(s, 0, t_lane.saturating_sub(made_ns));
      hop(s, 1, t_borrowed.saturating_sub(t_lane));
      hop(s, 2, t_exchanged.saturating_sub(t_borrowed));
    });
    if !answered {''')
rep('''      crate::xshard::run_on_counted(control, origin, move |s| fold_into(s, replies));
    }
    // After the answers are on their way''','''      crate::xshard::run_on_counted(control, origin, move |s| {
        let now = slates_machine::clock::monotonic_ns();
        hop(s, 3, now.saturating_sub(t_exchanged));
        fold_into(s, replies);
        hop(s, 4, slates_machine::clock::monotonic_ns().saturating_sub(made_ns));
      });
    }
    // After the answers are on their way''')
open(p,'w').write(s)
