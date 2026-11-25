// A dedicated worker that does what the messages it gets ask (log, throw,
// fetch, also from an imported script), as probe.html does, and logs as it
// starts with `?onload=token`; used by tests/differential/target_matrix.py.
importScripts('/probe-lib.js');
const onload = new URL(location.href).searchParams.get('onload');
if (onload) console.log('probe-onload ' + onload);
onmessage = event => {
  const [command, token, ...rest] = String(event.data).split(':');
  if (command === 'log') { console.log('probe-log ' + token); postMessage('done:' + token + ':logged'); }
  else if (command === 'ilog') { probeLib.log(token); postMessage('done:' + token + ':logged'); }
  else if (command === 'ithrow') { probeLib.throwLater(token); postMessage('done:' + token + ':thrown'); }
  else if (command === 'throw') {
    setTimeout(function probeThrower() { throw new Error('probe-throw ' + token); });
    postMessage('done:' + token + ':thrown');
  } else if (command === 'fetch') {
    fetch(rest.join(':')).then(async r => postMessage('done:' + token + ':' + r.status + ' ' + (await r.text()).slice(0, 40)),
      e => postMessage('done:' + token + ':failed ' + e));
  }
};
