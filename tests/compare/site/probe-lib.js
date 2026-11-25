// Helpers that probe.html (a <script src>) and probe-worker.js
// (importScripts) load from a script of their own, so the browser names
// this file as where their console messages and errors come from (#207);
// used by tests/differential/target_matrix.py.
self.probeLib = {
  log(token) { console.log('probe-lib-log ' + token); },
  throwLater(token) {
    setTimeout(function probeLibThrower() { throw new Error('probe-lib-throw ' + token); });
  },
};
