# slates — Node / TypeScript SDK

A thin [napi-rs](https://napi.rs) addon over the typed Rust `slates` client (design §2.3, D-19):
your Node or TypeScript agent drives a slates daemon through the *same* rings and completion records
the Rust client uses — never a parallel reimplementation. Every method is one client call; a typed
refusal from the daemon becomes a JS `Error`, volume ids cross as lowercase hex, and every integer
that crosses is range-checked (a JS number is an `f64`; a value that would not round-trip is refused,
not truncated).

This is the **synchronous** base the design says the async form wraps (R6); the async form (over the
completion descriptor, as Promises) is owed.

## Install

```
npm install slates
```

`slates` ships a prebuilt native addon per platform (macOS, Linux, Windows), resolved for your
`process.platform`/`arch` as an `optionalDependency` — no build toolchain is needed to install. From
a source checkout instead: `cargo build -p slates-sdk-node`, then `npm run build` (napi) produces the
platform `.node`.

## Connect

```js
import { Client, AsyncClient } from 'slates';

// replyNs / reconnectNs are nanosecond deadlines a production caller derives from the machine's
// budgets. The instance is the daemon `slates anchor --instance <name>` published.
const client = Client.connect('default', 5_000_000, 10_000_000);
```

`AsyncClient` is the **async-primary** form — the same verbs, each a Promise resolved on the event
loop by the completion fd's readiness (never blocking it), with the sync `Client` as the thin facade:

```js
const async = AsyncClient.connect('default', 5_000_000, 10_000_000);
const volume = await async.create('scratch', 8 * 1024 * 1024);
const status = await async.status(volume);
```

Connecting where no daemon answers throws. Use a client from the thread that connected it.

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

## No grants (R10)

There is no verb here that creates a landing grant — landing to a real disk is a human-only act at
the CLI or a confirmation surface, never something an agent answers for itself.
