import sys, subprocess
sys.path.insert(0, 'tests/compare')
from mcp_client import Server
srv = Server(['target/release/browser-control-mcp', '--headless', '--proxy-server', '127.0.0.1:18790'])
n = srv.call('navigate', {'url': 'http://proxied.test/x'}, timeout=60)
out = subprocess.run(['ps', '-o', 'args=', '-s', str(srv.proc.pid)], capture_output=True, text=True).stdout
flags = sorted({a for line in out.splitlines() for a in line.split() if a.startswith('--proxy-server=')})
srv.close()
print('navigate:', 'ERROR' if n.is_error else 'ok', n.text.strip()[:200])
print('chrome launched with:', flags)
bad = any('unparsable' in f for f in flags)
sys.exit(1 if bad else 0)
