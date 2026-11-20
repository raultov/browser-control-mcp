import sys, os, time, json, re
sys.path.insert(0, os.path.abspath('tests/compare'))
sys.path.insert(0, os.path.abspath('.'))
from urllib.parse import quote
from mcp_client import Server
from siteserver import Site
import repro_ws_server

PAGE = os.path.join('tests', 'compare', 'site', 'repro_wsf.html')
with open(PAGE, 'w') as f:
    f.write("<!doctype html><title>wsf</title><script>window.socks=[];for(const u of new URLSearchParams(location.search).getAll('u')){const w=new WebSocket(u);w.onopen=()=>{w.send('hi');top.postMessage('open '+u,'*')};socks.push(w)}</script>")
site = Site()
W = repro_ws_server.start()
s = Server(['target/release/browser-control-mcp', '--headless'], timeout=90)
bug = False
try:
    s.call('navigate', {'url': site.url('/basic.html')})
    s.call('evaluate_js', {'expression': "window.opened=[];addEventListener('message',e=>opened.push(e.data));1"})
    src = site.other_origin('/repro_wsf.html?u=' + quote('ws://127.0.0.1:%d/echo?k=hop' % W, safe=''))
    s.call('evaluate_js', {'expression': "const f=document.createElement('iframe');f.id='hop';f.src=%s;document.body.append(f);1" % json.dumps(src)})
    for _ in range(40):
        if re.search(r'"value": 1\b', s.call('evaluate_js', {'expression': 'opened.length'}).text):
            break
        time.sleep(0.5)
    # The cross-origin frame navigates within its own origin.
    s.call('evaluate_js', {'expression': "document.getElementById('hop').src=%s;1" % json.dumps(site.other_origin('/basic.html'))})
    time.sleep(5)
    r = s.call('get_network_logs', {'type_filter': 'websocket'})
    print(r.text)
    print('server saw:', repro_ws_server.LOG)
    closed = re.search(r'^\S+ (closed|failed) ws://127\.0\.0\.1:%d/echo\?k=hop' % W, r.text, re.M)
    server_closed = ('tcpclose', '/echo?k=hop') in repro_ws_server.LOG
    bug = server_closed and not closed
    print('BUG: socket closed by the browser but not listed closed' if bug else 'OK')
finally:
    s.close()
    site.stop()
    os.remove(PAGE)
sys.exit(1 if bug else 0)
