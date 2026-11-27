// A service worker that does what the messages it gets ask (log, throw,
// fetch, also from an imported script), as probe-worker.js does, answering
// the client that asked, and logs as it starts with `?onload=token`; used
// by tests/differential/target_matrix.py.
importScripts('/probe-lib.js');
const onload = new URL(location.href).searchParams.get('onload');
if (onload) console.log('probe-onload ' + onload);
self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', event => event.waitUntil(self.clients.claim()));
self.addEventListener('message', event => {
  const answer = text => event.source.postMessage(text);
  const [command, token, ...rest] = String(event.data).split(':');
  if (command === 'log') { console.log('probe-log ' + token); answer('done:' + token + ':logged'); }
  else if (command === 'ilog') { probeLib.log(token); answer('done:' + token + ':logged'); }
  else if (command === 'ithrow') { probeLib.throwLater(token); answer('done:' + token + ':thrown'); }
  else if (command === 'throwvalue') { probeLib.throwValue(token); answer('done:' + token + ':thrown'); }
  else if (command === 'burst') { probeLib.burst(token, Number(rest[0])); answer('done:' + token + ':thrown'); }
  else if (command === 'rejectnull') { probeLib.rejectNull(); answer('done:' + token + ':rejected'); }
  else if (command === 'throw') {
    setTimeout(function probeThrower() { throw new Error('probe-throw ' + token); });
    answer('done:' + token + ':thrown');
  } else if (command === 'fetch') {
    event.waitUntil(fetch(rest.join(':')).then(async r => answer('done:' + token + ':' + r.status + ' ' + (await r.text()).slice(0, 40)),
      e => answer('done:' + token + ':failed ' + e)));
  }
});
