// A dedicated worker that opens the WebSocket of its `u` parameter; used by
// tests/differential/ws_matrix.py.
const sock = new WebSocket(new URLSearchParams(location.search).get('u'));
sock.onopen = () => { sock.send('hi'); postMessage('open'); };
onmessage = event => {
  const text = String(event.data);
  if (text === 'close') sock.close();
  else if (text.startsWith('send:')) sock.send(text.slice(5));
};
