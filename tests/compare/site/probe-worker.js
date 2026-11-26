// A dedicated worker that does what the messages it gets ask (log, throw,
// fetch, also from an imported script), as probe.html does, and logs as it
// starts with `?onload=token`; with `nested=1` a worker it starts does it
// all instead (#217) and this one passes the messages on; used by
// tests/differential/target_matrix.py.
importScripts('/probe-lib.js');
const query = new URL(location.href).searchParams;
const onload = query.get('onload');
const inner = query.get('nested') && new Worker('/probe-worker.js?onload=' + encodeURIComponent(onload || ''));
if (inner) inner.onmessage = event => postMessage(event.data);
if (onload && !inner) console.log('probe-onload ' + onload);
onmessage = event => {
  if (inner) return inner.postMessage(event.data);
  const [command, token, ...rest] = String(event.data).split(':');
  if (command === 'log') { console.log('probe-log ' + token); postMessage('done:' + token + ':logged'); }
  else if (command === 'ilog') { probeLib.log(token); postMessage('done:' + token + ':logged'); }
  else if (command === 'ithrow') { probeLib.throwLater(token); postMessage('done:' + token + ':thrown'); }
  else if (command === 'throwvalue') { probeLib.throwValue(token); postMessage('done:' + token + ':thrown'); }
  else if (command === 'burst') { probeLib.burst(token, Number(rest[0])); postMessage('done:' + token + ':thrown'); }
  else if (command === 'throw') {
    setTimeout(function probeThrower() { throw new Error('probe-throw ' + token); });
    postMessage('done:' + token + ':thrown');
  } else if (command === 'fetch') {
    fetch(rest.join(':')).then(async r => postMessage('done:' + token + ':' + r.status + ' ' + (await r.text()).slice(0, 40)),
      e => postMessage('done:' + token + ':failed ' + e));
  }
};
