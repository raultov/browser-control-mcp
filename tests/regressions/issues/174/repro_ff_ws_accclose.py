import sys, os, time, re, json
sys.path.insert(0, os.path.abspath('tests/compare'))
sys.path.insert(0, os.path.abspath('.'))
from mcp_client import Server
from siteserver import Site
import repro_ws_server

# Firefox: a live socket, a refused one, one accepted with 101 then dropped,
# one dropped without answer, and a second live socket, opened together.
bug = False
for attempt in range(1, 4):
    site = Site()
    W = repro_ws_server.start()
    s = Server(['target/release/browser-control-mcp', '--headless', '--browser', 'firefox'], timeout=90)
    try:
        s.call('navigate', {'url': site.url('/basic.html')})
        base = 'ws://127.0.0.1:%d' % W
        urls = [base + '/echo?r=1&a=1', base + '/deny?r=1', base + '/acc-close?r=1', base + '/drop?r=1', base + '/echo?r=1&a=2']
        s.call('evaluate_js', {'expression': 'window.ss=[];' + ''.join('ss.push(new WebSocket(%s));' % json.dumps(u) for u in urls) + '1'})
        for _ in range(60):
            st = s.call('evaluate_js', {'expression': "ss.map(s=>s.readyState).join(',')"}).text
            if '0' not in st.split('value')[-1]:
                break
            time.sleep(0.5)
        time.sleep(3)
        r = s.call('get_network_logs', {'type_filter': 'websocket'})
        states = s.call('evaluate_js', {'expression': "ss.map(s=>s.url.split('/').pop()+'='+s.readyState).join(' ')"}).text
        print('attempt', attempt, 'page readyStates:', re.sub(r'\s+', ' ', states))
        print(r.text)
        live_closed = [l for l in r.text.splitlines() if re.match(r'^\S+ (closed|failed) %s/echo\?' % re.escape(base), l)]
        open_live = 'echo?r=1&a=1=1' in states and 'echo?r=1&a=2=1' in states
        if live_closed and open_live:
            print('BUG: live (readyState 1) socket listed closed:', live_closed)
            bug = True
    finally:
        s.close()
        site.stop()
    if bug:
        break
sys.exit(1 if bug else 0)
