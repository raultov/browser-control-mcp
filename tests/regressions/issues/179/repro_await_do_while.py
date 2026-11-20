import sys, os
sys.path.insert(0, os.path.abspath('tests/compare'))
from mcp_client import Server
from siteserver import Site

site = Site()
s = Server(['target/release/browser-control-mcp', '--headless'], timeout=90)
try:
    s.call('navigate', {'url': site.url('/basic.html')})
    control = s.call('evaluate_js', {'expression': "let x = 0; do while (x < 3) x++; while (false) 'v'"})
    case = s.call('evaluate_js', {'expression': "await 0; let y = 0; do while (y < 3) y++; while (false) 'v'"})
    print('control (no await):', control.text.strip().replace('\n', ' '))
    print('with await        :', case.text.strip().replace('\n', ' '))
    ok_control = '"v"' in control.text
    bug = ok_control and '"v"' not in case.text
    print('BUG: value after do-while lost with top-level await' if bug else 'OK')
finally:
    s.close()
    site.stop()
sys.exit(1 if bug else 0)
