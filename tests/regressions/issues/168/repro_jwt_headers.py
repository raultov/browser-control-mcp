import sys, time
sys.path.insert(0, 'tests/compare')
from mcp_client import Server
from siteserver import Site

site = Site()
s = Server(['target/release/browser-control-mcp', '--headless'], timeout=90)
s.call('navigate', {'url': site.url('/basic.html')})
s.call('evaluate_js', {'expression': "await fetch('/api/echo?jwt=1',{headers:{'X-Appwrite-JWT':'jwt-appwrite-81','X-Firebase-AppCheck':'appcheck-83','X-Trace-Hint':'visible-86'}}).then(r=>r.status)"})
time.sleep(1)
A = s.call('get_network_logs', {'url_filter': 'jwt=1', 'include_details': True})
s.close(); site.stop()
leaks = [v for v in ('jwt-appwrite-81', 'appcheck-83') if v in A.text]
print('control visible:', 'visible-86' in A.text)
for line in A.text.splitlines():
    if 'Appwrite' in line or 'AppCheck' in line:
        print(line.strip())
print('LEAKED:', leaks)
sys.exit(1 if leaks else 0)
