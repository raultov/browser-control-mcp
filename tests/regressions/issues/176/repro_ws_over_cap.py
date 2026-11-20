import sys, os, time, re
sys.path.insert(0, os.path.abspath('tests/compare'))
sys.path.insert(0, os.path.abspath('.'))
from mcp_client import Server
from siteserver import Site
import repro_ws_server

# Chrome: 120 same-origin iframes, one WebSocket each (over the 100-connection
# cap), then all removed. Frames whose connection was evicted stay listed with
# an empty URL and no close.
N = 120
PAGE = os.path.join('tests', 'compare', 'site', 'repro_wsf.html')
with open(PAGE, 'w') as f:
    f.write("<!doctype html><title>wsf</title><script>window.socks=[];for(const u of new URLSearchParams(location.search).getAll('u')){const w=new WebSocket(u);w.onopen=()=>{w.send('hi');top.postMessage('open '+u,'*')};socks.push(w)}</script>")
site = Site()
W = repro_ws_server.start()
s = Server(['target/release/browser-control-mcp', '--headless'], timeout=120)
bug = False
try:
    s.call('navigate', {'url': site.url('/basic.html')})
    s.call('evaluate_js', {'expression': "window.opened=0;addEventListener('message',e=>{if(String(e.data).startsWith('open '))opened++});for(let k=0;k<%d;k++){const f=document.createElement('iframe');f.className='q';f.src='/repro_wsf.html?u='+encodeURIComponent('ws://127.0.0.1:%d/echo?k='+k);document.body.append(f)}1" % (N, W)})
    t0 = time.time()
    while time.time() - t0 < 60:
        m = re.search(r'"value": (\d+)', s.call('evaluate_js', {'expression': 'opened'}).text)
        if m and int(m.group(1)) >= N:
            break
        time.sleep(0.5)
    print('opened:', m.group(1) if m else None)
    s.call('evaluate_js', {'expression': "document.querySelectorAll('iframe.q').forEach(f=>f.remove());1"})
    time.sleep(6)
    r = s.call('get_network_logs', {'type_filter': 'websocket'})
    print(r.text.splitlines()[0])
    orphans = [l for l in r.text.splitlines() if re.match(r'^\S+ (sent|received)  \(', l)]
    for l in orphans:
        print('orphan:', l)
    ids = {l.split()[0] for l in orphans}
    unclosed = [i for i in ids if not re.search(r'^%s closed ' % re.escape(i), r.text, re.M)]
    bug = bool(unclosed)
    print('BUG: %d WebSocket(s) listed with frames, an empty URL and no close' % len(unclosed) if bug else 'OK')
finally:
    s.close()
    site.stop()
    os.remove(PAGE)
sys.exit(1 if bug else 0)
