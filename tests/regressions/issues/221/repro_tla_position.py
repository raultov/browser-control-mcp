import re, sys
sys.path.insert(0, 'tests/compare')
from mcp_client import Server
from siteserver import Site

# (browser, code, expected 1-based position of the undefined name in the code)
CASES = [
    ('chrome', 'await 0;\n  nope_c;\n1', (2, 3)),
    ('firefox', 'await 0; nope_a', (1, 10)),
]
site = Site()
bad = 0
for browser, code, expected in CASES:
    cmd = ['target/release/browser-control-mcp', '--headless']
    if browser == 'firefox':
        cmd += ['--browser', 'firefox']
    s = Server(cmd)
    s.call('navigate', {'url': site.url('/basic.html')})
    r = s.call('evaluate_js', {'expression': code})
    s.close()
    m = re.search(r'\(line (\d+), column (\d+)\)', r.text)
    got = (int(m[1]), int(m[2])) if m else None
    print(f'{browser}: {code!r} -> {r.text.strip()!r}; expected position {expected}')
    if got != expected:
        bad += 1
        print(f'WRONG POSITION on {browser}: got {got}, expected {expected}')
site.stop()
sys.exit(1 if bad else 0)
