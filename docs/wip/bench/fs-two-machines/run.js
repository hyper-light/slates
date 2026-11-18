// The filesystem server on a second machine (fs-server plan step 3; R8, R10, §4.10, A-201, A-285, A-299): the server
// node on one kiln runner and the client node on another, from one fleet manifest over the LAN. The client reads the
// server's greens and pushes two increments over four files into each, and a late push that overlaps the first is
// refused with its window; the server checks each head out and lands it onto its own machine's project path under its
// launcher's write policy: one project under ask (the approval request first, nothing written, then the launcher's
// approval of exactly the presented manifest), the other under bypass (written in the one `slates land`). No person
// takes part. The per-machine steps are `crates/cli/tests/fs_server.rs`'s roles (`role`: `mint`, `unpack`, `node`,
// `ask`, `bypass`, `approve`) and the `slates` CLI; the launcher's secret is made on the server, kept there (0600) and
// read by its roles on standard input, never sent.
//
// A kiln `code_run` program: the body of an async function, given `input` and `kiln`. Paste this file as the program.
// input:
//   server, client  kiln runners by name (one Linux and one macOS for the plan's step).
//   sb, cb          on the server and on the client: {slates, testbin}, the `slates` binary and the `fs_server` test
//                   binary, built at one commit on both machines (build.sh beside this file prints both paths).
//   sPort, cPort    each node's base UDP port on its own machine: probes there, records on the next port
//                   (`slates_server::deploy`). A block its machine's firewall admits from the other machine and that
//                   nothing binds; the program checks the latter and fails the run's first phase otherwise.
//   tag             names the run's instances, systemd units and scratch directory ($HOME/.cache/fs-two-TAG).
//   capped          runners whose house rules cap every command (adacachy): each command runs in its own
//                   `systemd-run --user` unit under the box's caps.
//   lan             optional: a LAN address a Linux runner reads its source address from. Without it, a runner with
//                   no macOS interface address takes the route toward the other runner's address.
// Returns {passed, verdict, phases, addresses, asked, bypassed, cleanup, ...}: `passed` is the run's exit status,
// `verdict` each acceptance check, `phases` the seconds each phase took.
const {server: S, client: C, sb, cb, tag, sPort, cPort} = input;
const capped = new Set(input.capped || []);
const D = `$HOME/.cache/fs-two-${tag}`;
const sInst = `fs-two-${tag}-server`;
const cInst = `fs-two-${tag}-client`;
// Shape: how long a node may take to answer, the root group to bootstrap and a client's first forwarded read to be
// served, in seconds (a capped Linux box under its CPU weight of 1 boots a debug daemon in tens of seconds).
const NODE_UP_S = 120;
const BOOTSTRAP_S = 120;
const FIRST_READ_S = 120;
// Shape: how long a node's background run may live at most; the program stops both long before.
const NODE_RUN_S = 3000;
// Shape: how long the cleanup waits for the anchors to stop their daemons after `SIGTERM` before killing what is left.
const STOP_WAIT_S = 30;

