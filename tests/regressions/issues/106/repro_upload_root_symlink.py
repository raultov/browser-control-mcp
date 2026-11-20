import sys, os, time, tempfile, shutil
# Run from the repository root.
REPO = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(REPO, 'tests/compare'))
os.chdir(os.path.join(REPO, 'tests/compare'))
from mcp_client import Server
from siteserver import Site

browsers = sys.argv[1:] or ['chrome', 'firefox']
R = tempfile.mkdtemp(prefix='qa-root.', dir=REPO)
O = tempfile.mkdtemp(prefix='qa-out.', dir='/tmp')
for ext in ['pdf', 'mp4', 'json']:
    os.symlink(f'{O}/escaped.{ext}', f'{R}/a.{ext}')
site = Site()
bad = []
for name in browsers:
    B = [] if name == 'chrome' else ['--browser', 'firefox']
    s = Server(['../../target/release/browser-control-mcp', '--headless', '--tool-groups', 'files,perf', '--upload-root', R] + B)
    s.call('navigate', {'url': site.url('/basic.html')})
    p = s.call('save_pdf', {'path': R + '/a.pdf'})
    print(name, 'a save_pdf', p.is_error, p.short(200), flush=True)
    if not p.is_error: bad.append(f'{name}:save_pdf-not-error')
    if name == 'chrome':
        st = s.call('record_video', {'action': 'start'})
        print(name, 'b start', st.is_error, st.short(150))
        time.sleep(1)
        v = s.call('record_video', {'action': 'stop', 'path': R + '/a.mp4'})
        print(name, 'b stop', v.is_error, v.short(200), flush=True)
        if not v.is_error: bad.append(f'{name}:record_video-not-error')
        t = s.call('profile_page_performance', {'duration_ms': 1000, 'save_path': R + '/a.json'})
        print(name, 'c profile', t.is_error, t.short(200), flush=True)
        if not t.is_error: bad.append(f'{name}:profile-not-error')
    c = s.call('save_pdf', {'path': R + '/ok.pdf'})
    print(name, 'd control', c.is_error, c.short(200), flush=True)
    ok = os.path.exists(R + '/ok.pdf') and open(R + '/ok.pdf', 'rb').read(4) == b'%PDF'
    print(name, 'ok.pdf valid', ok)
    if c.is_error or not ok: bad.append(f'{name}:control')
    try: os.remove(R + '/ok.pdf')
    except FileNotFoundError: pass
    s.close()
esc = sorted(os.listdir(O))
print('ls O:', esc)
os.system(f'ls -l "{R}"')
if esc: bad.append('escaped:' + ','.join(esc))
shutil.rmtree(R); shutil.rmtree(O)
print('BAD', bad)
sys.exit(1 if bad else 0)
