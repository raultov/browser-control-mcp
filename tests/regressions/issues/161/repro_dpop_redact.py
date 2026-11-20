import sys, time
sys.path.insert(0, 'tests/compare')
from mcp_client import Server
from siteserver import Site

browser = sys.argv[1] if len(sys.argv) > 1 else 'chrome'
site = Site()
s = Server(['target/release/browser-control-mcp', '--headless'] + (['--browser', 'firefox'] if browser == 'firefox' else []), timeout=60)
try:
    s.call('navigate', {'url': site.url('/basic.html')})
    print(s.call('evaluate_js', {'expression': "fetch('/api/echo?dpop=1',{headers:{'DPoP':'eyJhbGciOiJFUzI1NiJ9.qa-dpop-proof-7f3a','X-Qa-Visible':'visible-ctl'}}).then(r=>r.status)"}).short(80))
    time.sleep(1)
    D = s.call('get_network_logs', {'url_filter': 'dpop=1', 'include_details': True}).text
finally:
    s.close(); site.stop()
print([l.strip() for l in D.splitlines() if 'dpop' in l.lower() and 'url' not in l])
print('DPOP_PROOF_SHOWN_BY_DEFAULT' if 'qa-dpop-proof-7f3a' in D else 'DPOP_REDACTED')