const q = (text) => `'${String(text).replace(/'/g, `'\\''`)}'`;
let unitSeq = 0;
// A capped machine's command runs in its own transient user unit (adacachy's caps); `scratch` gives it a TMPDIR inside
// the run's directory, which the cleanup removes. Every `$` is passed as `$$`: the service manager substitutes
// `${NAME}` anywhere in a unit's command (systemd.service(5), "Command lines"), so `"${f#./}"` reached bash empty
// (systemd 262 on adacachy, 2026-11-18), and `$$` is its literal dollar.
const wrap = (machine, command, unitName, scratch = true) => {
  if (!capped.has(machine)) return command;
  unitSeq += 1;
  const unit = unitName || `fs-two-${tag}-${unitSeq}`;
  const made = scratch ? `mkdir -p "${D}/tmp" && ` : '';
  const tmp = scratch ? ` -E TMPDIR="${D}/tmp"` : '';
  return `${made}systemd-run --user --quiet --wait --pipe --collect --unit=${unit} -p MemoryMax=8G -p CPUWeight=1 -p Nice=19 -p 'IOWriteBandwidthMax=/dev/nvme1n1 150M' -p 'IOReadBandwidthMax=/dev/nvme1n1 400M'${tmp} -E HOME="$HOME" -E PATH=/usr/local/bin:/usr/bin:/bin bash -c ${q(command.replace(/\$/g, '$$$$'))}`;
};
const marks = [];
const t0 = Date.now();
const note = (what, extra) => marks.push({at_s: Math.round((Date.now() - t0) / 100) / 10, what, ...(extra || {})});

const on = (machine, command, timeout_s = 300, unitName, scratch) =>
  kiln.machines.run({on: machine, command: wrap(machine, command, unitName, scratch), sync: false, timeout_s});
const must = async (machine, command, what, timeout_s) => {
  const r = await on(machine, command, timeout_s);
  if (r.exit_code !== 0) {
    throw new Error(`${what} failed on ${machine} (exit ${r.exit_code}): ${String(r.output).slice(-2500)}`);
  }
  return String(r.output);
};
// A role's report lines: the tag is found anywhere in a line, since libtest's `test role ... ` comes first on it.
const tagged = (out) => out.split('\n').map((l) => {
  const i = l.indexOf('FS-ROLE:');
  return i >= 0 ? l.slice(i + 8).trim() : null;
}).filter((x) => x !== null);
const role = (bin, name, envs, stdin) =>
  `SLATES_FS_ROLE=${name} ${envs} "${bin}" --exact role --nocapture --test-threads 1 ${stdin || '< /dev/null'}`;
const sl = (bin, inst, args) => `"${bin}" --instance ${inst} ${args}`;
const json = (out, what) => {
  const line = out.split('\n').map((l) => l.trim()).filter((l) => l.startsWith('{')).pop();
  if (line === undefined) throw new Error(`${what}: no JSON answer in: ${out.slice(-1500)}`);
  return JSON.parse(line);
};
// Retries a verb until it answers or `secs` pass: a bound in time, not in tries, since one refused verb may wait out
// its own deadline. Prints the answer, or the last refusal.
const poll = (bin, inst, args, secs) =>
  `end=$(( $(date +%s) + ${secs} )); while :; do out=$(${sl(bin, inst, args)} 2>&1) && { printf '%s\\n' "$out"; exit 0; }; [ "$(date +%s)" -lt "$end" ] || break; sleep 0.25; done; printf '%s\\n' "$out"; exit 1`;

// A node's address, and its port block checked free there: each port bound on the address and let go.
const PORT_CHECK = [
  'import socket, sys',
  'held = []',
  'for port in sys.argv[2:]:',
  '    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)',
  '    s.bind((sys.argv[1], int(port)))',
  '    held.append(s)',
  'print("PORTS_FREE " + " ".join(sys.argv[2:]))',
].join('\n');
const addressCmd = (port, toward) => [
  `mkdir -p ${D}/fleet ${D}/projects || exit 6`,
  'ip=$( (ipconfig getifaddr en0 || ipconfig getifaddr en1) 2>/dev/null )',
  ...(toward ? [`[ -n "$ip" ] || ip=$(ip -4 route get ${toward} 2>/dev/null | sed -nE 's/.* src ([0-9.]+).*/\\1/p')`] : []),
  'echo "IP=$ip"; echo "UID=$(id -u)"',
  `[ -z "$ip" ] || python3 -c ${q(PORT_CHECK)} "$ip" ${port} ${port + 1} || exit 7`,
  'echo "HOST=$(uname -sm) $(hostname)"',
  'echo "LOAD=$(uptime | sed -E \'s/.*load averages?: //\')"',
].join('\n');
const field = (out, k) => (out.match(new RegExp(`^${k}=(.*)$`, 'm')) || [])[1] || '';

// Every regular file under a project, by path relative to it in byte order, with its bytes: what the disk holds.
const files = (name) => `cd ${D}/projects/${name} && find . -type f | LC_ALL=C sort | while IFS= read -r f; do printf '%s=' "\${f#./}"; cat "$f"; echo; done`;
const merged = ["README=readme original + the client's line", 'src/new.txt=created by the client', 'src/one.txt=ONE: original line', 'src/two.txt=TWO: original line'];
const seeded = ['README=readme original', 'src/one.txt=one: original line', 'src/two.txt=two: original line'];
const lines = (out) => out.trim().split('\n');
const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);
// A node's fleet in brief, from its `slates status`: the mesh, the sessions re-dialed, the configuration it holds, its
// learner's last fetch, each record session's path (round trip, losses) and the fleet's refusal counters.
const brief = (status) => {
  const all = status.split('\n');
  const out = Object.fromEntries(all
    .map((l) => l.match(/^fleet_(peers_probed|replaced|unknown_id|inbox_full|sessions_refused|configuration_version|configuration_members|learner): (.*)$/))
    .filter((m) => m).map((m) => [m[1], m[2]]));
  out.sessions = all.filter((l) => l.startsWith('fleet_session ')).map((l) => l.slice(14, 240));
  out.refused = all.filter((l) => / refused (fleet|forward|owner)/.test(l)).map((l) => l.slice(0, 160));
  return out;
};

let sNode = null;
let cNode = null;
const result = {tag, server: S, client: C, ports: {server: sPort, client: cPort}, binaries: {server: sb, client: cb}};
try {
  // Addresses: each machine's LAN address and its port block, free.
  let [sInfo, cInfo] = await Promise.all([must(S, addressCmd(sPort, input.lan), 'address'), must(C, addressCmd(cPort, input.lan), 'address')]);
  if (!field(sInfo, 'IP') && field(cInfo, 'IP')) sInfo = await must(S, addressCmd(sPort, field(cInfo, 'IP')), 'address');
  if (!field(cInfo, 'IP') && field(sInfo, 'IP')) cInfo = await must(C, addressCmd(cPort, field(sInfo, 'IP')), 'address');
  const sIp = field(sInfo, 'IP');
  const cIp = field(cInfo, 'IP');
  if (!sIp || !cIp) throw new Error(`no LAN address: server ${sInfo} client ${cInfo}`);
  const cUid = field(cInfo, 'UID');
  result.addresses = {server: `${sIp}:${sPort}`, client: `${cIp}:${cPort}`, server_host: field(sInfo, 'HOST'), client_host: field(cInfo, 'HOST'), server_load: field(sInfo, 'LOAD'), client_load: field(cInfo, 'LOAD')};
  note('addresses and free ports', result.addresses);

  // Identities minted on the server, handed to the client; one manifest on both.
  const minted = tagged(await must(S, role(sb.testbin, 'mint', `SLATES_FS_ROLE_DIR=${D}/fleet`), 'mint'));
  const identityFiles = minted.filter((l) => l.startsWith('file ')).map((l) => l.slice(5));
  if (identityFiles.length !== 4) throw new Error(`mint gave ${identityFiles.length} files`);
  const unpacked = tagged(await must(C, `cat <<'FILES' | SLATES_FS_ROLE=unpack SLATES_FS_ROLE_DIR=${D}/fleet "${cb.testbin}" --exact role --nocapture --test-threads 1\n${identityFiles.join('\n')}\nFILES`, 'unpack'));
  const manifest = JSON.stringify({name: 'slates-fs-server', f: 0, nodes: [
    {node: 'server', address: `${sIp}:${sPort}`, certificate: 'server.crt.der', key: 'server.key.der'},
    {node: 'client', address: `${cIp}:${cPort}`, certificate: 'client.crt.der', key: 'client.key.der'},
  ]});
  await Promise.all([S, C].map((m) => must(m, `cat > ${D}/fleet/fleet.json <<'JSON'\n${manifest}\nJSON`, 'manifest')));
  // The launcher's secret: made on the server, kept there (0600), read by its roles on standard input.
  await must(S, `umask 077; head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \\n' > ${D}/launcher.hex; wc -c < ${D}/launcher.hex`, 'secret');
  note('identities, manifest and secret', {unpacked});

  // The two nodes, each kept by a background run.
  const nodeEnv = (inst, node, slates) => `SLATES_FS_ROLE_INSTANCE=${inst} SLATES_FS_ROLE_MANIFEST=${D}/fleet/fleet.json SLATES_FS_ROLE_NODE=${node} SLATES_FS_ROLE_SLATES="${slates}"`;
  sNode = await kiln.machines.start({on: S, sync: false, timeout_s: NODE_RUN_S, command: wrap(S, role(sb.testbin, 'node', nodeEnv(sInst, 'server', sb.slates), `< ${D}/launcher.hex`))});
  cNode = await kiln.machines.start({on: C, sync: false, timeout_s: NODE_RUN_S, command: wrap(C, role(cb.testbin, 'node', nodeEnv(cInst, 'client', cb.slates)))});
  await Promise.all([
    must(S, poll(sb.slates, sInst, 'volume list', NODE_UP_S), 'server up', NODE_UP_S + 120),
    must(C, poll(cb.slates, cInst, 'volume list', NODE_UP_S), 'client up', NODE_UP_S + 120),
  ]);
  note('nodes up', {server_run: sNode.id, client_run: cNode.id});
  await must(S, poll(sb.slates, sInst, 'bootstrap root', BOOTSTRAP_S), 'bootstrap', BOOTSTRAP_S + 120);
  const [sStatus, cStatus] = await Promise.all([must(S, sl(sb.slates, sInst, 'status'), 'server status'), must(C, sl(cb.slates, cInst, 'status'), 'client status')]);
  const cAnchor = (cStatus.match(/^anchor: (\S+)/m) || [])[1];
  result.fleet_after_bootstrap = {server: brief(sStatus), client: brief(cStatus)};
  note('root bootstrapped');

  const flows = {};
  for (const name of ['asked', 'bypassed']) {
    const P = `${D}/projects/${name}`;
    await must(S, `mkdir -p ${P}/src && printf '%s' 'readme original' > ${P}/README && printf '%s' 'one: original line' > ${P}/src/one.txt && printf '%s' 'two: original line' > ${P}/src/two.txt`, 'seed');
    const volume = json(await must(S, sl(sb.slates, sInst, `volume create ${name} --dynamic 64MiB --base ${P} --json`), 'volume'), 'volume').id;
    await must(S, sl(sb.slates, sInst, `base pin ${volume}`), 'pin');
    const snapshot = String(json(await must(S, sl(sb.slates, sInst, `volume snapshot ${volume} --json`), 'snapshot'), 'snapshot').snapshot);
    const green = json(await must(S, sl(sb.slates, sInst, `green ${name}-green --base ${volume} --snapshot ${snapshot} --json`), 'green'), 'green').id;
    await must(S, sl(sb.slates, sInst, `share ${green} uid:${cUid}@${cAnchor} --read --write --admin`), 'share');
    // The client's pushes, through the fleet's forward.
    const read = await must(C, poll(cb.slates, cInst, `read ${green} src/one.txt`, FIRST_READ_S), 'client read', FIRST_READ_S + 120);
    note(`${name}: the client read the server's green`);
    const work = async (wname, edits) => {
      const w = json(await must(C, sl(cb.slates, cInst, `work ${green} ${wname} --json`), 'work'), 'work').id;
      for (const [path, at, del, text] of edits) {
        await must(C, sl(cb.slates, cInst, `edit ${w} ${path} ${at} ${del} ${q(text)}`), 'edit');
      }
      return w;
    };
    const late = await work('late', [['src/one.txt', 0, 3, 'Uno']]);
    const first = await work('first', [['src/one.txt', 0, 3, 'ONE'], ['src/new.txt', 0, 0, 'created by the client']]);
    const a1 = (await must(C, sl(cb.slates, cInst, `submit ${first}`), 'submit 1')).trim();
    const second = await work('second', [['README', 15, 0, " + the client's line"], ['src/two.txt', 0, 3, 'TWO']]);
    const a2 = (await must(C, sl(cb.slates, cInst, `submit ${second}`), 'submit 2')).trim();
    const refused = json(await must(C, sl(cb.slates, cInst, `submit ${late} --json`), 'submit late'), 'submit late');
    // The server checks the head out.
    const attachment = String(json(await must(S, sl(sb.slates, sInst, `attach ${volume} --write --json`), 'attach'), 'attach').attachment);
    const checkout = json(await must(S, sl(sb.slates, sInst, `checkout ${green} ${volume} ${attachment} --snapshot ${snapshot} --json`), 'checkout'), 'checkout');
    flows[name] = {volume, green, read: lines(read).pop(), accepted: [a1, a2], late: refused, checkout};
    note(`${name}: pushed and checked out`, {accepted: [a1, a2], checkout});
  }

  const roleEnv = (vol, target) => `SLATES_FS_ROLE_INSTANCE=${sInst} SLATES_FS_ROLE_VOLUME=${vol} SLATES_FS_ROLE_TARGET=${target}`;
  // Ask: the approval request, nothing written, then the launcher's approval of exactly that manifest.
  const asked = flows.asked;
  const askSet = tagged(await must(S, role(sb.testbin, 'ask', roleEnv(asked.volume, `${D}/projects/asked`), `< ${D}/launcher.hex`), 'ask role'));
  const presented = json(await must(S, sl(sb.slates, sInst, `land ${asked.volume} ${D}/projects/asked --json`), 'land asked'), 'land asked');
  const before = await must(S, files('asked'), 'read asked before');
  const approved = tagged(await must(S, role(sb.testbin, 'approve', roleEnv(asked.volume, `${D}/projects/asked`), `< ${D}/launcher.hex`), 'approve role'));
  const after = await must(S, files('asked'), 'read asked after');
  result.asked = {set: askSet, presented: {grant_required: presented.grant_required, by_action: presented.summary && presented.summary.by_action, conflicts: presented.conflicts}, before: lines(before), approved, after: lines(after), flow: flows.asked};
  note('asked: presented, approved and landed', {approved});
  // Bypass: written in the one land.
  const bypassed = flows.bypassed;
  const bypassSet = tagged(await must(S, role(sb.testbin, 'bypass', roleEnv(bypassed.volume, `${D}/projects/bypassed`), `< ${D}/launcher.hex`), 'bypass role'));
  const landed = json(await must(S, sl(sb.slates, sInst, `land ${bypassed.volume} ${D}/projects/bypassed --json`), 'land bypassed'), 'land bypassed');
  const bypassedFiles = await must(S, files('bypassed'), 'read bypassed');
  result.bypassed = {set: bypassSet, landed: {grant_required: landed.grant_required, written: landed.outcome && landed.outcome.written, failed: landed.outcome && landed.outcome.failed, bytes: landed.outcome && landed.outcome.bytes_written}, files: lines(bypassedFiles), flow: flows.bypassed};
  note('bypassed: landed in one call');
  const late = (f) => same(f.late, {accepted: false, conflicts: [{at: 0, class: 0, len: 3, path: 'src/one.txt'}]});
  result.verdict = {
    asked_presented_unwritten: presented.grant_required === true && same(result.asked.before, seeded),
    asked_approved_written_exactly_the_merge: approved.includes('written=4 failed=0') && same(result.asked.after, merged),
    bypassed_written_at_once_exactly_the_merge: landed.grant_required === false && result.bypassed.landed.written === 4 && result.bypassed.landed.failed === 0 && same(result.bypassed.files, merged),
    late_pushes_refused_with_their_window: late(flows.asked) && late(flows.bypassed),
    accepted_in_order: flows.asked.accepted.join() === 'accepted: 1,accepted: 2' && flows.bypassed.accepted.join() === 'accepted: 1,accepted: 2',
  };
  result.passed = Object.values(result.verdict).every((v) => v === true);
  const [sEnd, cEnd] = await Promise.all([on(S, sl(sb.slates, sInst, 'status'), 120), on(C, sl(cb.slates, cInst, 'status'), 120)]);
  result.fleet_at_end = {server: brief(String(sEnd.output)), client: brief(String(cEnd.output))};
} catch (e) {
  result.error = String(e && e.message || e).slice(0, 4000);
  result.passed = false;
  // What each node saw, for the diagnosis: its whole status.
  const fleetLines = async (machine, bin, inst) => {
    try {
      const r = await on(machine, sl(bin, inst, 'status'), 120);
      return String(r.output).split('\n').map((l) => l.slice(0, 240)).slice(0, 600);
    } catch (err) {
      return [String(err && err.message || err).slice(0, 300)];
    }
  };
  if (sNode || cNode) {
    [result.server_status, result.client_status] = await Promise.all([fleetLines(S, sb.slates, sInst), fleetLines(C, cb.slates, cInst)]);
  }
} finally {
  note('flow ended');
  // The anchors are asked to stop (their daemons with them), then whatever is left is killed, the units stopped and the
  // run's directory removed; each machine then reports what is left. The pattern is split across two variables so the
  // cleanup's own command line never matches it.
  const cleanup = [
    `a=--instance; b=fs-two-${tag}-`,
    'pkill -TERM -f -- "$a $b" 2>/dev/null',
    `end=$(( $(date +%s) + ${STOP_WAIT_S} )); while pgrep -f -- "$a $b" >/dev/null && [ "$(date +%s)" -lt "$end" ]; do sleep 0.25; done`,
    'pkill -KILL -f -- "$a $b" 2>/dev/null',
    `command -v systemctl >/dev/null && systemctl --user stop "fs-two-${tag}-*" 2>/dev/null`,
    `rm -rf ${D}`,
    'echo "LEFT_PROCESSES=$(pgrep -f -- "$a $b" | wc -l | tr -d " ")"',
    `[ -e ${D} ] && echo LEFT_DIR=yes || echo LEFT_DIR=no`,
    `if command -v systemctl >/dev/null; then echo "LEFT_UNITS=$(systemctl --user list-units --all --no-legend --plain "fs-two-${tag}-*" | wc -l | tr -d " ")"; else echo LEFT_UNITS=0; fi`,
  ].join('\n');
  const left = async (machine) => {
    try {
      const out = String((await on(machine, cleanup, STOP_WAIT_S + 270, `fs-two-cleanup-${tag}`, false)).output);
      return {processes: Number(field(out, 'LEFT_PROCESSES')), dir: field(out, 'LEFT_DIR'), units: Number(field(out, 'LEFT_UNITS'))};
    } catch (e) {
      return {error: String(e && e.message || e).slice(0, 300)};
    }
  };
  const [sLeft, cLeft] = await Promise.all([left(S), left(C)]);
  // Each node's log, read before its run is stopped (a stopped run's output is gone): the role's lines and the
  // daemon's warnings when the run passed, its whole tail when it failed.
  const tail = async (run) => {
    if (!run || !run.id) return [];
    try {
      const o = await kiln.machines.output({id: run.id, wait_s: 10});
      const all = String(o.output || '').split('\n').map((l) => l.slice(0, 400));
      return result.passed ? all.filter((l) => /slates (anchor|daemon)|FS-ROLE|panicked|error|refused|warn/i.test(l)).slice(-16) : all.slice(-400);
    } catch (e) {
      return [`no output: ${String(e && e.message || e).slice(0, 200)}`];
    }
  };
  [result.server_node_log_tail, result.client_node_log_tail] = await Promise.all([tail(sNode), tail(cNode)]);
  for (const run of [sNode, cNode]) {
    if (run && run.id) {
      try { await kiln.machines.stop({id: run.id}); } catch (e) { /* already ended */ }
    }
  }
  const clean = (l) => l.processes === 0 && l.dir === 'no' && l.units === 0;
  result.cleanup = {server: sLeft, client: cLeft, clean: clean(sLeft) && clean(cLeft)};
  note('cleaned up');
  result.marks = marks;
  result.phases = marks.map((m, k) => ({phase: m.what, s: Math.round((m.at_s - (k > 0 ? marks[k - 1].at_s : 0)) * 10) / 10}));
  result.total_s = Math.round((Date.now() - t0) / 1000);
}
return result;
