import re, sys, time
sys.path.insert(0, 'tests/compare')
from mcp_client import Server
from siteserver import Site
import repro_ws_server

wsp = repro_ws_server.start()
site = Site()
s = Server(['target/release/browser-control-mcp', '--headless', '--browser', 'firefox'])
try:
    s.call('navigate', {'url': site.url('/basic.html')})
    r = s.call('evaluate_js', {'expression': "const ws=new WebSocket('ws://127.0.0.1:%d/bad');let o=0;ws.onopen=()=>o=1;"
                               "await new Promise(r=>{ws.onclose=r;setTimeout(r,5000)});'opened='+o+' readyState='+ws.readyState" % wsp})
    print('page:', r.short())
    time.sleep(1)
    t = s.call('get_network_logs', {'type_filter': 'websocket'}).text
finally:
    s.close()
print(t)
if not re.search(r'(?m)^\S+ closed ws://127\.0\.0\.1:\d+/bad', t):
    print('BUG: the failed WebSocket is not listed as closed')
    sys.exit(1)
