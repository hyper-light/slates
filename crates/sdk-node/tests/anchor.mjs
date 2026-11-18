// The anchor a Node SDK suite starts, and its end: one start and one teardown shared by `sdk.test.mjs`,
// `sdk_async.test.mjs` and `packaged.test.mjs` (which is copied beside this file wherever it runs alone). Not a test
// file itself: `node --test` is given the suites by name.
//
// Every suite starts a real anchor, which starts and supervises a daemon of the same instance, and must leave neither
// behind, whatever its assertions did. The two platforms tie the daemon to its anchor differently, so they end it
// differently:
// - Unix: the anchor is started `detached`, so it leads a process group of its own, and every daemon it starts is in
//   that group; one `SIGKILL` to the group (a negative pid) ends them all at once.
// - Windows: there are no process groups to signal, and a negative pid is refused (libuv's kill opens the process by
//   its id). The anchor holds its daemon in a job object that ends every process in it when the job's last handle
//   closes (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, `crates/anchor/src/supervise.rs`), and the anchor alone holds that
//   handle; so ending the anchor's own process, through the handle Node holds for it, ends the daemon with it.
// Until 2026-11-18 every teardown sent the group kill on Windows too, swallowed the refusal, and left the anchor
// running: its process handle kept the suite's process alive, and `node --test` waited on it for over 70 minutes on
// both Windows runners (diag run 44384411115). A teardown now fails its test when the anchor has not ended.

import { spawn } from 'node:child_process';

// Shape: how long a teardown waits for a killed anchor to have ended: the suites' own startup budget (20 s), far past
// a kill's delivery, so only an anchor that outlived its kill fails it.
const END_MS = 20_000;

// Starts `instance`'s anchor from `daemon` (the `slates` binary) on one shard, quick to start, its output dropped. On
// Unix `detached` makes it the leader of its own process group, which `endAnchor` signals whole; on Windows it gives
// the anchor a console of its own, kept hidden.
export function startAnchor(daemon, instance) {
  return spawn(daemon, ['--instance', instance, 'anchor', '--quick', '--shards', '1'], {
    stdio: 'ignore',
    detached: true,
    windowsHide: true,
  });
}

// Whether Node has seen `child` end.
function running(child) {
  return child.exitCode === null && child.signalCode === null;
}

// Resolves true once `child` has ended, false if it has not within `ms`.
function exited(child, ms) {
  if (!running(child)) return Promise.resolve(true);
  return new Promise((resolve) => {
    const timer = setTimeout(() => resolve(false), ms);
    child.once('exit', () => {
      clearTimeout(timer);
      resolve(true);
    });
  });
}

// Ends `anchor` and the daemon it supervises, as the header says, and waits for the anchor to have ended. Throws when
// the kill is refused for any reason but the group being gone already, or when the anchor is still running
// `END_MS` after it: a teardown that left the instance running fails its test rather than hanging the suite.
export async function endAnchor(anchor) {
  if (!running(anchor)) return;
  if (process.platform === 'win32') {
    // TerminateProcess on the anchor's own handle; its kill-on-close job ends the daemon as the anchor's handles close.
    // A refusal (the anchor ending between the check and the kill) is judged by the wait below, as is every outcome.
    anchor.kill('SIGKILL');
  } else {
    try {
      process.kill(-anchor.pid, 'SIGKILL');
    } catch (error) {
      // The group gone already (every member ended between the check and the kill) is what a teardown wants.
      if (error.code !== 'ESRCH') throw error;
    }
  }
  if (!(await exited(anchor, END_MS))) {
    throw new Error(`the anchor (pid ${anchor.pid}) was still running ${END_MS} ms after its kill`);
  }
}
