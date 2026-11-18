# slates — Node / TypeScript SDK

A thin [napi-rs](https://napi.rs) addon over the typed Rust `slates` client (design §2.3, D-19):
your Node or TypeScript agent drives a slates daemon through the *same* rings and completion records
the Rust client uses — never a parallel reimplementation. Every method is one client call; a typed
refusal from the daemon becomes a JS `Error`, volume ids cross as lowercase hex, and every integer
that crosses is range-checked (a JS number is an `f64`; a value that would not round-trip is refused,
not truncated).

Two clients over one daemon: `AsyncClient` is the **async-primary** form (R6) — every verb a Promise
resolved by the completion descriptor's readiness, never blocking the event loop — and `Client` is
the thin synchronous facade.

## Install

```
npm install @hyper-light/slates
```

`@hyper-light/slates` ships a prebuilt native addon per platform (macOS arm64; Linux glibc and
musl, x64/arm64; Windows x64/arm64), resolved for your `process.platform`/`arch` as an
`optionalDependency` (`@hyper-light/slates-<platform>`) — no build toolchain is needed to install.
Node 18 or newer. From a source checkout instead: `npm run build` (napi; needs a Rust toolchain)
produces the platform `.node` next to `index.js`, which the loader prefers. How a release is cut and
published is in `docs/publish.md`.

## Connect

```js
import { Client, AsyncClient } from '@hyper-light/slates';

// replyNs / reconnectNs are nanosecond deadlines a production caller derives from the machine's
// budgets. The instance is the daemon `slates anchor --instance <name>` published.
const client = Client.connect('default', 5_000_000, 10_000_000);
```

`AsyncClient` is the **async-primary** form — the same verbs, each a Promise resolved on the event
loop by the completion fd's readiness (never blocking it), with the sync `Client` as the thin facade:

```js
const async = await AsyncClient.connect('default', 5_000_000, 10_000_000);
const volume = await async.create('scratch', 8 * 1024 * 1024);
const status = await async.status(volume);
```

Connecting where no daemon answers throws (`AsyncClient.connect` rejects; it never holds the event
loop while the daemon answers). A call waits while its daemon lives, however long a busy machine keeps
the daemon off its CPU: a daemon that will never answer is ended (by its anchor, or by its own watchdog
when it runs alone), and its calls are then resent to the restarted daemon. Every async call ends: answered,
refused typed, `DaemonGone` when the daemon is gone and no restart answers within the reconnect budget, or
`CompletionLost` when the completion reader fails; `client.cancel(promise)` releases a waiting call
and rejects it with an `AbortError`. Calls past the client's bound (its outstanding limit in
flight, as many again queued) are refused `TooManyOutstanding` at once. Use a client from the thread that connected it.

## Volume lifecycle

```js
const volume = client.create('scratch', 8 * 1024 * 1024); // a bounded 8 MiB RAM volume; a hex id
const status = client.status(volume);                     // { placed, attachments, nfsPort, drifted, … }
for (const v of client.list()) console.log(v.name);       // { id, name, referencedBytes, overlay, … }
client.snapshot(volume);
client.resize(volume, 16 * 1024 * 1024);
client.destroy(volume);
```

## The merge workflow (§4.16)

Provision a shared **green**, clone a **work** volume, edit it, and submit — conflicts come back as
byte-exact windows to rebase against, never a silent interleave.

```js
const green = client.createGreen('main');
const work = client.createWork(green, 'feature');          // { id, base: <green version> }

// content edits: a splice at `at` — remove `deleteLen` bytes, insert `data` (a Buffer).
client.edit(work.id, '/notes.txt', 0, 0, Buffer.from('hello'));

// namespace operations, the counterpart to a content edit:
client.mkdir(work.id, '/dir');
client.rename(work.id, '/notes.txt', '/dir/notes.txt');
client.chmod(work.id, '/dir/notes.txt', 0o600);
client.symlink(work.id, '/dir/link', 'notes.txt');
client.link(work.id, '/dir/hard.txt', '/dir/notes.txt');
client.setXattr(work.id, '/dir/notes.txt', 'user.tag', Buffer.from('v'));
// also: unlink, rmdir, removeXattr

const outcome = client.submit(work.id);
if (outcome.ok) {
  console.log('landed at version', outcome.version);
} else {
  for (const w of outcome.conflicts) console.log('conflict at', w.path, w.at, w.len);
  // resolve, then client.rebase(work.id) and submit again
}

console.log(client.versions(green));            // the green's head version
console.log(client.changedSince(green, 0));     // paths changed since a version
```

## A volume at a path for one command (`slates exec`, Linux)

`execCommand` builds the command that runs slates' chosen-path launcher: the volume shown at an existing directory for
one command, in a namespace of its own. Spawn it as any command; the launcher finds the volume under `SLATES_ROOT` in
the environment you give it (the daemon's root mount):

```js
import { spawn } from 'node:child_process';
import { execCommand, apparmorProfile } from '@hyper-light/slates';

const argv = execCommand('scratch', '/home/u/proj/build', ['cargo', 'build']);
// ['slates', 'exec', '--volume=scratch', '--at=/home/u/proj/build', '--', 'cargo', 'build']
spawn(argv[0], argv.slice(1), { env: { ...process.env, SLATES_ROOT: '/run/user/1000/slates' }, stdio: 'inherit' });
```

`{ program }` names the `slates` binary when it is not the one on the `PATH`.

**AppArmor (Ubuntu 24.04 and later).** Where AppArmor restricts unprivileged user namespaces
(`kernel.apparmor_restrict_unprivileged_userns=1`), the launcher needs slates' AppArmor profile for the binary, which an
administrator loads once, as root, and you opt into it per command with `{ apparmorProfile: true }` (off unless set):

```js
console.log(apparmorProfile('/usr/local/bin/slates')); // the profile: save it as /etc/apparmor.d/usr.local.bin.slates
const asked = execCommand('scratch', '/home/u/proj/build', ['cargo', 'build'], {
  apparmorProfile: true,
  program: '/usr/local/bin/slates',
});
```

With the option, the launcher makes its namespace under the profile and runs the command outside it. Without it, the
launcher never uses the profile, even where it is loaded. `slates apparmor check` on the host says which applies and
prints the root commands that load or remove the profile (`docs/cli.md`, "The launcher and AppArmor").

## No grants (R10)

There is no verb here that creates a landing grant — landing to a real disk is a human-only act at
the CLI or a confirmation surface, never something an agent answers for itself.
