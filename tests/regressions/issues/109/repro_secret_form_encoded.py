import sys, os, re, json, tempfile
sys.path.insert(0, 'tests/compare')
from mcp_client import Server
from siteserver import Site
env = tempfile.NamedTemporaryFile('w', suffix='.env', delete=False)
env.write('PW=p@ss w&rd9\n'); env.close()
S = Site()
srv = Server(['target/release/browser-control-mcp', '--headless', '--secrets', env.name])
srv.call('navigate', {'url': S.url('/login.html')}, timeout=60)
srv.call('evaluate_js', {'expression': "const f=document.getElementById('login'); f.onsubmit=null; f.action='/status/200'; f.method='get'; 'ok'"})
srv.call('fill_input', {'selector': '#pass', 'text': '<secret:PW>'})
srv.call('click_element', {'selector': 'button[type=submit]'}, timeout=60)
srv.call('wait_for', {'url': '/status/200'}, timeout=60)
rx = re.compile(r'p(%40|@)ss(\+|%20| )w(%26|&)rd9', re.I)
leaks = []
for name, args in [('list_tabs', {}), ('evaluate_js', {'expression': 'location.search'}), ('get_network_logs', {}), ('snapshot', {})]:
    r = srv.call(name, args, timeout=60)
    m = rx.search(r.text + json.dumps(r.structured))
    if m: leaks.append(name + ' shows ' + m.group(0))
srv.close(); S.stop(); os.unlink(env.name)
print('secret value leaked in replies:', leaks)
sys.exit(1 if leaks else 0)
