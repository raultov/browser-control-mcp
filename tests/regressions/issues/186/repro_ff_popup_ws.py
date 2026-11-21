import json, os, re, sys, time
sys.path[:0] = [os.path.join(os.getcwd(), 'tests', 'compare'), os.path.join(os.getcwd(), 'tests', 'differential')]
from mcp_client import Server
from siteserver import Site
import ws_server

W = ws_server.start()
site = Site()
s = Server(['target/release/browser-control-mcp', '--browser', 'firefox', '--headless'], timeout=120)
s.call('navigate', {'url': site.url('/basic.html')})
opener = json.loads(s.call('list_tabs', {}).text)['tabs'][0]['tab_id']
known = {opener}


def new_popup():
    for t in json.loads(s.call('list_tabs', {}).text)['tabs']:
        if 'wsowner' in t['url'] and t['tab_id'] not in known:
            known.add(t['tab_id'])
            return t['tab_id']


def closes(tab, cell):
    pat = re.compile(r'^\S+ (closed|failed) ws://127\.0\.0\.1:%d/[a-z-]+\?cell=%s(:.*)?$' % (W, cell), re.M)
    for _ in range(16):
        text = s.call('get_network_logs', {'tab_id': tab, 'type_filter': 'websocket'}).text
        if pat.search(text):
            break
        time.sleep(0.5)
    time.sleep(1)
    text = s.call('get_network_logs', {'tab_id': tab, 'type_filter': 'websocket'}).text
    return len(pat.findall(text)), text.strip()


WAIT = "await new Promise(r => addEventListener('message', e => /^%s/.test(e.data) && r(1)));"
# F: a popup whose WebSocket handshake is refused (403).
s.call('evaluate_js', {'tab_id': opener, 'user_gesture': True, 'expression':
       "window.pop = window.open('/wsowner.html?u=' + encodeURIComponent('ws://127.0.0.1:%d/deny?cell=pop-refused')); %s 1"
       % (W, WAIT % '(open|closed)')})
p = new_popup()
nf, tf = closes(p, 'pop-refused')
print('F (refused handshake in a popup): %d close line(s); log: %r' % (nf, tf))
s.call('close_tab', {'tab_id': p})
# N: a popup opened by a popup; its socket is closed by the server.
s.call('evaluate_js', {'tab_id': opener, 'user_gesture': True, 'expression':
       "window.pop = window.open('/wsowner.html?u=' + encodeURIComponent('ws://127.0.0.1:%d/echo?cell=pop-parent')); %s 1"
       % (W, WAIT % '(open|closed)')})
p1 = new_popup()
s.call('evaluate_js', {'tab_id': p1, 'user_gesture': True, 'expression':
       "window.pop2 = window.open('/wsowner.html?u=' + encodeURIComponent('ws://127.0.0.1:%d/echo?cell=pop-nested')); %s "
       "pop2.postMessage('send:bye', '*'); 1" % (W, WAIT % 'open')})
p2 = new_popup()
nn, tn = closes(p2, 'pop-nested')
print('N (popup of a popup, server close): %d close line(s); log: %r' % (nn, tn))
s.close()
if nf != 1 or nn != 1:
    print('BUG: expected exactly 1 closed/failed line in each cell')
    sys.exit(1)
