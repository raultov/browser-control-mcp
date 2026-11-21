import json, os, re, sys, time
sys.path[:0] = [os.path.join(os.getcwd(), 'tests', 'compare'), os.path.join(os.getcwd(), 'tests', 'differential')]
from mcp_client import Server
from siteserver import Site
import ws_server

W = ws_server.start()
site = Site()
s = Server(['target/release/browser-control-mcp', '--browser', 'firefox', '--headless'], timeout=120)
s.call('navigate', {'url': site.url('/basic.html')})
# The popup opens a WebSocket and sends 'hi' at once; the echo server answers 'hi'.
s.call('evaluate_js', {'user_gesture': True, 'expression':
       "window.p1 = window.open('/wsowner.html?u=' + encodeURIComponent('ws://127.0.0.1:%d/echo?pop=1'), 'w1'); "
       "await new Promise(r => addEventListener('message', e => /^open/.test(e.data) && r(1))); 1" % W})
time.sleep(2)
tab = [t['tab_id'] for t in json.loads(s.call('list_tabs', {}).text)['tabs'] if 'wsowner' in t['url']][0]
s.call('evaluate_js', {'expression': "p1.postMessage('send:later', '*'); 1"})
time.sleep(2)
log = s.call('get_network_logs', {'tab_id': tab, 'type_filter': 'websocket'}).text
s.close()
print(log.strip())
hi = len(re.findall(r'^\S+ (?:sent|received) ws://127\.0\.0\.1:%d/echo\?pop=1 \(2 bytes\): hi$' % W, log, re.M))
print('hi frames listed: %d (expected 2)' % hi)
if hi != 2:
    print('BUG: the popup\'s first WebSocket frames are missing')
    sys.exit(1)
