p='crates/server/src/live_ship.rs'
s=open(p).read()
def rep(o,n,c=1):
    global s
    assert s.count(o)==c,(o[:80],s.count(o))
    s=s.replace(o,n)
rep('''/// Counter: entries a holder refused to keep (over its bound, unauthorized, malformed).''','''/// Notes live-shipping work as client activity on this shard (§4.7 "Shards poll rings while any client has activity
/// within the measured idle window"): the shard spins out its idle window before it parks, so the next message of a
/// live volume's write stream, each write's own, costs no kernel wake. Without it a candidate's control shard parked
/// after every entry it kept and the owner's after every exchange it sent, and each stable live write paid a readiness
/// wake on both: the exchange took 122–133 µs at the median-weighted mean on CI's x86_64 runner against about 25 µs for
/// the same exchange inside one process (`replication_bench`; diag run diag-live-durability-3, 2026-11-07).
fn note_live_activity() {
  slates_rt::registry::with_current(|ctx| ctx.note_activity());
}

/// Counter: entries a holder refused to keep (over its bound, unauthorized, malformed).''')
# owner: after each exchange in ship_lane
rep('''    let answered = !exchanged.replies.is_empty();
''','''    let answered = !exchanged.replies.is_empty();
    note_live_activity();
''')
open(p,'w').write(s)
s=open(p).read()
rep('''  request: &[u8],
) -> Vec<u8> {
  let object = slates_db::live::object_of(request);
  let bound = match admitted(state, local, peer, object) {''','''  request: &[u8],
) -> Vec<u8> {
  note_live_activity();
  let object = slates_db::live::object_of(request);
  let bound = match admitted(state, local, peer, object) {''')
rep('''  request: &[u8],
) -> Vec<u8> {
  if slates_db::live::is_fetch(request) {''','''  request: &[u8],
) -> Vec<u8> {
  note_live_activity();
  if slates_db::live::is_fetch(request) {''')
open(p,'w').write(s)
