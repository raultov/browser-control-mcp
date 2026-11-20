"""save_pdf writes outside --upload-root when a parent directory inside the
root is swapped for a symlink to an outside directory during the call."""
import os, shutil, sys, tempfile, threading, time
sys.path.insert(0, 'tests/compare')
from mcp_client import Server
from siteserver import Site

BIN = 'target/release/browser-control-mcp'
R = os.path.realpath(tempfile.mkdtemp(prefix='qa-race.', dir='.'))
O = tempfile.mkdtemp(prefix='qa-raceout.')
os.mkdir(R + '/sub'); os.mkdir(O + '/d')
stop = threading.Event()


def flipper():
    while not stop.is_set():
        try:
            os.rename(R + '/sub', R + '/sub.real')
            os.symlink(O + '/d', R + '/sub')
            time.sleep(0.001)
            os.unlink(R + '/sub')
            os.rename(R + '/sub.real', R + '/sub')
            time.sleep(0.001)
        except (FileNotFoundError, FileExistsError):
            pass

escaped = []
try:
    S = Site()
    srv = Server([BIN, '--headless', '--upload-root', R, '--tool-groups', 'files'])
    srv.call('navigate', {'url': S.url('/basic.html')}, timeout=60)
    th = threading.Thread(target=flipper, daemon=True); th.start()
    for i in range(300):
        r = srv.call('save_pdf', {'path': R + '/sub/race.pdf'}, timeout=30)
        escaped = sorted(os.listdir(O + '/d'))
        if escaped:
            print('call', i, 'is_error', r.is_error, r.text.strip()[:120])
            break
    stop.set(); th.join()
    srv.close()
finally:
    stop.set()
    shutil.rmtree(R, ignore_errors=True); shutil.rmtree(O, ignore_errors=True)
if escaped:
    print('ESCAPED: files written outside the upload root:', escaped)
    sys.exit(1)
print('OK: nothing written outside the root')
