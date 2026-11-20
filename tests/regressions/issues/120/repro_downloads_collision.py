import os, sys, json, tempfile, time
sys.path.insert(0, os.path.abspath('tests/compare'))
from mcp_client import Server
from siteserver import Site

BIN = os.path.abspath('target/release/browser-control-mcp')
# Both labels clean to '______' and have the same FNV-1a 32-bit hash (0xc64b3899).
L1, L2 = '!@^^&!', '!$%*!!'
D = tempfile.mkdtemp()
S = Site()
srv = Server([BIN, '--headless', '--downloads-dir', D])
ids = []
for label in (L1, L2):
    r = srv.call('open_instance', {'label': label})
    ids.append(json.loads(r.text)['instance_id'])
for iid in ids:
    srv.call('navigate', {'instance_id': iid, 'url': S.url('/download')})
    time.sleep(3)
srv.close()
S.stop()
folders = sorted(os.listdir(D))
files = sorted(os.path.relpath(os.path.join(p, f), D) for p, _, fs in os.walk(D) for f in fs)
print('folders', folders)
print('files', files)
if len(folders) < 2:
    print('BUG: two instances share one downloads folder')
    sys.exit(1)
print('OK')
