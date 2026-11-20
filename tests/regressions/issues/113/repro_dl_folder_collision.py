import sys, os, json, time, tempfile, shutil
sys.path.insert(0, 'tests/compare')
from mcp_client import Server
from siteserver import Site
D = tempfile.mkdtemp(prefix='qa-dl.', dir=os.path.expanduser('~'))
S = Site()
srv = Server(['target/release/browser-control-mcp', '--headless', '--downloads-dir', D])
dirs = {}
for L in ('a/b', 'a_b'):
    print('open_instance', L, srv.call('open_instance', {'label': L, 'browser': 'chrome', 'headless': True}, timeout=60).text.strip())
for L in ('a/b', 'a_b'):
    srv.call('open_tab', {'instance_id': L, 'url': S.url('/basic.html')}, timeout=60)
    srv.call('navigate', {'instance_id': L, 'url': S.url('/download')}, timeout=60)
    time.sleep(2)
    ld = json.loads(srv.call('list_downloads', {'instance_id': L}).text)
    dirs[L] = ld['directory']
    print(L, 'directory:', ld['directory'], 'files:', [d['path'] for d in ld['downloads']])
for L in ('a/b', 'a_b'):
    srv.call('close_instance', {'instance_id': L})
srv.close(); S.stop(); shutil.rmtree(D, ignore_errors=True)
same = dirs['a/b'] == dirs['a_b']
print('instances a/b and a_b share one downloads directory:', same)
sys.exit(1 if same else 0)
