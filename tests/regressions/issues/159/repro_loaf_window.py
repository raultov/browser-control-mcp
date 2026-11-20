import json, os, sys, time
sys.path.insert(0, 'tests/compare')
from mcp_client import Server
from siteserver import Site

page = 'tests/compare/site/qa_loaf_early.html'
open(page, 'w').write("<!doctype html><title>e</title><p>x</p><script>setTimeout(()=>requestAnimationFrame("
                      "()=>{const t=performance.now();while(performance.now()-t<400){}}),100)</script>")
try:
    site = Site()
    s = Server(['target/release/browser-control-mcp', '--headless', '--tool-groups', 'perf'], timeout=120)
    try:
        s.call('navigate', {'url': site.url('/qa_loaf_early.html')})
        time.sleep(4)  # the page is idle from here on
        r = s.call('profile_page_performance', {'duration_ms': 1000, 'insights': True})
    finally:
        s.close()
finally:
    os.remove(page)
text = r.text
def find(o):
    if isinstance(o, dict):
        if 'insights' in o:
            return o['insights']
        for v in o.values():
            f = find(v)
            if f is not None:
                return f
    return None
ins = find(json.loads(text[text.find('{'):]))
print('long_frames:', json.dumps(ins.get('long_frames')))
print('long_scripts:', json.dumps(ins.get('long_scripts')))
old = [f for f in ins.get('long_frames') or [] if f['duration_ms'] >= 300]
if old:
    print('BUG: a long frame from before the profile window is listed')
    sys.exit(1)
