import re, sys, time
sys.path.insert(0, 'tests/compare')
from mcp_client import Server
from siteserver import Site
import repro_ws_server

wsp = repro_ws_server.start()
site = Site()
s = Server(['target/release/browser-control-mcp', '--headless'])
try:
    s.call('navigate', {'url': site.url('/basic.html')})
    s.call('evaluate_js', {'expression': "window.qaWs=new WebSocket('ws://127.0.0.1:%d/echo?nav=1');"
                           "await new Promise(r=>qaWs.onopen=r);qaWs.send('ping');await new Promise(r=>qaWs.onmessage=r);'open'" % wsp})
    s.call('navigate', {'url': site.url('/second.html')})
    s.call('navigate', {'history': 'back'})
    time.sleep(2)
    t = s.call('get_network_logs', {'type_filter': 'websocket'}).text
    state = s.call('evaluate_js', {'expression': "window.qaWs ? window.qaWs.readyState : 'none'"}).short()
finally:
    s.close()
print(t)
print('qaWs after back:', state)
if 'echo:ping' in t and not re.search(r'(?m)^\S+ closed ws://127\.0\.0\.1:\d+/echo\?nav=1', t):
    print('BUG: the WebSocket closed by navigation is not listed as closed')
    sys.exit(1)
