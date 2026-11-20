import os, shutil, sys, tempfile, time
sys.path.insert(0, 'tests/compare')
from mcp_client import Server
from siteserver import Site

# --downloads-dir D whose instance folder D/default already exists but is read-only (0555).
D = tempfile.mkdtemp(prefix='qa-dl-rofolder-', dir=os.path.expanduser('~'))
dd = os.path.join(D, 'default')
os.makedirs(dd)
os.chmod(dd, 0o555)

def state(tag):
    if not os.path.exists(dd):
        print(tag, 'D/default: MISSING')
    else:
        print(tag, 'D/default:', oct(os.stat(dd).st_mode & 0o777), os.listdir(dd))

site = Site()
s = Server(['target/release/browser-control-mcp', '--headless', '--browser', 'firefox', '--downloads-dir', D], timeout=90)
try:
    s.call('navigate', {'url': site.url('/basic.html')})
    state('before download:')
    s.call('navigate', {'url': site.url('/download')})
    time.sleep(4)
    state('after 1st download:')
    gone = not os.path.exists(dd)
    s.call('navigate', {'url': site.url('/download')})
    time.sleep(4)
    state('after 2nd download:')
    l = s.call('list_downloads', {})
    print(l.text)
finally:
    s.close()
    site.stop()
    if os.path.exists(dd):
        os.chmod(dd, 0o755)
    shutil.rmtree(D)
if gone or '"completed"' in l.text:
    print('BUG: read-only instance folder was removed/replaced and the download completed')
    sys.exit(1)
print('ok')
