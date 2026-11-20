import sys, json, re
sys.path.insert(0, 'tests/compare')
from mcp_client import Server
from siteserver import Site

site = Site()
s = Server(['target/release/browser-control-mcp', '--headless'], timeout=90)
s.call('navigate', {'url': site.url('/basic.html')})
cases = [
    ('const o = await Promise.resolve({do: 4041})\nconst v = o.do\nv * 2', 8082),
    ('const o = await Promise.resolve({new: 5051})\nconst v = o.new\nv + 1', 5052),
    ('const o = await Promise.resolve({in: 6061})\nlet w = o.in\nw - 1', 6060),
    ("await 0; do { } while (false) 'dw-ok'", 'dw-ok'),
]
bad = 0
for e, want in cases:
    r = s.call('evaluate_js', {'expression': e})
    m = re.search(r'\{.*\}', r.text, re.S)
    got = json.loads(m.group(0)).get('value') if m else None
    ok = got == want
    bad += not ok
    print('OK ' if ok else 'BAD', repr(e), 'want', want, 'got', got, '|', r.short(200))
s.close(); site.stop()
print('FAILED CASES:', bad)
sys.exit(1 if bad else 0)
