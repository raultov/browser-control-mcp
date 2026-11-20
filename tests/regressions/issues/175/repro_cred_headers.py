import sys, os, time
sys.path.insert(0, os.path.abspath('tests/compare'))
from mcp_client import Server
from siteserver import Site

site = Site()
s = Server(['target/release/browser-control-mcp', '--headless'], timeout=90)
try:
    s.call('navigate', {'url': site.url('/basic.html')})
    r = s.call('evaluate_js', {'expression': "await fetch('/api/echo?cred=1',{headers:{'X-GitHub-OTP':'otpval-91','X-Vercel-Protection-Bypass':'vbypass-92','X-Honeycomb-Team':'hcteam-93'}}).then(r=>r.status)"})
    print('fetch:', r.text.strip().replace('\n', ' '))
    time.sleep(1)
    r1 = s.call('get_network_logs', {'type_filter': 'rest', 'url_filter': 'cred=1', 'include_details': True})
    leaked = [v for v in ['otpval-91', 'vbypass-92', 'hcteam-93'] if v in r1.text]
    for line in r1.text.splitlines():
        if any(v in line for v in leaked):
            print('default reply shows:', line.strip())
    print('LEAKED' if leaked else 'REDACTED', leaked)
finally:
    s.close()
    site.stop()
sys.exit(1 if leaked else 0)
