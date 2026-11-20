"""--downloads-dir: an instance label whose escaped folder name exceeds 255 bytes
('A'*90 -> 270 bytes) is accepted, but its download never lands under the dir.
Needs: cargo build --release --all-features; Chrome installed. Run from the repo root."""
import os, sys, time, tempfile
sys.path.insert(0, os.path.abspath('tests/compare'))
from mcp_client import Server
from siteserver import Site

BIN = os.path.abspath('target/release/browser-control-mcp')
D = tempfile.mkdtemp(dir=os.path.expanduser('~'), prefix='qa-dl-long-')
DL = os.path.expanduser('~/Downloads')
before = set(os.listdir(DL)) if os.path.isdir(DL) else set()
label = 'A' * 90
S = Site()
srv = Server([BIN, '--headless', '--downloads-dir', D])
try:
    o = srv.call('open_instance', {'label': label}, timeout=90)
    print('OPEN_INSTANCE is_error', o.is_error, o.short(200))
    if o.is_error:
        print('REFUSED (acceptable)')
        sys.exit(0)
    n = srv.call('navigate', {'instance_id': label, 'url': S.url('/download')}, timeout=60)
    print('NAVIGATE', n.short(200))
    time.sleep(5)
    srv.call('close_instance', {'instance_id': label})
finally:
    srv.close(); S.stop()
under = [os.path.join(r, f) for r, _, fs in os.walk(D) for f in fs]
after = set(os.listdir(DL)) if os.path.isdir(DL) else set()
print('FILES UNDER downloads-dir', under)
print('NEW FILES IN ~/Downloads', sorted(after - before))
if not any(p.endswith('data.csv') for p in under):
    print('DOWNLOAD_NOT_IN_DOWNLOADS_DIR')
    sys.exit(1)
