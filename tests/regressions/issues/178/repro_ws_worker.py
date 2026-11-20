import sys, os, time, re
sys.path.insert(0, os.path.abspath('tests/compare'))
sys.path.insert(0, os.path.abspath('.'))
from mcp_client import Server
from siteserver import Site
import repro_ws_server

JS = os.path.join('tests', 'compare', 'site', 'repro_wsworker.js')
with open(JS, 'w') as f:
    f.write("const w=new WebSocket(new URLSearchParams(location.search).get('u'));w.onopen=()=>{w.send('hi');postMessage('open')};")
site = Site()
W = repro_ws_server.start()
s = Server(['target/release/browser-control-mcp', '--headless', '--browser', 'firefox'], timeout=90)
bug = False
try:
    s.call('navigate', {'url': site.url('/basic.html')})
    r = s.call('evaluate_js', {'expression': "window.wk=new Worker('/repro_wsworker.js?u='+encodeURIComponent('ws://127.0.0.1:%d/echo?k=worker')); await Promise.race([new Promise(r=>wk.onmessage=()=>r('open')), new Promise(r=>setTimeout(()=>r('timeout'),10000))])" % W})
    print('worker:', r.text.strip().replace('\n', ' '))
    time.sleep(1)
    r1 = s.call('get_network_logs', {'type_filter': 'websocket'})
    s.call('evaluate_js', {'expression': 'wk.terminate();1'})
    time.sleep(3)
    r2 = s.call('get_network_logs', {'type_filter': 'websocket'})
    print('--- before terminate'); print(r1.text)
    print('--- after terminate'); print(r2.text)
    print('server saw:', repro_ws_server.LOG)
    listed = re.search(r'^\S+ sent ws://127\.0\.0\.1:%d/echo\?k=worker' % W, r1.text, re.M)
    closed = re.search(r'^\S+ (closed|failed) ws://127\.0\.0\.1:%d/echo\?k=worker' % W, r2.text, re.M)
    server_closed = ('tcpclose', '/echo?k=worker') in repro_ws_server.LOG
    bug = bool(listed) and server_closed and not closed
    print('BUG: terminated worker socket still looks open' if bug else 'OK')
finally:
    s.close()
    site.stop()
    os.remove(JS)
sys.exit(1 if bug else 0)
