// Helpers that probe.html (a <script src>) and probe-worker.js
// (importScripts) load from a script of their own, so the browser names
// this file as where their console messages and errors come from (#207);
// used by tests/differential/target_matrix.py.
self.probeLib = {
  log(token) { console.log('probe-lib-log ' + token); },
  throwLater(token) {
    setTimeout(function probeLibThrower() { throw new Error('probe-lib-throw ' + token); });
  },
  // A thrown value that is not an error (#219).
  throwValue(token) {
    setTimeout(function probeValueThrower() { throw 'probe-value ' + token; });
  },
  // The same error `count` times at once: several in one millisecond (#218).
  burst(token, count) {
    for (let k = 0; k < count; k++) {
      setTimeout(function probeBurstThrower() { throw new Error('probe-burst ' + token); });
    }
  },
};
